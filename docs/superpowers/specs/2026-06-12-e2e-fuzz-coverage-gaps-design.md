# Design: close the #280 e2e/fuzz coverage gaps (#320, #306, #313)

## Summary

Three independent test/fuzz coverage gaps surfaced by the #280 audit, landed
together on the `coverage` branch as one cohesive effort. None is a live
production defect — each is missing or stale **test coverage** of an existing,
correct invariant:

- **#320** — `musefs/tests/sigterm_unmount.rs` asserts a path the current
  default template no longer produces, so both e2e tests fail whenever actually
  run; and the `musefs` / `musefs-latencyfs` binary-level `--ignored` e2e tests
  never run in CI, so the staleness was undetectable.
- **#306** — the read-only refusal e2e (`read_consistency.rs`) probes a subset
  of mutating syscalls; whole families (`rename`, `rmdir`, `symlink`, `chown`,
  xattr mutators, `mknod`/`link`) are uncovered.
- **#313** — the `serve` fuzz target only builds well-formed DB rows via the
  public API; it never fuzzes hostile/corrupt rows, never streams a
  `Segment::BinaryTag`, and routes three selector values to a single Opus
  fixture.

The guiding invariant for the whole repo is unchanged: **original audio bytes
are never copied or modified**, and the SQLite store is the external-writer
contract. This work hardens the *tests* that guard that contract.

## Non-goals

- No production behavior change. The musefs binary, FUSE callbacks, read/
  synthesis paths, and DB schema are not modified (except the additive,
  feature-gated DB accessor in #313, which is compiled only for the
  out-of-workspace fuzz crate).
- No new `EROFS`-exact assertions: the read-only contract stays "mutation
  refused", accepting any errno that proves no write occurred.
- The malformed-FLAC-structural fuzz angle is **explicitly skipped** (see
  Section C / Skips).

---

## Section A — #320: stale sigterm tests + CI wiring

### Problem

Both tests in `musefs/tests/sigterm_unmount.rs`
(`sigterm_unmounts_cleanly`, `sigterm_exits_bounded_when_mount_is_busy`) build a
FLAC fixture tagged only `ARTIST=Alice` / `TITLE=Song`, mount via the real
binary with **no `--template`**, and assert the synthesized file at
`Alice/Song.flac`. The default template is now `$albumartist/$album/$title`
with `--default-fallback Unknown`, so the file actually renders at
`Unknown/Unknown/Song.flac`. The asserted path never appears, and both tests
panic `mount did not come up` at the 15 s poll. CI never runs
`cargo test -p musefs -- --ignored`, so this was invisible there.

### Fix — tests

Pass an explicit `--template '$artist/$title'` to the `mount` command in **both**
tests. The fixture stays `ARTIST=Alice` / `TITLE=Song`; the asserted path
`Alice/Song.flac` is produced deterministically and is immune to future
default-template changes (the exact failure mode that rotted these tests).

Concretely, each test's mount invocation changes from:

```rust
.args(["mount", mp.path().to_str().unwrap(), "--db", db])
```

to add `"--template", "$artist/$title"`. No fixture, assertion-path, or SIGTERM
logic changes.

### Fix — CI

In `.github/workflows/ci.yml`, the `e2e` job currently runs only
`musefs-fuse` ignored tests. Add two steps:

- `cargo test -p musefs -- --ignored`
- `cargo test -p musefs-latencyfs -- --ignored`

so binary-level and latencyfs e2e cannot silently rot again. (`musefs-cli` has
no ignored tests, so it is not wired.)

### Contingency

`musefs-latencyfs` has its own ignored e2e (`latency_effect`, `passthrough`,
`sqlite_wal`) that also have never run in CI and may themselves be stale. Before
wiring them, run both suites locally on this `/dev/fuse` host. If a latencyfs
test is stale in the same trivial way (drifted default/template/path), fix it
in-scope. If a failure reveals something larger, do **not** wire a red test —
surface it as a separate finding and wire only the green suites.

---

## Section B — #306: expand read-only refusal coverage

### Problem

The mount is `MountOption::RO` and `MusefsFs` implements no mutating callbacks,
so no write-through defect exists. But the ignored e2e
`write_ops_are_refused_on_read_only_mount` only probes `open(O_WRONLY)`,
`open(O_RDWR)`, `open(O_CREAT)`, `unlink`, `truncate`, `ftruncate`, `mkdir`,
`chmod`, `utimes`. Whole mutating-syscall families are unprobed, so a future
platform change or a later callback could weaken the read-only contract without
a test catching it.

### Design

Extend the existing `write_ops_are_refused_on_read_only_mount` test (do not add
a parallel test). Reuse the existing `assert_refused(ret, accepted, what)`
helper and its "mutation refused, not exactly `EROFS`" contract.

**New mutating syscalls** — each asserted refused with a broad "no write
happened" errno set:

