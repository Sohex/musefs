"""Full end-to-end: generate audio -> `beet import` -> retag in beets ->
`beet musefs` (auto-scan + sync) -> real FUSE mount -> verify the mount shows
beets' tags and serves byte-identical audio. Opt-in (marker `e2e`): needs
ffmpeg, the built `musefs` binary, `/dev/fuse` + a fusermount helper, and beets.

Run with: `python -m pytest -m e2e`
"""

import hashlib
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
from mutagen.flac import FLAC, Picture  # noqa: E402
from mutagen.id3 import APIC, ID3, ID3NoHeaderError  # noqa: E402
from mutagen.mp4 import MP4, MP4Cover  # noqa: E402

pytestmark = pytest.mark.e2e

REPO_ROOT = Path(__file__).resolve().parents[3]
BEETSPLUG_DIR = Path(__file__).resolve().parents[1] / "beetsplug"
_DEBUG = REPO_ROOT / "target" / "debug" / "musefs"
_RELEASE = REPO_ROOT / "target" / "release" / "musefs"
MUSEFS = str(_DEBUG if _DEBUG.exists() else _RELEASE)
BEET = os.path.join(os.path.dirname(sys.executable), "beet")


def _fusermount():
    """The unprivileged FUSE unmount helper, or None if neither is installed.

    libfuse 3 installs `fusermount3`; only libfuse 2 provides bare `fusermount`,
    and a fuse3-only system (any current distro) has just the former. Probing
    for the bare name alone skipped this whole tier on exactly the machines most
    likely to run it. Same order as the Rust signal handler's unmount ladder
    (`musefs-cli/src/signal.rs`), minus its `umount` last resort: here a missing
    helper is a skip, not something to fall back from.
    """
    for name in ("fusermount3", "fusermount"):
        found = shutil.which(name)
        if found:
            return found
    return None


PLAYBACK_FORMATS = [
    {
        "filename": "a.flac",
        "freq": 330,
        "title": "PCM FLAC",
        "query": "title:PCM FLAC",
        "served_ext": "flac",
    },
    {
        "filename": "b.mp3",
        "freq": 440,
        "title": "PCM MP3",
        "query": "title:PCM MP3",
        "served_ext": "mp3",
    },
    {
        "filename": "c.m4a",
        "freq": 550,
        "title": "PCM M4A",
        "query": "title:PCM M4A",
        "served_ext": "m4a",
    },
    {
        "filename": "d.opus",
        "freq": 660,
        "title": "PCM Opus",
        "query": "title:PCM Opus",
        "served_ext": "opus",
    },
    {
        "filename": "e.ogg",
        "freq": 770,
        "title": "PCM Vorbis",
        "query": "title:PCM Vorbis",
        "served_ext": "vorbis",
    },
    {
        "filename": "f.oga",
        "freq": 880,
        "title": "PCM OggFLAC",
        "query": "title:PCM OggFLAC",
        "served_ext": "oggflac",
    },
    {
        "filename": "g.wav",
        "freq": 990,
        "title": "PCM WAV",
        "query": "title:PCM WAV",
        "served_ext": "wav",
    },
]


def _missing_tool():
    """Why this tier cannot run here, or None when everything it needs is present."""
    if not (_DEBUG.exists() or _RELEASE.exists()):
        return f"musefs binary not built (looked in {_DEBUG}, {_RELEASE})"
    if not (os.path.exists("/dev/fuse") and _fusermount()):
        return "no /dev/fuse, or no fusermount3/fusermount helper"
    if not shutil.which("ffmpeg"):
        return "ffmpeg not available"
    if not os.path.exists(BEET):
        return f"beet not found at {BEET}"
    return None


@pytest.fixture(autouse=True)
def _require_tools():
    """Skip the test when required external tools are missing — or, in CI's
    contract tier, fail it. A guard that skips is how this tier went dark once
    already (#728): an unmet requirement there must be loud."""
    missing = _missing_tool()
    if missing is None:
        return
    if os.environ.get("MUSEFS_REQUIRE_BIN"):
        pytest.fail(missing)
    pytest.skip(missing)


