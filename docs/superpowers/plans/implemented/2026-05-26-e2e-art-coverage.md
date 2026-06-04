# E2E Cover-Art Coverage Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add end-to-end tests proving that a served virtual file carries the correct front-cover image for FLAC, MP3, and M4A — covering both art-ingestion paths (musefs `scan` of embedded art, and beets-plugin sync of an external cover) and their documented precedence (beets art wins).

**Architecture:** Test-only change, entirely within `contrib/beets/tests/test_e2e.py`. Reuses the existing `beet import → beet musefs → real FUSE mount` harness. Art is embedded into source files with **mutagen** (byte-deterministic) for the scan path, and dropped as a `cover.jpg` with **fetchart** (filesystem source) enabled for the plugin path. Served pictures are extracted with mutagen and compared by `sha256` to the source-of-truth image — mirroring the existing `_audio_md5` integrity check.

**Tech Stack:** Python, pytest (`-m e2e` opt-in marker), beets 2.x + fetchart, mutagen, ffmpeg, the built `musefs` binary, FUSE.

---

## Background facts (already verified — do not re-litigate)

- **Synthesis embeds art for all three formats** via an `ArtImage` segment (`musefs-format/src/{flac,mp3,mp4}.rs::synthesize_layout`). The image payload is spliced **verbatim** from the DB blob; no format re-encodes it. So `sha256(served picture) == sha256(stored image)` holds exactly.
- **`musefs scan` ingests embedded pictures** (`musefs-core/src/scan.rs`, via `flac/mp3/mp4::read_pictures`) into `track_art`.
- **The beets plugin syncs external art**: it reads `album.artpath` and links the cover via `replace_track_art` (`contrib/beets/beetsplug/_core.py`).
- **`beet musefs` runs autoscan BEFORE sync** (`contrib/beets/beetsplug/musefs.py:63-72`), so the plugin's `replace_track_art` overrides any art `scan` ingested → "beets wins" is deterministic.
- **fetchart filesystem source sets `album.artpath`** under `beet import -A -q` with `copy: yes` (spiked: a `cover.jpg` in the source dir is copied into the library album dir and `artpath` is set). No API fallback needed.
- **mutagen embed/extract round-trips preserve exact bytes** for FLAC (`Picture`/`pictures[0].data`), MP3 (`APIC`/`getall("APIC")[0].data`), and M4A (`MP4Cover`/`tags["covr"][0]`).

## Prerequisites for running the tests

These tests carry the `e2e` marker and are skipped by default (`addopts` in `contrib/beets/pyproject.toml`). To run them:

1. Build the binary so `target/debug/musefs` exists:
   ```bash
   cargo build
   ```
2. Run from the `contrib/beets` directory using its virtualenv interpreter, selecting the marker explicitly:
   ```bash
   cd contrib/beets
   .venv/bin/python -m pytest -m e2e tests/test_e2e.py -v
   ```
   (Passing `-m e2e` overrides the default `-m 'not ... and not e2e'`.)

Baseline: the two existing e2e tests pass in ~6s in this environment.

## File structure

- Modify only: `contrib/beets/tests/test_e2e.py`
  - Add stdlib/mutagen imports.
  - Add helpers: `_make_cover`, `_embed_cover`, `_served_cover`, `_check_mount_art`.
  - Extend `_write_config` (opt-in `fetchart`) and `_imported_library` (opt-in `embed_cover` / `external_cover`).
  - Add three tests: `test_e2e_art_embedded_via_scan`, `test_e2e_art_external_via_plugin`, `test_e2e_art_precedence_beets_wins`.

No production code changes.

---

## Task 1: Test infrastructure (imports, helpers, config/library extension)

**Files:**
- Modify: `contrib/beets/tests/test_e2e.py`

- [ ] **Step 1: Add imports**

At the top of the file, add `import hashlib` alongside the existing stdlib imports (e.g. directly after `import os`). Then, immediately after the existing `import mutagen  # noqa: E402` line, add the mutagen submodule imports:

