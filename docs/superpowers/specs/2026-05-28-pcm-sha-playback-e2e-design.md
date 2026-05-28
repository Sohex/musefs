# PCM SHA Playback E2E Design

## Goal

Add end-to-end playback fidelity tests that validate every supported served
audio format by decoding both the source file and the mounted musefs file to a
canonical PCM stream and comparing dynamic SHA-256 hashes.

The coverage must exist in both places:

- `musefs-fuse`: direct Rust real-mount E2E coverage for the core filesystem
  invariant.
- `contrib/beets`: Python Beets workflow E2E coverage for import, sync, mount,
  and playback behavior.

The comparison is dynamic. Tests compute the source and mounted hashes on the
same host during the test run. No stored golden hashes are used.

## Architecture

### Rust FUSE Playback E2E

Add a dedicated ignored Rust test at
`musefs-fuse/tests/playback_pcm.rs`, rather than expanding the existing
`mount.rs` or `ogg_read_through.rs` files.

The test generates one short deterministic source file per supported codec and
container, and it records the served extension explicitly:

| Codec/container | Source extension | Served extension |
| --- | --- | --- |
| FLAC | `.flac` | `.flac` |
| MP3 | `.mp3` | `.mp3` |
| M4A/AAC | `.m4a` | `.m4a` |
| Opus-in-Ogg | `.opus` | `.opus` |
| Vorbis-in-Ogg | `.ogg` | `.vorbis` |
| FLAC-in-Ogg | `.oga` | `.oggflac` |
| WAV | `.wav` | `.wav` |

Each source file carries tags that scan into a unique mounted path. The test
then scans the backing directory into an in-memory DB, opens musefs in synthesis
mode, mounts it through `musefs_fuse::spawn`, and validates every generated
case through the mounted tree.

### Beets Playback E2E

Extend `contrib/beets/tests/test_e2e.py` so the Beets workflow also validates
all supported playback formats. Keep the existing art tests scoped to the
formats they already cover; art coverage is not part of this feature.

The Beets playback path should import generated source files, run `beet musefs`
to scan and sync metadata, mount musefs, and compare the mounted file's decoded
PCM SHA-256 against the Beets-reported backing path for that item.

Prefer a new all-format playback helper/test over overloading helpers used by
the existing art tests. That keeps FLAC/MP3/M4A art behavior isolated from Ogg
and WAV playback coverage.

## Canonical PCM Hash

Both harnesses should use the same decode semantics:

```text
ffmpeg -hide_banner -loglevel error -i <path> -map 0:a:0 \
  -f s16le -acodec pcm_s16le -ac 2 -ar 48000 -
```

The test process reads stdout and computes SHA-256 over those PCM bytes.

This canonicalization makes the hash stable across containers and source
sample formats while still asserting the important invariant: musefs must not
change the audible audio stream when it regenerates metadata.

## Components

### Rust Helpers

- `make_audio_fixture(path, codec_args, tags)`: runs `ffmpeg` with a short sine
  or generated-audio source, explicit encoder/container args, and metadata.
  This intentionally duplicates the small helper shape in
  `ogg_read_through.rs` for now because Rust integration tests do not share a
  common module today. Do not refactor `ogg_read_through.rs` as part of this
  feature; extract shared FUSE test helpers only if a later cleanup justifies
  the extra module boundary.
- `pcm_sha256(path)`: decodes the first audio stream to canonical PCM and
  returns a SHA-256 hex string.
- `mount_and_validate(cases)`: scans the backing directory, mounts musefs, and
  validates each expected mounted path.

The Rust test should use a table of cases containing:

- source filename
- served extension and full expected mounted path
- title and artist tags
- ffmpeg codec/container args

The initial codec argument sketches are:

- Opus-in-Ogg: `-c:a libopus`
- Vorbis-in-Ogg: `-c:a libvorbis`
- FLAC-in-Ogg: `-c:a flac -f ogg`

The remaining formats use the same straightforward choices as the existing E2E
fixtures: FLAC defaults or `-c:a flac`, MP3 `-c:a libmp3lame`, M4A/AAC
`-c:a aac`, and WAV `-c:a pcm_s16le`.

### Python Helpers

- Keep `_audio_md5` unchanged for the existing Beets tests that already use
  ffmpeg's `-f md5` output. Add a new `_audio_sha256` helper for the all-format
  playback test, using the same canonical ffmpeg decode as the Rust helper.
- Extend audio generation so the all-format playback test can create FLAC,
  MP3, M4A/AAC, Opus-in-Ogg, Vorbis-in-Ogg, FLAC-in-Ogg, and WAV sources.
- Query Beets for each imported backing path by format/title, then compare that
  path's PCM SHA-256 with the corresponding mounted file.

## Error Handling

Fixture generation may return a per-format unavailable result only for expected
ffmpeg encoder/container failures. That lets a local machine without a specific
codec skip that case cleanly.

These remain hard failures:

- filesystem errors
- scan failures
- mount failures
- mounted path missing
- ffmpeg decode failures
- PCM SHA mismatches

If ffmpeg itself is unavailable, skip the whole playback test. If every format
case is skipped, skip the test with a message that calls out missing codec
support. CI must be configured so the full case list runs.

Mounted path lookup must use deterministic expected paths built from the
template, tags, and served extension. Do not reuse the `find_one_file` pattern
from `ogg_read_through.rs`; a wrong path, served extension, or tag synthesis
result should fail clearly instead of being hidden by a recursive search.

## Test Assertions

Primary assertion:

- `sha256(decode_pcm(source)) == sha256(decode_pcm(mounted))`

Secondary assertions:

- the expected mounted path exists
- synthesized tags/path identify the intended track

Existing tests remain valuable and should not be removed:

- Ogg packet/page tests still validate page CRC and packet preservation details
  that PCM playback alone may not expose.
- Core interop byte-range tests still protect lower-level byte preservation
  without requiring FUSE.
- Existing Beets art E2E tests still cover artwork behavior independently.

## CI Impact

Update the Rust `e2e` job in `.github/workflows/ci.yml` to install `ffmpeg`
alongside `fuse3`, `libfuse3-dev`, and `pkg-config`. The job should continue to
run:

```bash
cargo test -p musefs-fuse -- --ignored
```

Installing `ffmpeg` also activates the existing ignored
`ogg_read_through.rs` codec tests in CI. That is desired: the E2E job should
exercise both the existing Ogg page/payload validation and the new PCM playback
validation.

The Beets playback E2E remains opt-in under the existing `pytest.mark.e2e`
mechanism. Document or preserve the manual command:

```bash
cd contrib/beets
python -m pytest -m e2e tests/test_e2e.py -v
```

## Non-Goals

- No stored golden PCM hashes.
- No comparison between different encodes of the same tone. The source file and
  mounted file are the same encoded audio with regenerated metadata, so decoded
  PCM must match exactly.
- No embedded-art expansion for Ogg or WAV in the Beets art tests.
- No production code changes unless implementation exposes a real bug.
- No broad Beets plugin refactor beyond helper extraction needed for clean
  all-format playback coverage.

## Open Decisions Closed

- Scope is both Rust FUSE E2E and Beets Python E2E.
- Hashes are dynamic source-vs-mounted comparisons.
- The recommended implementation approach is separate but aligned helpers in
  each harness, with the Rust test acting as the exhaustive invariant test and
  the Beets test acting as the plugin workflow smoke.