# --- helpers ---------------------------------------------------------------


def _ffmpeg_gen(path, freq, **tags):
    """Generate an audio fixture with ffmpeg using codec-specific args."""
    cmd = [
        "ffmpeg",
        "-hide_banner",
        "-loglevel",
        "error",
        "-y",
        "-f",
        "lavfi",
        "-i",
        f"sine=frequency={freq}:duration=1",
    ]
    suffix = str(path).lower()
    if suffix.endswith(".flac"):
        cmd += ["-c:a", "flac"]
    elif suffix.endswith(".mp3"):
        cmd += ["-c:a", "libmp3lame", "-q:a", "5"]
    elif suffix.endswith(".m4a"):
        cmd += ["-c:a", "aac", "-b:a", "64k"]
    elif suffix.endswith(".opus"):
        cmd += ["-c:a", "libopus"]
    elif suffix.endswith(".ogg"):
        cmd += ["-c:a", "libvorbis"]
    elif suffix.endswith(".oga"):
        cmd += ["-c:a", "flac", "-f", "ogg"]
    elif suffix.endswith(".wav"):
        cmd += ["-c:a", "pcm_s16le"]
    for key, value in tags.items():
        cmd += ["-metadata", f"{key}={value}"]
    cmd.append(str(path))
    subprocess.run(cmd, check=True, capture_output=True)


def _env(tmp_path):
    """Return a copy of os.environ with BEETSDIR isolated to tmp_path."""
    env = dict(os.environ)
    env["BEETSDIR"] = str(tmp_path)  # isolate from any real beets config
    return env


def _write_config(tmp_path, library, db, fetchart=False):
    """Write a beets config.yaml pointing at the musefs plugin."""
    plugins_block = ("plugins:\n  - musefs\n  - fetchart\n") if fetchart else "plugins: musefs\n"
    fetchart_block = ("fetchart:\n  auto: yes\n  sources:\n    - filesystem\n") if fetchart else ""
    cfg = tmp_path / "config.yaml"
    cfg.write_text(
        f"directory: {library}\n"
        f"library: {tmp_path / 'beets_lib.db'}\n"
        f"pluginpath: {BEETSPLUG_DIR}\n"
        f"{plugins_block}"
        f"musefs:\n"
        f"  db: {db}\n"
        f"  bin: {MUSEFS}\n"
        f"  autoscan: yes\n"
        f"{fetchart_block}"
        f"import:\n"
        f"  copy: yes\n"
        f"  write: no\n"
    )
    return cfg


def _beet(cfg, env, *args):
    """Run a beet subcommand and return stdout, raising on failure."""
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
        [
            "ffmpeg",
            "-hide_banner",
            "-loglevel",
            "error",
            "-i",
            str(path),
            "-map",
            "0:a",
            "-f",
            "md5",
            "-",
        ],
        check=True,
        capture_output=True,
    ).stdout.decode()
    return out.strip()


def _audio_sha256(path):
    """SHA-256 of canonical decoded PCM used by the all-format playback E2E."""
    out = subprocess.run(
        [
            "ffmpeg",
            "-hide_banner",
            "-loglevel",
            "error",
            "-i",
            str(path),
            "-map",
            "0:a:0",
            "-f",
            "s16le",
            "-acodec",
            "pcm_s16le",
            "-ac",
            "2",
            "-ar",
            "48000",
            "-",
        ],
        check=True,
        capture_output=True,
    ).stdout
    return hashlib.sha256(out).hexdigest()


def _make_cover(path, color):
    """Generate a small real image at `path` (extension picks the codec via
    ffmpeg) and return its bytes. Distinct colors yield distinct sha256s."""
    subprocess.run(
        [
            "ffmpeg",
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            f"color=c={color}:s=64x64",
            "-frames:v",
            "1",
            str(path),
        ],
        check=True,
        capture_output=True,
    )
    return Path(path).read_bytes()