```python
from mutagen.flac import FLAC, Picture  # noqa: E402
from mutagen.id3 import APIC, ID3, ID3NoHeaderError  # noqa: E402
from mutagen.mp4 import MP4, MP4Cover  # noqa: E402
```

- [ ] **Step 2: Add the art helpers**

Add these four helpers in the `# --- helpers ---` section (e.g. after `_audio_md5`):

```python
def _make_cover(path, color):
    """Generate a small real image at `path` (extension picks the codec via
    ffmpeg) and return its bytes. Distinct colors yield distinct sha256s."""
    subprocess.run(
        ["ffmpeg", "-hide_banner", "-loglevel", "error", "-y",
         "-f", "lavfi", "-i", f"color=c={color}:s=64x64", "-frames:v", "1", str(path)],
        check=True, capture_output=True,
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


def _served_cover(path):
    """Extract the raw front-cover image bytes from a (served) audio file."""
    p = str(path)
    if p.endswith(".flac"):
        pics = FLAC(p).pictures
        assert pics, f"no FLAC picture in {path}"
        return bytes(pics[0].data)
    if p.endswith(".mp3"):
        apics = ID3(p).getall("APIC")
        assert apics, f"no MP3 APIC in {path}"
        return bytes(apics[0].data)
    if p.endswith(".m4a"):
        covrs = MP4(p).tags.get("covr") or []
        assert covrs, f"no M4A covr in {path}"
        return bytes(covrs[0])
    raise ValueError(f"unsupported audio for art extract: {path}")


def _check_mount_art(cfg, env, mnt, expected_cover_sha):
    """For each served format under the default `Test AA/Orig Album` tree, assert
    title, byte-faithful audio, and that the served front cover's sha256 equals
    `expected_cover_sha`."""
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
        served_sha = hashlib.sha256(_served_cover(vpath)).hexdigest()
        assert served_sha == expected_cover_sha, f"{vpath.name}: cover sha mismatch"
```

- [ ] **Step 3: Add a `fetchart` switch to `_write_config`**

Replace the existing `_write_config` with this version (adds an opt-in `fetchart` parameter; default off keeps existing callers unchanged):

```python
def _write_config(tmp_path, library, db, fetchart=False):
    plugins = "musefs, fetchart" if fetchart else "musefs"
    fetchart_block = (
        "fetchart:\n"
        "  auto: yes\n"
        "  sources: filesystem\n"
    ) if fetchart else ""
    cfg = tmp_path / "config.yaml"
    cfg.write_text(
        f"directory: {library}\n"
        f"library: {tmp_path / 'beets_lib.db'}\n"
        f"pluginpath: {BEETSPLUG_DIR}\n"
        f"plugins: {plugins}\n"
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
```

- [ ] **Step 4: Add art options to `_imported_library`**

Replace the existing `_imported_library` with this version. The file generation moves before `_write_config` so art can be embedded and the cover placed before import; the return tuple is unchanged:

```python
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

    _ffmpeg_gen(src / "a.flac", 440, title="Orig FLAC", artist="Orig",
                album="Orig Album", album_artist="Test AA")
    _ffmpeg_gen(src / "b.mp3", 330, title="Orig MP3", artist="Orig",
                album="Orig Album", album_artist="Test AA")
    _ffmpeg_gen(src / "c.m4a", 550, title="Orig M4A", artist="Orig",
                album="Orig Album", album_artist="Test AA")

    if embed_cover is not None:
        for name in ("a.flac", "b.mp3", "c.m4a"):
            _embed_cover(src / name, embed_cover, "image/png")
    if external_cover is not None:
        (src / "cover.jpg").write_bytes(external_cover)

    cfg = _write_config(tmp_path, library, db, fetchart=external_cover is not None)
    _beet(cfg, env, "import", "-A", "-q", str(src))
    return cfg, env, db, mnt, library
```

- [ ] **Step 5: Regression-run the existing e2e tests (refactor must not break them)**

