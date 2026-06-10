# Mount-boundary read-consistency, mmap fidelity, and read-only refusal e2e

Design spec for GitHub issues #215 and #214.

## Problem

The FUSE e2e suite (`musefs-fuse/tests/`) exercises the mount boundary only
through `pread`-based sequential and seek reads. Two classes of kernel-facing
behavior are untested:

- **#215** — There is no standardized read-consistency / I/O-pattern harness run
  against a live mount. The daemon-level `read_at` property tests
  (`musefs-core/tests/proptest_read_fidelity.rs`) cover the splicing arithmetic
  but never go through a kernel mount, so the kernel's own offset/length
  splitting, short reads, and readahead are unverified.
- **#214** — Serving a file via `mmap` (the kernel `readpage` / page-cache path,
  distinct from `pread`) is never exercised, and no test asserts that mutating
  operations against the read-only mount are refused with a defined errno. Both
  bear directly on the cardinal invariant: original audio bytes are never copied
  or modified.

## Why not fsx/fio

`fsx` and `fio` are the genre-standard tools the issue names, but both are
fundamentally read-write: `fsx` mutates a file and self-compares; `fio`'s verify
modes expect a pattern it wrote itself. The musefs mount is `MountOption::RO` and
its served bytes are freshly synthesized, not a fillable pattern. Neither tool can
operate in its normal mode against a read-only synthesized mount. The faithful
read-only equivalent is an **oracle-based harness**: read the served file once
into memory, then fire randomized `(offset, len)` `pread`s and `mmap` slices at the
live mount and compare each against the oracle slice. That is exactly the
"randomized read/seek/mmap vs. known expected bytes" the issue asks for,
implemented in-tree rather than shelling out to a tool that cannot run read-only.

## Scope

In scope: a single new `#[ignore]` integration test file that adds (a) a seeded
randomized read/seek consistency sweep, (b) whole-file mmap fidelity, (c) a
read-only write-refusal matrix, and (d) a multi-format breadth sweep. Plus one
`CONTRIBUTING.md` bullet.

Out of scope (YAGNI):

- No external `fsx`/`fio` binaries and no CI changes to install them.
- No `write()`/`create()`/`setattr()` implementations in the FUSE layer — the
  mount is `MountOption::RO`, so the kernel refuses writes at the VFS layer before
  they reach the daemon. The behavior under test already exists; only tests are
  added.
- No new CI job. The existing `e2e` job already runs
  `cargo test -p musefs-fuse -- --ignored` with `fuse3`/`libfuse3-dev`/`ffmpeg`
  installed.
- No README or ARCHITECTURE changes.

## Design

### Placement

New file `musefs-fuse/tests/read_consistency.rs`, written in the established
in-process-mount style used by `mount.rs`: build a backing dir, scan it into an
in-memory `musefs_db::Db`, `Musefs::open`, `musefs_fuse::spawn(...)` to mount in
the background, read through the mountpoint, and `drop(session)` to unmount. It is
gated with `#[ignore = "requires /dev/fuse; run with: cargo test -p musefs-fuse -- --ignored"]`
like the rest of the suite and runs in the existing CI `e2e` job with no workflow
change.

**Fixture invariant — `Mode::Synthesis` is required.** Every fixture must mount in
`Mode::Synthesis` (as `mount.rs::config()` does). This is load-bearing, not
incidental: musefs requests `FUSE_PASSTHROUGH` in `init`
(`musefs-fuse/src/platform/passthrough.rs`), and `Musefs::passthrough_fd`
(`musefs-core/src/facade.rs`) hands the kernel a backing fd **only** in
`Mode::StructureOnly`. Under StructureOnly the kernel reads the backing file
directly, `read_at` never runs, and an mmap-fidelity test would compare the raw
backing file to itself — testing nothing about synthesis. In `Mode::Synthesis`
`passthrough_fd` returns `None`, so all reads and page faults traverse the
daemon's `read_at` splice path, which is exactly what #215/#214 must exercise.
Each fixture builder therefore treats "config uses `Mode::Synthesis`" as an
invariant.

