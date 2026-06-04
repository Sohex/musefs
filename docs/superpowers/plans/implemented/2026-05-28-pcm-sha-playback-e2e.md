# PCM SHA Playback E2E Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add Rust FUSE and Beets workflow E2E tests that verify every supported served format decodes to the same canonical PCM SHA-256 as its backing source.

**Architecture:** Add a dedicated ignored Rust FUSE playback test for exhaustive direct mount coverage, then extend the opt-in Beets E2E suite with a separate all-format playback workflow. Both harnesses compute dynamic SHA-256 over `ffmpeg`-decoded canonical PCM, compare source versus mounted output, and use deterministic mounted paths including Ogg served-extension remapping.

**Tech Stack:** Rust integration tests, `ffmpeg`, `sha2`, FUSE via `musefs_fuse::spawn`, Python `pytest`, Beets CLI, GitHub Actions.

---

## File Structure

- Create `musefs-fuse/tests/playback_pcm.rs`: ignored real-mount Rust E2E test; owns local ffmpeg fixture generation, PCM SHA-256 decoding, test-case table, mount setup, and deterministic mounted-path validation.
- Modify `musefs-fuse/Cargo.toml`: add `sha2` as a dev-dependency for the new Rust test.
- Modify `contrib/beets/tests/test_e2e.py`: keep `_audio_md5` unchanged, add `_audio_sha256`, add all-format fixture generation/import helper, add new `test_e2e_all_formats_pcm_sha_playback`.
- Create `ruff.toml`: repo-root Ruff config so `tests/interop` is linted intentionally rather than with Ruff defaults. Keep `contrib/beets/ruff.toml` in place; it uses the same rules for the Beets package.
- Modify `.github/workflows/ci.yml`: install `ffmpeg` in the Rust `e2e` job so the new playback test and existing `ogg_read_through.rs` tests execute in CI.

## Task 1: Rust FUSE Playback Test Skeleton

**Files:**
- Create: `musefs-fuse/tests/playback_pcm.rs`
- Modify: `musefs-fuse/Cargo.toml`

- [ ] **Step 1: Add the `sha2` dev-dependency**

In `musefs-fuse/Cargo.toml`, add `sha2 = "0.10"` under `[dev-dependencies]`:

```toml
[dev-dependencies]
tempfile = "3"
metaflac = "0.2"
musefs-db = { path = "../musefs-db" }
musefs-format = { path = "../musefs-format" }
ogg = "0.9"
sha2 = "0.10"
```

- [ ] **Step 2: Create the ignored Rust playback test file**

Create `musefs-fuse/tests/playback_pcm.rs` with this complete skeleton:

```rust
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use musefs_core::{scan_directory, MountConfig, Musefs};
use sha2::{Digest, Sha256};

#[derive(Clone, Copy)]
struct PlaybackCase {
    source_name: &'static str,
    served_ext: &'static str,
    title: &'static str,
    artist: &'static str,
    freq: u32,
    codec_args: &'static [&'static str],
}

fn config() -> MountConfig {
    MountConfig {
        template: "$artist/$title".to_string(),
        fallbacks: BTreeMap::new(),
        default_fallback: "Unknown".to_string(),
        mode: musefs_core::Mode::Synthesis,
        poll_interval: std::time::Duration::ZERO,
    }
}

fn playback_cases() -> Vec<PlaybackCase> {
    vec![
        PlaybackCase {
            source_name: "flac.flac",
            served_ext: "flac",
            title: "PCM FLAC",
            artist: "PCM Artist",
            freq: 330,
            codec_args: &["-c:a", "flac"],
        },
        PlaybackCase {
            source_name: "mp3.mp3",
            served_ext: "mp3",
            title: "PCM MP3",
            artist: "PCM Artist",
            freq: 440,
            codec_args: &["-c:a", "libmp3lame", "-q:a", "5"],
        },
        PlaybackCase {
            source_name: "m4a.m4a",
            served_ext: "m4a",
            title: "PCM M4A",
            artist: "PCM Artist",
            freq: 550,
            codec_args: &["-c:a", "aac", "-b:a", "64k"],
        },
        PlaybackCase {
            source_name: "opus.opus",
            served_ext: "opus",
            title: "PCM Opus",
            artist: "PCM Artist",
            freq: 660,
            codec_args: &["-c:a", "libopus"],
        },
        PlaybackCase {
            source_name: "vorbis.ogg",
            served_ext: "vorbis",
            title: "PCM Vorbis",
            artist: "PCM Artist",
            freq: 770,
            codec_args: &["-c:a", "libvorbis"],
        },
        PlaybackCase {
            source_name: "oggflac.oga",
            served_ext: "oggflac",
            title: "PCM OggFLAC",
            artist: "PCM Artist",
            freq: 880,
            codec_args: &["-c:a", "flac", "-f", "ogg"],
        },
        PlaybackCase {
            source_name: "wav.wav",
            served_ext: "wav",
            title: "PCM WAV",
            artist: "PCM Artist",
            freq: 990,
            codec_args: &["-c:a", "pcm_s16le"],
        },
    ]
}

fn make_audio_fixture(path: &Path, case: PlaybackCase) -> bool {
    let mut cmd = Command::new("ffmpeg");
    let input = format!("sine=frequency={}:duration=0.4:sample_rate=48000", case.freq);
    let title = format!("title={}", case.title);
    let artist = format!("artist={}", case.artist);
    cmd.args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-y",
        "-f",
        "lavfi",
        "-i",
        input.as_str(),
    ]);
    cmd.args(case.codec_args);
    cmd.args(["-metadata", title.as_str(), "-metadata", artist.as_str()]);
    cmd.arg(path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
        && path.exists()
}

fn pcm_sha256(path: &Path) -> String {
    let output = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error"])
        .arg("-i")
        .arg(path)
        .args([
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
        ])
        .output()
        .unwrap_or_else(|err| panic!("failed to run ffmpeg for {}: {err}", path.display()));
    assert!(
        output.status.success(),
        "ffmpeg decode failed for {}: {}",
        path.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    format!("{:x}", Sha256::digest(&output.stdout))
}

fn mounted_path(mountpoint: &Path, case: PlaybackCase) -> PathBuf {
    mountpoint
        .join(case.artist)
        .join(format!("{}.{}", case.title, case.served_ext))
}

#[test]
#[ignore = "requires /dev/fuse + libfuse + ffmpeg; run with --ignored"]
fn all_supported_formats_decode_to_same_pcm_sha_as_source() {
    if Command::new("ffmpeg")
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_or(true, |status| !status.success())
    {
        eprintln!("ffmpeg unavailable; skipping playback PCM E2E");
        return;
    }

    let backing = tempfile::tempdir().unwrap();
    let mut generated = Vec::new();
    for case in playback_cases() {
        let src = backing.path().join(case.source_name);
        if make_audio_fixture(&src, case) {
            generated.push((case, src));
        } else {
            eprintln!("ffmpeg codec/container unavailable for {}; skipping", case.source_name);
        }
    }

    if generated.is_empty() {
        eprintln!("no playback fixtures could be generated; skipping playback PCM E2E");
        return;
    }

    let db = musefs_db::Db::open_in_memory().unwrap();
    scan_directory(&db, backing.path()).unwrap();
    let fs = Musefs::open(db, config()).unwrap();

    let mountpoint = tempfile::tempdir().unwrap();
    let session = musefs_fuse::spawn(fs, mountpoint.path(), "musefs-playback-pcm").unwrap();

    for (case, src) in generated {
        let mounted = mounted_path(mountpoint.path(), case);
        assert!(
            mounted.exists(),
            "expected mounted path {} to exist; tree entries: {:?}",
            mounted.display(),
            std::fs::read_dir(mountpoint.path())
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            pcm_sha256(&mounted),
            pcm_sha256(&src),
            "{} should decode to the same canonical PCM as {}",
            mounted.display(),
            src.display()
        );
    }

    drop(session);
}
```

- [ ] **Step 3: Run the new ignored test list**

Run:

```bash
cargo test -p musefs-fuse --test playback_pcm -- --ignored --list
```

Expected: the output lists `all_supported_formats_decode_to_same_pcm_sha_as_source`.

- [ ] **Step 4: Run the new test on a FUSE host**

Run:

```bash
cargo test -p musefs-fuse --test playback_pcm -- --ignored --nocapture
```