Run:
```bash
cargo build
cd contrib/beets && .venv/bin/python -m pytest -m e2e tests/test_e2e.py -v
```
Expected: `test_e2e_import_retag_mount_playback` PASSED and `test_e2e_move_reconcile` PASSED (2 passed). These call `_imported_library`/`_write_config` with no art kwargs, proving the refactor is behavior-preserving.

- [ ] **Step 6: Commit**

```bash
git add contrib/beets/tests/test_e2e.py
git commit -m "$(cat <<'EOF'
test(beets): add e2e art helpers + opt-in cover wiring

Adds _make_cover/_embed_cover/_served_cover/_check_mount_art and opt-in
embed_cover/external_cover (fetchart) hooks to the e2e harness. No new
assertions yet; existing e2e tests unchanged.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Embedded-art scenario (scan path)

Source files carry embedded art; no external cover; fetchart off. `musefs scan` (autoscan) ingests the embedded pictures; the served files must carry that exact image.

**Files:**
- Modify: `contrib/beets/tests/test_e2e.py`

- [ ] **Step 1: Add the test**

Add in the `# --- tests ---` section:

```python
def test_e2e_art_embedded_via_scan(tmp_path):
    cover = _make_cover(tmp_path / "embed.png", "red")
    cfg, env, db, mnt, _ = _imported_library(tmp_path, embed_cover=cover)
    _beet(cfg, env, "musefs")  # autoscan ingests the embedded pictures
    with _mounted(mnt, db, "$albumartist/$album/$title"):
        _check_mount_art(cfg, env, mnt, hashlib.sha256(cover).hexdigest())
```

- [ ] **Step 2: Prove the cover assertion is non-vacuous (deliberate red)**

Temporarily change the last line to assert against a wrong reference:
```python
        _check_mount_art(cfg, env, mnt, hashlib.sha256(b"not-the-cover").hexdigest())
```
Run:
```bash
cd contrib/beets && .venv/bin/python -m pytest -m e2e tests/test_e2e.py::test_e2e_art_embedded_via_scan -v
```
Expected: FAIL with `cover sha mismatch` (proves the served picture is found and byte-compared, not silently skipped). Then revert the line back to `hashlib.sha256(cover).hexdigest()`.

- [ ] **Step 3: Run the test (green)**

Run:
```bash
cd contrib/beets && .venv/bin/python -m pytest -m e2e tests/test_e2e.py::test_e2e_art_embedded_via_scan -v
```
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add contrib/beets/tests/test_e2e.py
git commit -m "$(cat <<'EOF'
test(beets): e2e embedded-art served byte-faithfully via scan

Embeds a cover in FLAC/MP3/M4A sources; asserts musefs scan ingests it and
the mount serves the exact image bytes for every format.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: External-cover scenario (plugin-sync path)

No embedded art; a `cover.jpg` sits in the source album dir with fetchart on, so beets sets `album.artpath` and the plugin links it. The served files must carry that exact cover.

**Files:**
- Modify: `contrib/beets/tests/test_e2e.py`

- [ ] **Step 1: Add the test**

```python
def test_e2e_art_external_via_plugin(tmp_path):
    cover = _make_cover(tmp_path / "ext.jpg", "green")
    cfg, env, db, mnt, _ = _imported_library(tmp_path, external_cover=cover)
    _beet(cfg, env, "musefs")  # plugin syncs album.artpath into track_art
    with _mounted(mnt, db, "$albumartist/$album/$title"):
        _check_mount_art(cfg, env, mnt, hashlib.sha256(cover).hexdigest())
```

- [ ] **Step 2: Run the test**

Run:
```bash
cd contrib/beets && .venv/bin/python -m pytest -m e2e tests/test_e2e.py::test_e2e_art_external_via_plugin -v
```
Expected: PASS. (If it fails with the FLAC/MP3/M4A picture-missing assertion, fetchart did not set `album.artpath`; confirm with `.venv/bin/beet -c <cfg> ls -af '$artpath'` — per the spiked behavior it should be a path under the library album dir.)

