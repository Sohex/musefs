"""Full end-to-end: generate audio -> `beet import` -> retag in beets ->
`beet musefs` (auto-scan + sync) -> real FUSE mount -> verify the mount shows
beets' tags and serves byte-identical audio. Opt-in (marker `e2e`): needs
ffmpeg, the built `musefs` binary, `/dev/fuse` + fusermount, and beets.

Run with: `python -m pytest -m e2e`
"""

import os
import shutil
import subprocess
import sys
import time
from contextlib import contextmanager
from pathlib import Path

import pytest

pytest.importorskip("beets")
import mutagen  # noqa: E402

pytestmark = pytest.mark.e2e

REPO_ROOT = Path(__file__).resolve().parents[3]
BEETSPLUG_DIR = Path(__file__).resolve().parents[1] / "beetsplug"
_DEBUG = REPO_ROOT / "target" / "debug" / "musefs"
_RELEASE = REPO_ROOT / "target" / "release" / "musefs"
MUSEFS = str(_DEBUG if _DEBUG.exists() else _RELEASE)
BEET = os.path.join(os.path.dirname(sys.executable), "beet")


@pytest.fixture(autouse=True)
def _require_tools():
    if not (_DEBUG.exists() or _RELEASE.exists()):
        pytest.skip(f"musefs binary not built (looked in {_DEBUG}, {_RELEASE})")
    if not (os.path.exists("/dev/fuse") and shutil.which("fusermount")):
        pytest.skip("no /dev/fuse or fusermount")
    if not shutil.which("ffmpeg"):
        pytest.skip("ffmpeg not available")
    if not os.path.exists(BEET):
        pytest.skip(f"beet not found at {BEET}")


# --- helpers ---------------------------------------------------------------

def _ffmpeg_gen(path, freq, **tags):
    cmd = ["ffmpeg", "-hide_banner", "-loglevel", "error", "-y",
           "-f", "lavfi", "-i", f"sine=frequency={freq}:duration=1"]
    if str(path).endswith(".mp3"):
        cmd += ["-c:a", "libmp3lame", "-q:a", "5"]
    elif str(path).endswith(".m4a"):
        cmd += ["-c:a", "aac", "-b:a", "64k"]
    for key, value in tags.items():
        cmd += ["-metadata", f"{key}={value}"]
    cmd.append(str(path))
    subprocess.run(cmd, check=True, capture_output=True)


def _env(tmp_path):
    env = dict(os.environ)
    env["BEETSDIR"] = str(tmp_path)  # isolate from any real beets config
    return env


def _write_config(tmp_path, library, db):
    cfg = tmp_path / "config.yaml"
    cfg.write_text(
        f"directory: {library}\n"
        f"library: {tmp_path / 'beets_lib.db'}\n"
        f"pluginpath: {BEETSPLUG_DIR}\n"
        f"plugins: musefs\n"
        f"musefs:\n"
        f"  db: {db}\n"
        f"  bin: {MUSEFS}\n"
        f"  autoscan: yes\n"
        f"import:\n"
        f"  copy: yes\n"
        f"  write: no\n"
    )
    return cfg


def _beet(cfg, env, *args):
    result = subprocess.run([BEET, "-c", str(cfg), *args], capture_output=True, env=env)
    if result.returncode != 0:
        raise AssertionError(
            f"`beet {' '.join(args)}` failed ({result.returncode}):\n"
            f"stdout: {result.stdout.decode(errors='replace')}\n"
            f"stderr: {result.stderr.decode(errors='replace')}"
        )
    return result.stdout.decode(errors="replace")


def _audio_md5(path):
    """MD5 of the decoded audio stream (proves byte-faithful audio independent
    of container/metadata framing)."""
    out = subprocess.run(
        ["ffmpeg", "-hide_banner", "-loglevel", "error", "-i", str(path),
         "-map", "0:a", "-f", "md5", "-"],
        check=True, capture_output=True,
    ).stdout.decode()
    return out.strip()


@contextmanager
def _mounted(mnt, db, template):
    proc = subprocess.Popen(
        [MUSEFS, "mount", str(mnt), "--db", str(db), "--template", template],
        stdout=subprocess.PIPE, stderr=subprocess.PIPE,
    )
    try:
        deadline = time.time() + 10
        while time.time() < deadline:
            if os.path.ismount(str(mnt)):
                break
            if proc.poll() is not None:
                raise AssertionError(
                    "musefs mount exited early: "
                    + proc.stderr.read().decode(errors="replace")
                )
            time.sleep(0.05)
        else:
            raise AssertionError("musefs mount did not come up within 10s")
        yield
    finally:
        subprocess.run(["fusermount", "-u", str(mnt)], capture_output=True)
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()