| Syscall | Target | Accepted errnos |
|---------|--------|-----------------|
| `rename` | existing served file → new name in `Alice/` | `EROFS`, `EPERM`, `EACCES` |
| `rmdir` | a virtual dir (e.g. `Alice`) | `EROFS`, `EPERM`, `EACCES`, `ENOTEMPTY` |
| `symlink` | new link in `Alice/` | `EROFS`, `EPERM`, `EACCES` |
| `chown` | existing served file | `EROFS`, `EPERM`, `EACCES` |
| `lchown` | existing served file | `EROFS`, `EPERM`, `EACCES` |
| `setxattr` | existing served file | `EROFS`, `EPERM`, `EACCES`, `ENOTSUP` |
| `removexattr` | existing served file | `EROFS`, `EPERM`, `EACCES`, `ENOTSUP`, `ENODATA` |
| `mknod` | new regular file (`S_IFREG`, mode `0o644` — no privilege) | `EROFS`, `EPERM`, `EACCES`, `ENOSYS` |
| `link` | existing served file → new name | `EROFS`, `EPERM`, `EACCES`, `ENOSYS` |

`ENOSYS` tolerance covers the issue's "(where available)" caveat: if the
platform/FUSE build does not implement the callback at all, the syscall still
did not mutate, which satisfies the contract.

**Read probes** (`getxattr`, `listxattr`) — these are **not** mutations.
Asserting they are "refused" would be a bug. Exercise them against an existing
served file and assert **read-safety** instead: the call either succeeds
(returns `>= 0`) or fails with `ENOTSUP` / `ENODATA` (no xattr support / no such
attr). Add a small `assert_read_safe(ret, accepted_errnos, what)` helper rather
than overloading `assert_refused`.

All raw libc calls stay inside the existing single `unsafe` block under the
existing `#[expect(unsafe_code, …)]`; extend the `reason` text if needed.

---

## Section C — #313: expand serve fuzzing

### Problem

`fuzz/fuzz_targets/serve.rs` reaches the real core read path, but its DB state is
built only through the public, validating API (`upsert_track`, `replace_tags`,
`upsert_art`, `set_track_art`). It therefore never fuzzes rows that can exist
only through a hostile SQLite writer or corruption, never calls
`set_binary_tags` (so `Segment::BinaryTag` / `read_binary_tag_chunk_into` are
unreached by fuzzed read windows), and routes selector values 4–6 all to the
same `ogg_opus()` fixture.

### Design — feature-gated raw DB accessor