def _embed_cover(path, cover_bytes, mime):
    """Embed `cover_bytes` as a front cover (type 3) into the audio file at
    `path` with mutagen, so the stored picture payload is byte-identical to
    `cover_bytes`."""
    p = str(path)
    if p.endswith(".flac"):
        flac = FLAC(p)
        pic = Picture()
        pic.type = 3
        pic.mime = mime
        pic.data = cover_bytes
        flac.add_picture(pic)
        flac.save()
    elif p.endswith(".mp3"):
        try:
            tags = ID3(p)
        except ID3NoHeaderError:
            tags = ID3()
        tags.add(APIC(encoding=3, mime=mime, type=3, desc="", data=cover_bytes))
        tags.save(p)
    elif p.endswith(".m4a"):
        fmt = MP4Cover.FORMAT_PNG if mime == "image/png" else MP4Cover.FORMAT_JPEG
        mp4 = MP4(p)
        mp4["covr"] = [MP4Cover(cover_bytes, imageformat=fmt)]
        mp4.save()
    else:
        raise ValueError(f"unsupported audio for art embed: {path}")


def _served_covers(path):
    """Every picture a (served) audio file carries, as ``(data, mime, type,
    description)``.

    M4A's ``covr`` has no picture type or description, so those are ``None``
    there, and its mime is only the PNG/JPEG flag the atom holds.
    """
    p = str(path)
    if p.endswith(".flac"):
        return [(bytes(pic.data), pic.mime, pic.type, pic.desc) for pic in FLAC(p).pictures]
    if p.endswith(".mp3"):
        return [(bytes(a.data), a.mime, a.type, a.desc) for a in ID3(p).getall("APIC")]
    if p.endswith(".m4a"):
        covrs = MP4(p).tags.get("covr") or []
        return [
            (
                bytes(c),
                "image/png" if c.imageformat == MP4Cover.FORMAT_PNG else "image/jpeg",
                None,
                None,
            )
            for c in covrs
        ]
    raise ValueError(f"unsupported audio for art extract: {path}")


def _check_mount_art(cfg, env, mnt, expected_cover_sha, expected_mime):
    """For each served format under the default `Test AA/Orig Album` tree, assert
    title, byte-faithful audio, and that the file carries exactly one picture:
    the front cover whose sha256 is `expected_cover_sha`, declared as
    `expected_mime`.

    Exactly one, not "the first one matches": where the two art sources meet,
    the one that loses must not be served alongside the one that wins.

    On M4A the mime is weaker evidence than it looks. Synthesis can only choose
    between the atom's PNG and JPEG flags, and anything that is not ``image/png``
    is served as JPEG — so a JPEG there cannot be told from a wrong mime. The
    FLAC and MP3 checks carry that half."""
    specs = [
        (mnt / "Test AA" / "Orig Album" / "Orig FLAC.flac", "format:FLAC", "Orig FLAC"),
        (mnt / "Test AA" / "Orig Album" / "Orig MP3.mp3", "format:MP3", "Orig MP3"),
        (mnt / "Test AA" / "Orig Album" / "Orig M4A.m4a", "format:AAC", "Orig M4A"),
    ]
    for vpath, fquery, title in specs:
        assert vpath.exists(), sorted(p.name for p in mnt.rglob("*"))
        tags = mutagen.File(str(vpath), easy=True)
        assert tags["title"] == [title]
        backing = _beet(cfg, env, "ls", "-p", fquery).strip()
        assert _audio_md5(str(vpath)) == _audio_md5(backing)
        covers = _served_covers(vpath)
        assert len(covers) == 1, f"{vpath.name}: expected one picture, got {len(covers)}"
        ((data, mime, picture_type, description),) = covers
        assert hashlib.sha256(data).hexdigest() == expected_cover_sha, (
            f"{vpath.name}: cover sha mismatch"
        )
        assert mime == expected_mime, f"{vpath.name}: served mime {mime!r}"
        if picture_type is not None:  # M4A carries neither
            assert (picture_type, description) == (3, ""), vpath.name


