# Serve-time front-read cap for hostile stored audio offsets

Issue: [#265](https://github.com/Sohex/musefs/issues/265)
Date: 2026-06-12

## Problem

The serving path trusts `tracks.audio_offset` enough to allocate that many
bytes when reconstructing headers. `read_front` (`musefs-core/src/reader.rs`)
does `vec![0u8; usize_from(n)]` with `n = track.bounds.audio_offset()` before
reading. A hostile SQLite writer can insert a valid-looking `tracks` row with a
very large `audio_offset` and a matching huge/sparse backing file, then trigger
`getattr`, `open`, or `read` to force that allocation before a controlled error
is returned — a memory-exhaustion vector.

The DB schema (`musefs-db/src/schema.rs`) only enforces nonnegative bounds and
`audio_offset + audio_length <= backing_size`. The scanner's `MAX_PROBE_BYTES`
(64 MiB) cap is **not** enforced at serve time, so a direct external writer
bypasses it.

### Threat model (refined by existing guards)

Two pre-existing guards in `reader.rs` bound the attack before `read_front` is
reached, so the precise residual capability the cap closes is narrower than
"insert a row and read":

- `resolve` (`reader.rs:119`) rejects unless `meta.len() == track.backing_size`.
- `build` (Synthesis, `reader.rs:156-163`) rejects unless
  `audio_offset + audio_length <= meta.len()`.

Chained with the schema constraint, reaching `read_front` with
`audio_offset > MAX_PROBE_BYTES` therefore requires a **real backing file at
least `audio_offset + audio_length` bytes long** whose size matches
`backing_size`. The realistic attack: a multi-GB **sparse** file (cheap — no
disk used) plus a row pointing `audio_offset` deep into it, forcing a
multi-GB *contiguous* front allocation. The cap bounds that single allocation
to 64 MiB even when such a validly-sized large file exists — it is the only
thing standing between the guards and an unbounded `Vec` allocation.

## Invariant being protected

The scanner refuses to ingest a file whose parseable metadata does not appear
within the first `MAX_PROBE_BYTES` (64 MiB). Therefore every legitimately
scanned file of a front-read format has `audio_offset <= MAX_PROBE_BYTES`.
External writers own tags and art per the store contract — they do **not** own
`audio_offset` (a scanner-owned field). A serve-time front read above the cap
can only originate from a hostile or contract-violating row.

## Scope

`read_front` is the only serve-time allocator keyed on `audio_offset`. All three
vulnerable header-build paths funnel through it:

- FLAC legacy fallback (`reader.rs`, when no structural rows exist)
- WAV (`reader.rs`)
- Ogg: Opus / Vorbis / OggFlac (`reader.rs`)

Out of scope (already bounded, not changed):

- MP4 streams its structure with its own cap (`Mp4MetadataTooLarge`).
- Art payloads are bounded by `ArtTooLarge`.
- The FLAC structural fast-path streams blocks from the DB; it does not
  `read_front`.
- The issue's longer-term suggestion ("bounded structural validation over
  trusting scanner-owned DB fields") is deferred.

## Design

### 1. Shared cap constant

Promote `MAX_PROBE_BYTES` (`musefs-core/src/scan.rs:25`, `64 << 20`) from
private to `pub(crate)` and reference it from `reader.rs`. The serve cap *is*
the scan cap — a single source of truth makes the issue's "aligned with scanner
caps" requirement structural, so the two cannot drift.

### 2. Enforce in `read_front` (single choke point)

Add the cap check **before** any side effect or allocation — before
`metrics::on_open()`, the file open, and the `vec![0u8; n]`:

```rust
fn read_front(path: &Path, n: u64) -> crate::Result<Vec<u8>> {
    use std::io::Read;
    if n > crate::scan::MAX_PROBE_BYTES {
        return Err(CoreError::HeaderTooLarge {
            requested: n,
            cap: crate::scan::MAX_PROBE_BYTES,
        });
    }
    crate::metrics::on_open();
    let mut f = std::fs::File::open(path)?;
    let mut buf = vec![0u8; usize_from(n)];
    f.read_exact(&mut buf)?;
    Ok(buf)
}
```

The return type changes from `std::io::Result<Vec<u8>>` to
`crate::Result<Vec<u8>>` (`CoreError`). The three call sites already use `?`
inside a `CoreError`-returning function and need no edits — the inner I/O still
converts via the existing `std::io::Error` `#[from]`.

Placing the check before `on_open()` means a rejected hostile read never
increments the metrics open-counter, so the exact-count metrics tests are
unaffected.

The cap also retires a latent footgun: `usize_from(n)` is lossless only on
64-bit targets, so bounding `n` to 64 MiB removes any 32-bit truncation risk in
the allocation length.

**Precedence.** The cap check sits *inside* `read_front`, which runs after both
pre-existing guards (`resolve`'s `backing_size`/mtime check and `build`'s
`audio_offset + audio_length <= meta.len()` check). So a row that is both stale
*and* over-cap surfaces as `BackingChanged`, not `HeaderTooLarge`: stale-file
detection deliberately wins, and the cap fires only for an in-bounds-but-
oversized front. This is the single enforcement point by design — no future
non-`read_front` front allocator is anticipated, so no defense-in-depth check is
added at the `resolve`/`build` boundary.

### 3. New error variant → EIO

Add to `CoreError` (`musefs-core/src/error.rs`), mirroring the existing
`Mp4MetadataTooLarge` / `ArtTooLarge` variants:

```rust
#[error("front/header read of {requested} bytes exceeds the {cap}-byte serve cap")]
HeaderTooLarge { requested: u64, cap: u64 },
```

The field is named `requested` (bytes asked of `read_front`), not `audio_offset`
— at every current call site it equals `audio_offset`, but the variant lives in
the generic allocator and should stay honest if a non-`audio_offset` caller is
ever added.

Map it to `EIO` in `musefs-fuse/src/lib.rs` by adding it to the existing
structural-error arm alongside `Mp4MetadataTooLarge` and `ArtTooLarge`. This
fails closed with a controlled `EIO` on `getattr`, `open`, and `read` alike,
since all three reach the shared header-build path.

## Testing

- **`read_front` unit test:** `n > MAX_PROBE_BYTES` returns `HeaderTooLarge`
  before any file open (no backing file required — proves the fail-closed
  ordering ahead of allocation).
- **End-to-end serve test, one per vulnerable path** (FLAC legacy fallback,
  WAV, Ogg): insert a `tracks` row with a hostile `audio_offset >
  MAX_PROBE_BYTES` and drive resolve / `read_at`, asserting `HeaderTooLarge`.
  The fixture must satisfy **both** pre-existing guards to reach `read_front`,
  which pins the sizes:
  - `audio_offset = MAX_PROBE_BYTES + 1`, `audio_length = 1`.
  - sparse backing file of **`MAX_PROBE_BYTES + 2` bytes** (= `audio_offset +
    audio_length`), so `build`'s `audio_offset + audio_length <= meta.len()`
    holds. A file of exactly `MAX_PROBE_BYTES + 1` with positive `audio_length`
    would fail that guard and surface `BackingChanged` instead — so the earlier
    "positive length" repro and an exactly-`+1` file are mutually exclusive.
  - `backing_size = MAX_PROBE_BYTES + 2` to match `meta.len()` (resolve guard).
  - Back the sparse file on tmpfs per the latency-bench storage note; `set_len`
    keeps it sparse so no real 64 MiB is written.
  - **FLAC fixture specifically** must insert **zero** `track_structural_blocks`
    rows so the legacy fallback branch (`rows.is_empty()`) is taken. Because the
    cap fires before `flac::read_metadata`, the garbage/sparse front is never
    parsed — the test asserts `HeaderTooLarge`, not a FLAC `Format` error.
- **errno test:** `errno(&CoreError::HeaderTooLarge { .. }).code() == libc::EIO`
  in `maps_core_errors_to_errno` (`musefs-fuse/src/lib.rs`).

## Non-impacts

- No `musefs-db` schema change, so no Python schema-mirror regeneration.
- No change to the external-writer contract surface.
- No new public API beyond the `CoreError` variant.