**Fixture-helper sourcing.** Each `tests/*.rs` file is its own crate, so items in
`mount.rs` / `playback_pcm.rs` / `ogg_read_through.rs` cannot be `use`d from the
new file. The repo's established convention is per-file copies — `make_flac` and
`config()` already appear independently in `mount.rs`, `keep_cache.rs`,
`passthrough.rs`, and `concurrency.rs`. This spec follows that convention:
`read_consistency.rs` copies the helpers it needs (`make_flac`, `config()`, the
`scan → spawn → drop` boilerplate, and the `PlaybackCase` / `make_audio_fixture` /
`mounted_path` / with-cover ffmpeg builders). Extracting a shared
`tests/common/mod.rs` and refactoring the existing green test files onto it is
deliberately out of scope — it would churn passing tests for no behavioral gain.

### Dev-dependency

Add `memmap2 = "0.9"` to `[dev-dependencies]` of `musefs-fuse/Cargo.toml` — a
safe, well-known mmap wrapper. No PRNG crate is added: a small inline
`xorshift64` seeded from a fixed constant drives the randomized sweep so failures
are perfectly reproducible without `proptest` or `rand`.

```rust
// Deterministic, dependency-free PRNG for reproducible randomized reads.
struct XorShift64(u64);
impl XorShift64 {
    fn new(seed: u64) -> Self { Self(seed) }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, bound: u64) -> u64 {
        if bound == 0 { 0 } else { self.next_u64() % bound }
    }
}
const SEED: u64 = 0x9E37_79B9_7F4A_7C15;
```

### Component (a): oracle randomized read/seek sweep — #215

Format-agnostic helper `sweep_reads(served_path)`:

1. Read the served file fully via `std::fs::read` → the in-memory **oracle**
   (`Vec<u8>`), of length `n`.
2. Open the file once with `File::open` for positioned reads
   (`std::os::unix::fs::FileExt::read_at`) and once via `memmap2::Mmap`
   (`MAP_SHARED`, `PROT_READ`) over the whole file.
3. Run `ITERS` (~2000) seeded iterations. Each picks an `(offset, len)` biased
   toward boundary cases rather than uniform random. The two variables carry
   distinct "edge" conditions, kept unambiguous:
   - `offset` drawn from `{0, 1, n - 1, n, a-random-in-range value}` plus the
     **seam offsets** when known (see below). `offset == n` and `offset > n` are
     the past-EOF cases.
   - `len` drawn from `{0, 1, a-random value, a value that makes
     offset + len > n}` — i.e. a read that starts in range but crosses EOF.
   So the two distinct EOF cases are: (in-range `offset`, `len` crossing `n`) and
   (`offset >= n`, any `len`). Both expect a read count of `0` for the past-EOF
   start.

   **Seam targeting (hermetic FLAC case).** The format-agnostic sweep cannot know
   where a synthesized `Inline` prefix meets the `BackingAudio` tail, and biasing
   to `0/1/n-1/n` clusters at file *ends*, not at that interior seam. For the
   hermetic `make_flac` fixture the test constructs the file, so the prefix length
   (the synthesized `fLaC` + STREAMINFO + rewritten VORBIS_COMMENT bytes, i.e.
   served length minus the known backing-audio length) is computable. The sweep
   helper accepts an optional `seam: Option<u64>`; when `Some(s)` it adds
   `s`, `s ± 1`, `s ± 8` to the biased offset set, so the synthesized/`BackingAudio`
   boundary is straddled **deterministically every run** rather than
   probabilistically. For the ffmpeg breadth sweep (component d) the seam is not
   known, so `seam` is `None` and small fixtures keep the offset space densely
   sampled.