def _imported_library(tmp_path):
    """Generate a FLAC + MP3, import them into a fresh beets library (as-is).
    Returns (cfg, env, db, mnt, library)."""
    src = tmp_path / "src"
    library = tmp_path / "library"
    mnt = tmp_path / "mnt"
    for d in (src, library, mnt):
        d.mkdir()
    db = tmp_path / "musefs.db"
    cfg = _write_config(tmp_path, library, db)
    env = _env(tmp_path)

    _ffmpeg_gen(src / "a.flac", 440, title="Orig FLAC", artist="Orig",
                album="Orig Album", album_artist="Test AA")
    _ffmpeg_gen(src / "b.mp3", 330, title="Orig MP3", artist="Orig",
                album="Orig Album", album_artist="Test AA")
    _ffmpeg_gen(src / "c.m4a", 550, title="Orig M4A", artist="Orig",
                album="Orig Album", album_artist="Test AA")
    _beet(cfg, env, "import", "-A", "-q", str(src))
    return cfg, env, db, mnt, library


# --- tests -----------------------------------------------------------------

def test_e2e_import_retag_mount_playback(tmp_path):
    cfg, env, db, mnt, library = _imported_library(tmp_path)

    # Retag in the beets DB only (no file write, no move) so the divergence is
    # real: files keep their original embedded tags; the mount must show beets'.
    _beet(cfg, env, "modify", "-W", "-M", "-y", "format:FLAC",
          "title=New FLAC", "artist=New Artist", "albumartist=AA", "album=New Album")
    _beet(cfg, env, "modify", "-W", "-M", "-y", "format:MP3",
          "title=New MP3", "artist=New Artist", "albumartist=AA", "album=New Album")
    _beet(cfg, env, "modify", "-W", "-M", "-y", "format:AAC",
          "title=New M4A", "artist=New Artist", "albumartist=AA", "album=New Album")

    # Backing paths (modify -M kept them put) for the audio-integrity check.
    flac_backing = _beet(cfg, env, "ls", "-p", "format:FLAC").strip()
    mp3_backing = _beet(cfg, env, "ls", "-p", "format:MP3").strip()
    m4a_backing = _beet(cfg, env, "ls", "-p", "format:AAC").strip()

    _beet(cfg, env, "musefs")  # auto-scan + sync the changed tags

    with _mounted(mnt, db, "$albumartist/$album/$title"):
        flac = mnt / "AA" / "New Album" / "New FLAC.flac"
        mp3 = mnt / "AA" / "New Album" / "New MP3.mp3"
        m4a = mnt / "AA" / "New Album" / "New M4A.m4a"
        assert flac.exists(), sorted(p.name for p in mnt.rglob("*"))
        assert mp3.exists()
        assert m4a.exists()

        ft = mutagen.File(str(flac), easy=True)
        assert ft["title"] == ["New FLAC"]
        assert ft["artist"] == ["New Artist"]
        assert ft["albumartist"] == ["AA"]
        assert ft["album"] == ["New Album"]

        mt = mutagen.File(str(mp3), easy=True)
        assert mt["title"] == ["New MP3"]
        assert mt["album"] == ["New Album"]

        at = mutagen.File(str(m4a), easy=True)
        assert at["title"] == ["New M4A"]
        assert at["album"] == ["New Album"]

        # Audio served byte-faithfully: decoded PCM identical to the backing file.
        assert _audio_md5(flac) == _audio_md5(flac_backing)
        assert _audio_md5(mp3) == _audio_md5(mp3_backing)
        assert _audio_md5(m4a) == _audio_md5(m4a_backing)


def test_e2e_move_reconcile(tmp_path):
    cfg, env, db, mnt, library = _imported_library(tmp_path)
    _beet(cfg, env, "musefs")  # initial sync (original tags)

    # A write-back modify renames/moves the FLAC. The plugin's cli_exit reconcile
    # must scan the new path and prune the row left at the old one.
    _beet(cfg, env, "modify", "-w", "-y", "format:FLAC", "title=Relocated FLAC")

    with _mounted(mnt, db, "$albumartist/$album/$title"):
        new = mnt / "Test AA" / "Orig Album" / "Relocated FLAC.flac"
        old = mnt / "Test AA" / "Orig Album" / "Orig FLAC.flac"
        assert new.exists(), sorted(p.name for p in mnt.rglob("*.flac"))
        assert not old.exists()  # stale entry was pruned, not duplicated
        assert len(list(mnt.rglob("*.flac"))) == 1

        ft = mutagen.File(str(new), easy=True)
        assert ft["title"] == ["Relocated FLAC"]
        flac_backing = _beet(cfg, env, "ls", "-p", "format:FLAC").strip()
        assert _audio_md5(new) == _audio_md5(flac_backing)