Add a `fuzzing` cargo feature to `musefs-db`, mirroring the existing
`musefs-format` `fuzzing` feature and `musefs-db`'s own test-only `mutants`
feature (both "named after the activity that needs it … not for production
use"). Off by default; enabled only by the out-of-workspace fuzz crate.

Under the feature, expose a raw-connection escape hatch on `Db`, e.g.:

```rust
#[cfg(feature = "fuzzing")]
impl Db {
    /// TEST/FUZZ ONLY. Hands the raw rusqlite connection to `f` so fuzz
    /// harnesses can plant rows the validating public API cannot produce
    /// (e.g. under `PRAGMA ignore_check_constraints`). Never compiled in
    /// production: the `fuzzing` feature is enabled only by the fuzz crate,
    /// which is outside the cargo workspace.
    pub fn with_raw_conn<R>(&self, f: impl FnOnce(&rusqlite::Connection) -> R) -> R { … }
}
```

The doc-comment must make the test/fuzz-only contract unmistakable. Because the
fuzz crate is out-of-workspace, the feature never unifies into normal
`cargo build` / `test` / `clippy`; the gated code is compiled and linted only by
CI's `cargo +nightly fuzz build` smoke — the same exposure profile as the
existing format `fuzzing` helpers.

In `fuzz/Cargo.toml`, change `musefs-db = { path = "../musefs-db" }` to enable
`features = ["fuzzing"]`.

### Design — hostile-row stage in `serve.rs`

After the existing well-formed setup, add a **fuzzer-gated** hostility stage:
the fuzzer draws a selector deciding whether (and which) hostile mutation(s) to
apply via `with_raw_conn`. One corpus thus reaches both the well-formed and the
corrupt states. Categories (all via parameterized statements, using
`PRAGMA ignore_check_constraints` where a CHECK would otherwise block the write):

1. **Negative / oversized integers** — `UPDATE tracks SET audio_offset / audio_length / backing_size / backing_mtime_ns / backing_ctime_ns = <fuzzer i64, incl. negative & i64::MAX>`; likewise art `width`/`height`. Stresses `i64`→`u64`/`usize` model conversion before any read.
2. **Constraint-bypassed `tracks` rows** — invalid `format` discriminant / impossible geometry planted with checks disabled. Stresses model deserialization and format dispatch.
3. **Orphaned / missing art** — `track_art.art_id` pointed at a non-existent `art` row (or the `art` row deleted). Stresses `ArtImage` / `OggArtSlice` resolve + stream.
4. **Oversized text / blob** — a multi-KB+ `mime` string or art blob whose safe rejection must happen before materialization.
5. **Stale / reused `tags.rowid` binary handles** — set binary tags, resolve (capturing the layout/handles), then delete or replace the binary-tag row, then read. Stresses `read_binary_tag_chunk_into`'s missing/stale-handle path.
6. **content-version / backing-geometry mismatch** — `backing_size` / `backing_mtime_ns` / `content_version` set to disagree with the real backing file. Stresses the freshness/staleness check.

### Design — binary-tag streaming

When the chosen format supports binary tags and the fuzzer opts in, call
`db.set_binary_tags(id, &[…])` with fuzzer-chosen key/mime/payload so a
`Segment::BinaryTag` is materialized and the existing draw-up-to-8 read windows
exercise `read_binary_tag_chunk_into`. (This composes with hostile category 5,
which then corrupts the handle the windows read through.)

### Design — distinct Ogg fixtures

Add `ogg_vorbis()` and `ogg_flac()` to `musefs-format::fuzz_check::fixtures`,
built from the existing `ogg::page_test_support::{build_header_pub,
lace_packet_pub}` helpers, mirroring `ogg_opus()` (Vorbis identification header
`\x01vorbis…`; OggFLAC mapping header `\x7FFLAC…`). Split the `serve` selector so
4 → Opus, 5 → Vorbis, 6 → OggFLAC, giving the shared
`Format::Opus | Vorbis | OggFlac` Ogg branch distinct inputs.

### Assertion discipline (critical)

The splice-consistency invariants — whole-read length `== total_len`, and each
window equals the clamped slice of the whole read — hold **only on the clean,
non-hostile path**. When the fuzzer has applied a hostile mutation:

- `HeaderCache::resolve` returning `Err`, or a read returning `Err`, is
  **acceptable and expected** — `return` early, do not assert.
- The only contract on the hostile path is **no panic / no UB / no
  out-of-bounds**. Reads that *do* succeed must still not read or copy audio
  bytes outside `[offset, total)` (the existing clamp invariant), but length
  equality against a "whole read" computed from a corrupt layout is not
  asserted.

Structurally: compute and assert the splice invariants before applying any
hostile mutation (or guard them behind `if !hostile_applied`).

### Skips (explicit, per "no silent caps")

The issue's 7th hostile category — **"malformed structural blocks for the FLAC
fast-path synthesis"** — is **not** implemented here. It is *backing-byte*
corruption, not a hostile DB row, and that surface is already the entire job of
the dedicated `flac` fuzz target. Folding it into `serve` would add only the
"corrupt structure reached at synthesis read-time" angle at meaningful overlap.
The spec records the skip so it is visible; revisit only if read-time FLAC
structural coverage is later judged worth the duplication.

---

## Testing & verification

- **#320**: run `cargo test -p musefs --test sigterm_unmount -- --ignored
  --test-threads=1` and `cargo test -p musefs-latencyfs -- --ignored` locally on
  this `/dev/fuse` host; both must pass before the CI steps are wired (contingency
  above).
- **#306**: run `cargo test -p musefs-fuse --test read_consistency -- --ignored`
  locally; the extended test must pass on `/dev/fuse`.
- **#313**: `cargo +nightly fuzz build serve` (the fuzz crate is out-of-workspace,
  so workspace build/test/clippy will not catch breakage in the target or the new
  feature-gated accessor). A short local `cargo +nightly fuzz run serve` smoke to
  confirm the hostile stage neither panics on clean inputs nor trivially crashes.
- Full workspace suite via the pre-commit hook (fmt, clippy `-D warnings`, all
  tests). The new `fuzzing`-gated `musefs-db` code is not compiled by the
  workspace; ensure `musefs-db` still builds/lints with the feature off (default)
  and that the fuzz build exercises it on.

## Documentation

- `CONTRIBUTING.md`: if the new `musefs-db/fuzzing` feature changes the documented
  fuzz build invocation, update the coverage-guided-fuzzing section; note the new
  `serve` hostile-row / binary-tag scope alongside the existing fuzz-target list.
- Note the two new `e2e`-job steps where the CI e2e job / FUSE e2e tiers are
  described (CONTRIBUTING.md test-tiers).

## Commit shape (TDD-aware)

The pre-commit hook rejects any commit with red workspace tests, so each commit
must be green:

1. #320 test fix + CI wiring (after local verification of latencyfs).
2. #306 read-only test extension.
3. #313 in two parts: (a) `musefs-db` `fuzzing` accessor + `fuzz/Cargo.toml`
   feature + `ogg_vorbis`/`ogg_flac` fixtures (workspace stays green; fuzz build
   verifies); (b) `serve.rs` hostile stage + binary-tag + selector split.

Ordering within the branch is flexible since the three issues are independent;
each lands as its own green commit.