4. For each `(offset, len)`:
   - `read_at`: assert the returned byte count equals `min(len, n - min(offset, n))`
     (the expected short-read length) and the returned bytes equal
     `oracle[offset..offset + count]`. This is correct for every case: `offset >= n`
     yields `n - min(offset, n) == 0` → count `0`, and `len == 0` yields count `0`.
   - `mmap`: assert `mmap[offset..offset + clamped_len]` equals the same oracle
     slice (mmap has no short-read; compare the clamped in-bounds slice). The mmap
     compare is skipped for `offset >= n` (nothing in-bounds to map-compare).
5. On any mismatch the assertion message prints `SEED`, `offset`, `len`, and the
   format/path so the exact case reproduces deterministically.

Boundary bias is deliberate: uniform random offsets over a large file rarely land
on the low offsets and EOF edges where a kernel offset/length-split or short-read
bug bites. The explicit `0`/`1`/`n-1`/`n` sampling plus deterministic seam
targeting (hermetic FLAC) covers both the file ends and the interior
synthesized/`BackingAudio` boundary every run.

**Page-fault safety (mmap).** The mount runs on a `BackgroundSession` whose worker
pool serves `read` requests concurrently, so page faults on the `MAP_SHARED`
region become FUSE reads served by that pool — faulting cannot deadlock the test
thread. `memmap2::Mmap` requires a non-empty file, so fixtures are asserted
non-empty before mapping.

### Component (b): whole-file mmap fidelity — #214

`mmap_matches_pread(served_path)`: map the entire served file `MAP_SHARED` /
`PROT_READ` via `memmap2::Mmap`, and assert the mapped contents equal
`std::fs::read(served_path)` byte-for-byte. This exercises the kernel
`readpage`/page-cache path, distinct from `pread`, and directly guards the
byte-identical-audio invariant. It runs on the **hand-built `make_flac` fixture**
(copied from `mount.rs`) so it is hermetic and always runs, even when
ffmpeg/codecs are unavailable.

### Component (c): read-only write-refusal matrix — #214

`write_ops_are_refused(mountpoint, served_path)`: against the served file and the
mount root, assert each mutating operation **fails** (return value `-1`) with an
errno drawn from a documented accepted set. The contract under test is "mutation
is refused," **not** "refused with exactly `EROFS`." The mount is
`MountOption::RO`, so the kernel enforces refusal at the VFS layer — but the exact
errno can vary by operation and kernel version (a write-unrelated check, e.g. fd
writability, may fire before the RO check), so strict equality on `EROFS` would be
flaky. Each row asserts `ret == -1 && accepted_set.contains(errno)`:

| Operation                         | Target            | Accepted errno set        |
| --------------------------------- | ----------------- | ------------------------- |
| `open(O_WRONLY)`                  | existing file     | `{EROFS}`                 |
| `open(O_RDWR)`                    | existing file     | `{EROFS}`                 |
| `open(O_CREAT)`, fresh path       | **new** path in mount | `{EROFS}`             |
| `unlink`                          | existing file     | `{EROFS}`                 |
| `truncate`                        | existing file     | `{EROFS}`                 |
| `ftruncate` (on an `O_RDONLY` fd) | existing file     | `{EINVAL, EROFS}`         |
| `mkdir`                           | new dir in mount  | `{EROFS}`                 |
| `chmod` (setattr path)            | existing file     | `{EROFS, EPERM}`          |
| `utimes` (setattr path)           | existing file     | `{EROFS, EPERM, EACCES}`  |

Notes: the `O_CREAT` row must target a genuinely new path (not the served file),
so it exercises create-refusal rather than an existing-file open. `ftruncate` on an
`O_RDONLY` fd commonly returns `EINVAL` (the fd is not writable — a check
independent of the RO mount) rather than `EROFS`, hence the two-element set.
`chmod`/`utimes` are normally `EROFS` on an RO mount but can surface `EPERM`/
`EACCES` on some kernels before the RO check. Implemented with direct `libc`
calls, reading `std::io::Error::last_os_error().raw_os_error()`. This test is
hermetic on `make_flac`.

### Component (d): multi-format breadth sweep — #215