Expected on a host with `/dev/fuse`, libfuse, and ffmpeg codecs: `all_supported_formats_decode_to_same_pcm_sha_as_source ... ok`.

If `/dev/fuse` is unavailable locally, expected result is an environment failure from FUSE setup. In that case, continue after recording that the test must run in CI or on a FUSE host.

- [ ] **Step 5: Commit the Rust playback test**

```bash
git add musefs-fuse/Cargo.toml musefs-fuse/tests/playback_pcm.rs
git commit -m "test(fuse): validate mounted playback PCM sha across formats"
```

## Task 2: Beets All-Format PCM SHA Playback E2E

**Files:**
- Modify: `contrib/beets/tests/test_e2e.py`

- [ ] **Step 1: Add `_audio_sha256` without changing `_audio_md5`**

Insert this helper immediately after `_audio_md5`:

```python
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
```

- [ ] **Step 2: Extend `_ffmpeg_gen` with all-format codec choices**

Replace `_ffmpeg_gen` with the version below. Keep the existing
`sine=frequency={freq}:duration=1` input unchanged so the existing Beets E2E
fixtures do not silently change; this step only adds explicit codec choices for
the new formats plus explicit choices for the formats the helper already
generated.

```python
def _ffmpeg_gen(path, freq, **tags):
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
```

- [ ] **Step 3: Add all-format case data**

Add this constant after `BEET = ...`:

```python
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
```

- [ ] **Step 4: Add an all-format import helper**

Insert this helper after `_imported_library`:

```python
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
    return cfg, env, db, mnt
```

- [ ] **Step 5: Add the Beets all-format playback test**

Insert this test before the art tests:

```python
def test_e2e_all_formats_pcm_sha_playback(tmp_path):
    cfg, env, db, mnt = _imported_playback_library(tmp_path)
    _beet(cfg, env, "musefs")

    with _mounted(mnt, db, "$albumartist/$album/$title"):
        for spec in PLAYBACK_FORMATS:
            mounted = (
                mnt
                / "PCM Album Artist"
                / "PCM Album"
                / f"{spec['title']}.{spec['served_ext']}"
            )
            assert mounted.exists(), sorted(str(p.relative_to(mnt)) for p in mnt.rglob("*"))

            tags = mutagen.File(str(mounted), easy=True)
            assert tags is not None, f"mutagen could not open {mounted}"
            assert tags["title"] == [spec["title"]]

            backing_paths = _beet(cfg, env, "ls", "-p", spec["query"]).splitlines()
            assert backing_paths, f"no beets backing path for {spec['query']}"
            assert len(backing_paths) == 1, (
                f"expected one beets backing path for {spec['query']}, "
                f"got {backing_paths!r}"
            )
            backing = backing_paths[0]
            assert _audio_sha256(mounted) == _audio_sha256(backing)
```

- [ ] **Step 6: Run Python formatting and lint checks**

Run:

```bash
ruff check contrib/beets
ruff format --check contrib/beets
```

Expected: both commands pass.

- [ ] **Step 7: Run the new Beets E2E test on a prepared host**

First ensure the debug binary exists:

```bash
cargo build -p musefs-cli
```

Then run:

```bash
cd contrib/beets
python -m pytest -m e2e tests/test_e2e.py::test_e2e_all_formats_pcm_sha_playback -v
```

Expected on a host with beets, ffmpeg codecs, the musefs binary, and `/dev/fuse`: one test passes.

If `/dev/fuse` is unavailable locally, expected result is a skip or mount-environment failure; record that the E2E must run on a FUSE host.

- [ ] **Step 8: Run the existing Beets E2E playback test**

Run:

```bash
cd contrib/beets
python -m pytest -m e2e tests/test_e2e.py::test_e2e_import_retag_mount_playback -v
```

Expected: the existing test still passes and still uses `_audio_md5`.

- [ ] **Step 9: Commit the Beets playback E2E**

```bash
git add contrib/beets/tests/test_e2e.py
git commit -m "test(beets): validate all-format playback PCM sha"
```

## Task 3: Python Lint Scope And CI E2E ffmpeg Activation

**Files:**
- Create: `ruff.toml`
- Modify: `.github/workflows/ci.yml`

- [ ] **Step 1: Add a repo-root Ruff config for interop tests**

Create `ruff.toml` at the repository root:

```toml
line-length = 100
target-version = "py311"

[lint]
select = ["E", "F", "I", "N", "W"]

[format]
preview = true
```

This makes `tests/interop` linting explicit when Ruff runs from the repo root.
Leave `contrib/beets/ruff.toml` in place; it applies the same rules to the
Beets package.

- [ ] **Step 2: Restore intentional interop lint commands in the plan and CI-compatible checks**

Run:

```bash
ruff check contrib/beets tests/interop
ruff format --check contrib/beets tests/interop
```

Expected: both commands pass, with `tests/interop` using the new root `ruff.toml`.

- [ ] **Step 3: Update the Rust E2E package install**

In `.github/workflows/ci.yml`, replace the `e2e` job install command:

```yaml
      - name: Install libfuse3
        run: sudo apt-get update && sudo apt-get install -y fuse3 libfuse3-dev pkg-config
```

with:

```yaml
      - name: Install FUSE and ffmpeg
        run: sudo apt-get update && sudo apt-get install -y fuse3 libfuse3-dev pkg-config ffmpeg
```

- [ ] **Step 4: Verify workflow YAML syntax by inspection**

Run:

```bash
sed -n '78,92p' .github/workflows/ci.yml
```

Expected: the `e2e` job installs `fuse3 libfuse3-dev pkg-config ffmpeg` before running `cargo test -p musefs-fuse -- --ignored`.

- [ ] **Step 5: Commit the lint and CI changes**

```bash
git add ruff.toml .github/workflows/ci.yml
git commit -m "ci: lint interop explicitly and install ffmpeg for e2e"
```

## Task 4: Final Verification

**Files:**
- Verify: workspace, Rust FUSE E2E, Python lint, Beets E2E where environment permits

- [ ] **Step 1: Run Rust formatting**

Run:

```bash
cargo fmt --all -- --check
```

Expected: passes.

- [ ] **Step 2: Run Rust clippy**

Run:

```bash
cargo clippy --all-targets -- -D warnings
```

Expected: passes.

- [ ] **Step 3: Run normal Rust tests**

Run:

```bash
cargo test --workspace
```

Expected: passes; ignored FUSE tests are listed as ignored but not executed.

- [ ] **Step 4: Run Rust FUSE ignored E2E on a FUSE host**

Run:

```bash
cargo test -p musefs-fuse -- --ignored --nocapture
```

Expected on a FUSE host with ffmpeg: existing ignored tests pass, including `ogg_read_through.rs`, and the new playback PCM test passes.

- [ ] **Step 5: Run Python lint and format checks**

Run:

```bash
ruff check contrib/beets tests/interop
ruff format --check contrib/beets tests/interop
```

Expected: both pass.

- [ ] **Step 6: Run default Beets tests**

Run:

```bash
python -m pytest contrib/beets/tests -v
```

Expected: default tests pass; `e2e` tests are deselected by `contrib/beets/pyproject.toml`.

- [ ] **Step 7: Run full Beets E2E on a prepared FUSE host**

Run:

```bash
cargo build -p musefs-cli
cd contrib/beets
python -m pytest -m e2e tests/test_e2e.py -v
```

Expected on a host with beets, ffmpeg, the musefs binary, and `/dev/fuse`: all Beets E2E tests pass, including `test_e2e_all_formats_pcm_sha_playback`.

- [ ] **Step 8: Record unavailable environment checks**

If any FUSE E2E cannot run locally, record the exact command and the exact environment failure in the PR summary. Do not mark the implementation complete until the command has passed in CI or on another FUSE-capable host.

## Self-Review

- **Spec coverage:** Task 1 implements direct Rust FUSE all-format playback coverage, including `.ogg -> .vorbis` and `.oga -> .oggflac`. Task 2 implements Beets all-format playback coverage while preserving `_audio_md5`. Task 3 makes interop Ruff linting explicit with a root config, installs `ffmpeg` in CI, and explicitly activates existing Ogg E2E tests. Task 4 covers verification.
- **Placeholder scan:** No unresolved markers. Code snippets define the concrete helpers, test cases, commands, and expected outcomes.
- **Type consistency:** Rust helper names are consistent across Task 1. Python helper names and `PLAYBACK_FORMATS` keys are consistent across Task 2. The canonical decode command matches the approved spec in both languages.