@contextmanager
def _mounted(mnt, db, template):
    """Context manager that mounts via musefs and unmounts on exit."""
    proc = subprocess.Popen(
        [MUSEFS, "mount", str(mnt), "--db", str(db), "--template", template],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    try:
        deadline = time.time() + 10
        while time.time() < deadline:
            if os.path.ismount(str(mnt)):
                break
            if proc.poll() is not None:
                raise AssertionError(
                    "musefs mount exited early: " + proc.stderr.read().decode(errors="replace")
                )
            time.sleep(0.05)
        else:
            raise AssertionError("musefs mount did not come up within 10s")
        yield
    finally:
        # Resolved rather than assumed: the guard above has already established
        # that one of the two exists.
        subprocess.run([_fusermount(), "-u", str(mnt)], capture_output=True)
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()


def _imported_library(tmp_path, *, embed_cover=None, external_cover=None):
    """Generate a FLAC, MP3, and M4A, import them into a fresh beets library
    (as-is). Returns (cfg, env, db, mnt, library).

    embed_cover: PNG bytes embedded as a front cover into every source file
        (exercises musefs scan's embedded-art ingestion).
    external_cover: JPEG bytes written as `cover.jpg` in the source album dir,
        with fetchart enabled so beets sets album.artpath (exercises the plugin
        sync art path)."""
    src = tmp_path / "src"
    library = tmp_path / "library"
    mnt = tmp_path / "mnt"
    for d in (src, library, mnt):
        d.mkdir()
    db = tmp_path / "musefs.db"
    env = _env(tmp_path)

    _ffmpeg_gen(
        src / "a.flac",
        440,
        title="Orig FLAC",
        artist="Orig",
        album="Orig Album",
        album_artist="Test AA",
    )
    _ffmpeg_gen(
        src / "b.mp3",
        330,
        title="Orig MP3",
        artist="Orig",
        album="Orig Album",
        album_artist="Test AA",
    )
    _ffmpeg_gen(
        src / "c.m4a",
        550,
        title="Orig M4A",
        artist="Orig",
        album="Orig Album",
        album_artist="Test AA",
    )

    if embed_cover is not None:
        for name in ("a.flac", "b.mp3", "c.m4a"):
            _embed_cover(src / name, embed_cover, "image/png")
    if external_cover is not None:
        (src / "cover.jpg").write_bytes(external_cover)

    cfg = _write_config(tmp_path, library, db, fetchart=external_cover is not None)
    _beet(cfg, env, "import", "-A", "-q", str(src))

    if external_cover is not None:
        album_dir = library / "Test AA" / "Orig Album"
        (album_dir / "cover.jpg").write_bytes(external_cover)
        _beet(cfg, env, "fetchart", "-f")

    return cfg, env, db, mnt, library


def _imported_playback_library(tmp_path):
    """Generate all supported playback formats and import them into beets."""
    src = tmp_path / "src"
    library = tmp_path / "library"
    mnt = tmp_path / "mnt"
    for d in (src, library, mnt):
        d.mkdir()
    db = tmp_path / "musefs.db"
    env = _env(tmp_path)

    for spec in PLAYBACK_FORMATS:
        _ffmpeg_gen(
            src / spec["filename"],
            spec["freq"],
            title=spec["title"],
            artist="PCM Artist",
            album="PCM Album",
            album_artist="PCM Album Artist",
        )

    cfg = _write_config(tmp_path, library, db)
    _beet(cfg, env, "import", "-A", "-q", str(src))

    imported = []
    for spec in PLAYBACK_FORMATS:
        paths = _beet(cfg, env, "ls", "-p", spec["query"]).splitlines()
        if len(paths) == 1:
            imported.append(spec)
    assert imported, "no playback formats were imported successfully"
    return cfg, env, db, mnt, imported


# --- tests -----------------------------------------------------------------


def test_e2e_import_retag_mount_playback(tmp_path):
    """Retag in beets, mount, verify tags and audio."""
    cfg, env, db, mnt, library = _imported_library(tmp_path)

    # Retag in the beets DB only (no file write, no move) so the divergence is
    # real: files keep their original embedded tags; the mount must show beets'.
    _beet(
        cfg,
        env,
        "modify",
        "-W",
        "-M",
        "-y",
        "format:FLAC",
        "title=New FLAC",
        "artist=New Artist",
        "albumartist=AA",
        "album=New Album",
    )
    _beet(
        cfg,
        env,
        "modify",
        "-W",
        "-M",
        "-y",
        "format:MP3",
        "title=New MP3",
        "artist=New Artist",
        "albumartist=AA",
        "album=New Album",
    )
    # beets classifies m4a/AAC audio with format "AAC" (not "M4A"/"MP4").
    _beet(
        cfg,
        env,
        "modify",
        "-W",
        "-M",
        "-y",
        "format:AAC",
        "title=New M4A",
        "artist=New Artist",
        "albumartist=AA",
        "album=New Album",
    )

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
        assert mt["artist"] == ["New Artist"]
        assert mt["albumartist"] == ["AA"]
        assert mt["album"] == ["New Album"]

        at = mutagen.File(str(m4a), easy=True)
        assert at["title"] == ["New M4A"]
        assert at["artist"] == ["New Artist"]
        assert at["albumartist"] == ["AA"]
        assert at["album"] == ["New Album"]

        # Audio served byte-faithfully: decoded PCM identical to the backing file.
        assert _audio_md5(flac) == _audio_md5(flac_backing)
        assert _audio_md5(mp3) == _audio_md5(mp3_backing)
        assert _audio_md5(m4a) == _audio_md5(m4a_backing)


def test_e2e_move_reconcile(tmp_path):
    """Rename via beets modify, verify mount reflects the move."""
    cfg, env, db, mnt, library = _imported_library(tmp_path)
    _beet(cfg, env, "musefs")  # initial sync (original tags)

    # A write-back modify renames/moves the FLAC. The passive cli_exit reconcile
    # scans the new path but never prunes (#538: pruning is a deliberate act), so
    # an explicit `beet musefs --revalidate` is what removes the row left at the
    # old path (it forwards to `musefs revalidate --prune`).
    _beet(cfg, env, "modify", "-w", "-y", "format:FLAC", "title=Relocated FLAC")
    _beet(cfg, env, "musefs", "--revalidate")  # deliberate prune of the moved-away row

    with _mounted(mnt, db, "$albumartist/$album/$title"):
        new = mnt / "Test AA" / "Orig Album" / "Relocated FLAC.flac"
        old = mnt / "Test AA" / "Orig Album" / "Orig FLAC.flac"
        assert new.exists(), sorted(p.name for p in mnt.rglob("*.flac"))
        assert not old.exists()  # stale entry pruned by the explicit sync
        assert len(list(mnt.rglob("*.flac"))) == 1

        ft = mutagen.File(str(new), easy=True)
        assert ft["title"] == ["Relocated FLAC"]
        flac_backing = _beet(cfg, env, "ls", "-p", "format:FLAC").strip()
        assert _audio_md5(new) == _audio_md5(flac_backing)


def test_e2e_all_formats_pcm_sha_playback(tmp_path):
    """Verify all formats decode to same PCM SHA."""
    cfg, env, db, mnt, imported = _imported_playback_library(tmp_path)
    _beet(cfg, env, "musefs")

    with _mounted(mnt, db, "$albumartist/$album/$title"):
        for spec in imported:
            mounted = (
                mnt / "PCM Album Artist" / "PCM Album" / f"{spec['title']}.{spec['served_ext']}"
            )
            assert mounted.exists(), sorted(str(p.relative_to(mnt)) for p in mnt.rglob("*"))

            tags = mutagen.File(str(mounted), easy=True)
            assert tags is not None, f"mutagen could not open {mounted}"
            assert tags["title"] == [spec["title"]]

            backing_paths = _beet(cfg, env, "ls", "-p", spec["query"]).splitlines()
            assert backing_paths, f"no beets backing path for {spec['query']}"
            assert len(backing_paths) == 1, (
                f"expected one beets backing path for {spec['query']}, got {backing_paths!r}"
            )
            backing = backing_paths[0]
            assert _audio_sha256(mounted) == _audio_sha256(backing)


def test_e2e_art_embedded_via_scan(tmp_path):
    """Verify embedded art survives scan and mount."""
    cover = _make_cover(tmp_path / "embed.png", "red")
    cfg, env, db, mnt, _ = _imported_library(tmp_path, embed_cover=cover)
    _beet(cfg, env, "musefs")  # autoscan ingests the embedded pictures
    with _mounted(mnt, db, "$albumartist/$album/$title"):
        _check_mount_art(cfg, env, mnt, hashlib.sha256(cover).hexdigest(), "image/png")


def test_e2e_art_external_via_plugin(tmp_path):
    """Verify external cover.jpg is synced via plugin."""
    cover = _make_cover(tmp_path / "ext.jpg", "green")
    cfg, env, db, mnt, _ = _imported_library(tmp_path, external_cover=cover)
    _beet(cfg, env, "musefs")  # plugin syncs album.artpath into track_art
    with _mounted(mnt, db, "$albumartist/$album/$title"):
        # The mime is the plugin's own sniff of the file, stored on the link.
        _check_mount_art(cfg, env, mnt, hashlib.sha256(cover).hexdigest(), "image/jpeg")


def test_e2e_art_precedence_beets_wins(tmp_path):
    """Verify beets art wins over embedded art."""
    embedded = _make_cover(tmp_path / "embed.png", "red")
    external = _make_cover(tmp_path / "ext.jpg", "blue")
    embedded_sha = hashlib.sha256(embedded).hexdigest()
    external_sha = hashlib.sha256(external).hexdigest()
    assert embedded_sha != external_sha  # the two covers must be distinguishable

    cfg, env, db, mnt, _ = _imported_library(
        tmp_path, embed_cover=embedded, external_cover=external
    )
    _beet(cfg, env, "musefs")  # scan ingests A, then sync replaces with B
    with _mounted(mnt, db, "$albumartist/$album/$title"):
        # beets art (external B) wins; the embedded A must not survive.
        _check_mount_art(cfg, env, mnt, external_sha, "image/jpeg")


def test_e2e_full_fields_sticky_delete_and_restore(tmp_path):
    """Rich fields reach the mount; a deleted file-embedded tag stays gone across
    re-scans; --restore-backing brings the backing value back."""
    cfg, env, db, mnt, library = _imported_library(tmp_path)
    template = "$albumartist/$album/$title"

    # `-w`, explicitly: without it `modify` falls back to `import.write`, which
    # this config sets to no, so "from file" would only ever reach the store
    # through the sync and --restore-backing below would have no file value to
    # bring back.
    _beet(
        cfg,
        env,
        "modify",
        "-w",
        "-M",
        "-y",
        "format:FLAC",
        "comments=from file",
        "rg_track_gain=-7.5",
        "mb_albumid=11111111-1111-1111-1111-111111111111",
    )
    _beet(cfg, env, "musefs")
    with _mounted(mnt, db, template):
        ft = FLAC(str(next(mnt.rglob("*.flac"))))
        assert ft["replaygain_track_gain"] == ["-7.50 dB"]
        assert ft["musicbrainz_albumid"] == ["11111111-1111-1111-1111-111111111111"]
        assert ft["comment"][0] == "from file"

    _beet(cfg, env, "modify", "-W", "-M", "-y", "format:FLAC", "comments!")
    _beet(cfg, env, "musefs")
    _beet(cfg, env, "musefs")
    with _mounted(mnt, db, template):
        ft = FLAC(str(next(mnt.rglob("*.flac"))))
        assert "comment" not in ft

    _beet(cfg, env, "musefs", "--restore-backing")
    with _mounted(mnt, db, template):
        ft = FLAC(str(next(mnt.rglob("*.flac"))))
        assert ft["comment"][0] == "from file"