Reuse `playback_pcm.rs`'s `PlaybackCase` + `make_audio_fixture`, plus
`ogg_read_through.rs`'s with-cover variant, to generate fixtures for all formats
the suite already covers (`flac`, `mp3`, `m4a`, `opus`, `vorbis`, `oggflac`,
`wav`), with embedded cover art where the format supports it. For each fixture:
scan → mount → run the component (a) sweep against the served file.

Per-format generation uses the suite's established **skip-if-codec-unavailable**
pattern (`make_audio_fixture` returns `false` → log and skip that format). The
test first checks `ffmpeg -version` and returns early if ffmpeg is entirely
absent, and asserts that at least one fixture was generated so it cannot silently
no-op on every format. To bound total e2e runtime, the breadth sweep runs a
reduced iteration count per format (~500, vs the hermetic FLAC sweep's ~2000)
rather than the full count across all seven containers; the hermetic FLAC sweep
carries the deep per-offset coverage. This is where `Inline`, `ArtImage`, `BinaryTag`,
`OggAudio` (patched-in-place pages), and `OggArtSlice` (incremental base64) +
`BackingAudio` segment splicing meets the kernel read boundary across containers.

### Error handling and determinism

- A fixed `SEED` constant makes the randomized sweep fully reproducible; assert
  messages echo the seed, offset, length, and format.
- ffmpeg-derived fixtures (component d) skip gracefully when a codec is missing —
  non-hermetic by nature, but consistent with `playback_pcm.rs` and
  `ogg_read_through.rs`.
- The hermetic FLAC tests (components b and c) form the always-runs floor, so
  #214's read-only contract and whole-file mmap fidelity are never skipped even
  on a machine without ffmpeg.
- Oracle stability: the served file is read once as the oracle, then all preads
  compare against it. This is valid only if nothing retags mid-test. The fixtures
  use `poll_interval: Duration::ZERO` (so `lookup`/`getattr`'s `fire_poll_refresh`
  is a no-op) and the in-memory DB is not written during the test, so the served
  bytes are immutable for the run. A future author copying this harness against a
  live-polling mount would need to account for a racy oracle.

### Documentation

Add one bullet to the e2e / test-tiers section of `CONTRIBUTING.md` noting the new
read-consistency harness in `musefs-fuse/tests/read_consistency.rs`, that the
randomized read/mmap sweep is seeded and reproducible (print the seed on failure),
and that the multi-format breadth sweep skips formats whose ffmpeg codec is
unavailable while the FLAC-based mmap-fidelity and write-refusal tests always run.

## Testing

These additions are themselves the tests. Verification:

- `cargo test -p musefs-fuse -- --ignored` runs the new file end-to-end against a
  real mount (requires `/dev/fuse` + libfuse; ffmpeg for the breadth sweep).
- `cargo clippy --all-targets` and `cargo fmt --all --check` must pass (the test
  file compiles under `--all-targets`).
- The hermetic components (b, c) and at least one format in (d) must pass on the
  CI `e2e` runner, which has fuse3 + ffmpeg installed.

## Acceptance criteria

- A randomized, seeded read/seek/mmap consistency sweep runs against a live mount
  and compares against an in-memory oracle, biased toward boundary offsets/lengths
  (#215).
- The sweep spans all ffmpeg-generatable formats, skipping unavailable codecs and
  asserting at least one ran (#215).
- A whole-file `mmap` (`MAP_SHARED`/`PROT_READ`) fidelity test asserts mapped bytes
  equal `pread` bytes on a hermetic FLAC fixture (#214).
- A write-refusal matrix asserts `open(O_WRONLY/O_RDWR)`, `create`, `unlink`,
  `truncate`/`ftruncate`, `mkdir`, and `chmod`/`utimes` each fail (`ret == -1`)
  with an errno in the documented accepted set on a hermetic FLAC fixture (#214).
- No production code changes; no new CI job; one `CONTRIBUTING.md` bullet added.