- [ ] **Step 3: Commit**

```bash
git add contrib/beets/tests/test_e2e.py
git commit -m "$(cat <<'EOF'
test(beets): e2e external cover served via plugin sync

Drops a cover.jpg with fetchart on; asserts the plugin links album.artpath
and the mount serves the exact cover bytes for FLAC/MP3/M4A.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: Precedence scenario (beets art wins)

Sources carry embedded art **A**; an external cover **B** (distinct) is present with fetchart on. `beet musefs` scans (ingests A) then syncs (replaces with B). The served files must carry **B**, not A.

**Files:**
- Modify: `contrib/beets/tests/test_e2e.py`

- [ ] **Step 1: Add the test**

```python
def test_e2e_art_precedence_beets_wins(tmp_path):
    embedded = _make_cover(tmp_path / "embed.png", "red")
    external = _make_cover(tmp_path / "ext.jpg", "blue")
    embedded_sha = hashlib.sha256(embedded).hexdigest()
    external_sha = hashlib.sha256(external).hexdigest()
    assert embedded_sha != external_sha  # the two covers must be distinguishable

    cfg, env, db, mnt, _ = _imported_library(
        tmp_path, embed_cover=embedded, external_cover=external)
    _beet(cfg, env, "musefs")  # scan ingests A, then sync replaces with B
    with _mounted(mnt, db, "$albumartist/$album/$title"):
        # beets art (external B) wins; the embedded A must not survive.
        _check_mount_art(cfg, env, mnt, external_sha)
```

- [ ] **Step 2: Run the test**

Run:
```bash
cd contrib/beets && .venv/bin/python -m pytest -m e2e tests/test_e2e.py::test_e2e_art_precedence_beets_wins -v
```
Expected: PASS (served cover == external B for all three formats; the `_check_mount_art` sha assert would fail if A had leaked through).

- [ ] **Step 3: Commit**

```bash
git add contrib/beets/tests/test_e2e.py
git commit -m "$(cat <<'EOF'
test(beets): e2e beets art wins over embedded art

Sources embed cover A and carry external cover B; asserts the mount serves
B (plugin sync overrides scan's embedded art) for FLAC/MP3/M4A.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: Final verification

**Files:** none (verification only)

- [ ] **Step 1: Run the full e2e suite**

Run:
```bash
cargo build
cd contrib/beets && .venv/bin/python -m pytest -m e2e tests/test_e2e.py -v
```
Expected: 5 passed — `test_e2e_import_retag_mount_playback`, `test_e2e_move_reconcile`, `test_e2e_art_embedded_via_scan`, `test_e2e_art_external_via_plugin`, `test_e2e_art_precedence_beets_wins`.

- [ ] **Step 2: Run the default (non-e2e) beets suite to confirm collection still works**

Run:
```bash
cd contrib/beets && .venv/bin/python -m pytest -q
```
Expected: the default tier passes and `tests/test_e2e.py` collects without import errors (it is deselected by the default marker filter, but its new module-level mutagen imports must not break collection).

---

## Self-review notes

- **Spec coverage:** scan path → Task 2; plugin-sync path → Task 3; precedence → Task 4; exact-bytes verification → `_check_mount_art` (sha256); helpers/config/library changes → Task 1. All spec sections map to a task.
- **Reference correctness:** embedded path compares to the mutagen-embedded PNG (known bytes); external/precedence compare to the fetchart cover JPEG (stored verbatim by the plugin). Matches the spec's per-path reference rule.
- **Type/name consistency:** `_make_cover`, `_embed_cover`, `_served_cover`, `_check_mount_art`, and the `fetchart` / `embed_cover` / `external_cover` parameters are used with identical names across all tasks.
- **Backwards compatibility:** the two existing e2e tests call `_imported_library(tmp_path)` / `_write_config(tmp_path, library, db)` with no new args; defaults preserve prior behavior (regression-checked in Task 1, Step 5).
