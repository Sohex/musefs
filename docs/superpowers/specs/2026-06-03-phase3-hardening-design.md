# Phase 3 — Safety net + small Rust hardening

**Date:** 2026-06-03
**Scope:** Roadmap "Phase 3 — Safety net + small Rust hardening" — issues #88,
#91, #92, #93, #94. Low-risk, independent fixes. Ships as **one batched PR**
(matching how Phases 0–2 each landed).

## Goal

Close five latent-hardening issues surfaced by the v1 multi-model review and the
fuzz triage. None are user-visible behavior changes for well-formed inputs; each
removes a way a malformed input, a malformed external DB row, or a future caller
could cause an OOM, a silent overflow, a panic, or an unhelpful error. The
byte-identical-audio invariant is untouched.

---

## #88 — Ogg fuzz target does not exercise art synthesis

**Problem.** `fuzz/fuzz_targets/ogg.rs` calls
`ogg::synthesize_layout(&header, …, &tags, &[])` with an empty art slice. The
flac/mp3/mp4/wav targets all pass `arb_arts(&mut u)`. Ogg's art path — page
renumber + per-page CRC recompute + incremental base64 windowing — is the most
complex art synthesis of any format and is never reached by the fuzzer.

**Fix (NOT a one-liner — ogg's signature differs from flac/mp3).** flac/mp3 take
`arts: &[ArtInput]`, so they pass `arb_arts(&mut u)` directly. Ogg's
`synthesize_layout` instead takes `arts: &[OggArt]`
(`musefs-format/src/ogg/mod.rs:240`), where
`OggArt<'a> { meta: &'a ArtInput, image: &'a [u8] }` carries the **raw image
bytes** (ogg needs them to recompute per-page CRCs — `arb_arts` carries only a
`data_len`, no bytes). The real caller (`reader.rs:347`) zips `art_inputs` with
the stored image blobs, so the held invariant is `image.len() == meta.data_len`.

The fuzz target must therefore, locally (not in `arb_arts`, because of the
borrow lifetimes):
1. Generate a small vec (0..=2) of arbitrary image byte buffers — `Vec<Vec<u8>>`,
   each bounded (≤ ~8 KiB, matching `arb_arts`'s `data_len` bound) — and a
   parallel `Vec<ArtInput>` whose `data_len == bytes.len()` (with arbitrary
   `mime`/`picture_type`/dims as in `arb_arts`). The byte buffers and the
   `ArtInput`s must outlive the `OggArt` slice.
2. Build `let arts: Vec<OggArt> = inputs.iter().zip(images.iter()).map(|(meta, img)|
   OggArt { meta, image: img.as_slice() }).collect();`
3. Pass `&arts` as the 5th argument to `synthesize_layout`.

This exercises the page-renumber + CRC-recompute + incremental-base64 art path
the empty slice never reached.

**Verification.** The fuzz crate is out-of-workspace, so a normal
`cargo build`/`clippy` will *not* compile it — only CI's smoke job would catch a
break. Verify locally with `cargo +nightly fuzz build ogg`, then a short
`cargo +nightly fuzz run ogg -- -runs=100000` to confirm the art path is reached
and stays panic-free.

---

## #93 — `byte_budget` increment is non-saturating while its guard saturates

**Problem.** In `musefs-core/src/byte_budget.rs::acquire`, the wait guard tests
`in_flight.saturating_add(n) > self.cap`, but the state mutation is a plain
`*in_flight += n`. `release` already uses `saturating_sub`. The two disagree on
overflow; the existing comment even claims the saturating style is mirrored.

**Fix.** Change `*in_flight += n` to `*in_flight = in_flight.saturating_add(n)`.
No observable change (art weights are file-bounded; `in_flight` never approaches
`u64::MAX`), purely an internal-consistency fix.

**Verification.** The existing `byte_budget` unit tests (which pin additive
accumulation and every guard mutant) continue to pass unchanged — `saturating_add`
is still additive below the saturation point. No new test; the asymmetry is not
reachably observable.

---

## #92 — `mapping.rs` casts `byte_len as u64` without a non-negative guard

**Problem.** `musefs-core/src/mapping.rs::track_art_to_inputs` builds
`data_len: meta.byte_len as u64` (line 45) from `art.byte_len`, an `i64` **stored
column** (`art.byte_len INTEGER NOT NULL`, `schema.rs:31`). The SQLite store is
the documented contract external tools write to; a negative `byte_len` from a
malformed/external row casts to a huge `u64` (e.g. `-1 → u64::MAX`), which would
drive a bogus segment length.

**Scope — art only, NOT binary tags.** The sibling cast `len: row.byte_len as u64`
in `binary_tags_to_inputs` (line 63) is **safe and needs no guard**: that
`byte_len` is computed as `length(value_blob)` in SQL (`tags.rs:127`), which is
always ≥ 0 — a negative value is unrepresentable, so a guard there would be
unreachable defensive code. Only the stored `art.byte_len` column is externally
forgeable into a negative.

**Fix.** **Skip the malformed art row** rather than clamp. A negative `byte_len`
is a contract violation for that single row; dropping it lets the track still
synthesize without that art (graceful degradation). Clamping to `0` was rejected:
a `len: 0` `ArtImage` fails layout validation downstream, which would make the
*whole track* unreadable instead of just losing the bad row.

- `track_art_to_inputs`: inside the existing `if let Some(meta)` arm, skip the
  push (`continue`) when `meta.byte_len < 0`, so the row is filtered out of
  `inputs`.

**Verification.** New unit test in `mapping.rs`. The negative `byte_len` cannot be
produced through the public `Db` API (`upsert_art` derives `byte_len` from
`data.len()`), so the test simulates an external malformed write: create a
**file-backed** `Db`, upsert a track + a valid art row + `set_track_art`, then open
a second raw `rusqlite::Connection` to the same path and
`UPDATE art SET byte_len = -1`. `rusqlite` is already a musefs-core dev-dependency.
Assert `track_art_to_inputs` omits the corrupted row while keeping a well-formed
sibling art row.

---

## #91 — MP4 `moov`/`ftyp` bytes read with no metadata-size cap

**Problem.** `musefs-format/src/mp4.rs::read_structure_from`'s `region()` helper
allocates `vec![0u8; len]` for the `ftyp`/`moov` boxes, where `len` is the
declared box length. `box_header` (`mp4.rs:59`) already rejects
`total_len > remaining` (`remaining = file_len - pos`), so the allocation is
bounded by the file's real length — but for a genuinely large file (a real
multi-hundred-MB audiobook, or a backing file whose `moov` is corrupt-but-large
within a large file) there is still no upper bound comparable to
`mp3.rs::id3v2_alloc_safe`: a real 600 MB `moov` forces a 600 MB allocation. This
path is shared by scan **and** serve-time resolve (`reader.rs:320`; Phase 6 made
resolve seek the `moov`), so the cap guards both.

**Fix.** Add a `const MAX_MP4_METADATA_BYTES: u64 = 256 * 1024 * 1024;` (256 MiB).
Generous headroom for extreme audiobooks (tens of hours → tens of MB of sample
tables) while rejecting corrupt multi-GB boxes. Before the `ftyp_bytes`/
`moov_bytes` reads in `read_structure_from`, reject when either box's declared
`total_len` exceeds the cap, returning a new distinct error variant:

```rust
#[derive(Debug, thiserror::Error)]
pub enum Mp4ScanError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Format(#[from] FormatError),
    #[error("MP4 {box_kind} box is {size} bytes, exceeds the {cap}-byte metadata cap")]
    MetadataTooLarge { box_kind: &'static str, size: u64, cap: u64 },
}
```

The check runs **before** allocation (on the declared `total_len`), so a corrupt
header never allocates.

**Clear logging (per requirement).** `scan.rs:221` currently swallows the error
silently (`let Ok(scan) = … else { return Ok(None) }`). Replace that with a match
that, on `Mp4ScanError::MetadataTooLarge`, emits
`log::warn!("skipping {path}: {e}")` (naming the file, box, size, and cap) before
returning `Ok(None)`; all other errors keep the current silent-skip behavior so
this change is scoped to the new case only. This is the first scan-site use of
`log::warn!`, consistent with the existing `log::warn!`/`error!` usage in
`facade.rs`/`lock.rs`.

**Verification.** New `mp4.rs` unit test. Note `box_header`'s `total_len > remaining`
check would intercept a huge box over a small file as `Malformed` *before* the new
cap, so the test must exploit that `file_len` is a **parameter** of
`read_structure_from` independent of the reader's real length: pass a large
`file_len` (so `remaining` clears `box_header`) over a short in-memory cursor whose
`moov` header declares `total_len` > 256 MiB. The cap check fires before `region`'s
`read_exact`, so the result is `MetadataTooLarge` with **no** giant allocation and
no EOF. A boundary test (box size == cap exactly) confirms the guard is strict
`>` (it proceeds to `region`, hitting `Io`/EOF on the short buffer — *not*
`MetadataTooLarge`). The scan-site `log::warn!` branch is reachable only with a
genuinely >256 MiB file (scan passes the file's *real* length, so a small fixture
hits `box_header`'s `Malformed` first) — verified by inspection plus the unaffected
existing MP4 scan tests, not a multi-hundred-MB fixture.

---

## #94 — DbPool thread-local lifecycle footguns

Three latent issues in `musefs-core/src/db_pool.rs`'s `PerThread` path. Decision:
fix the two cheap/real ones; document-and-accept the third (genuinely unreachable
under our thread model, and only fixable with a cross-thread registry we don't
want to add for a case that can't currently occur).

**(a) Re-entrant `with()` panic — FIX.** `with()` holds
`PER_PATH.borrow_mut()` across the user closure `f`. A re-entrant `with()` on the
same thread (the natural "second query inside a closure" pattern) panics on the
second `borrow_mut` with an opaque `BorrowMutError`. Not reachable today (every
closure is a single leaf read), but a sharp landmine for future callers and cheap
to disarm.

*Fix:* the `PerThread` connections live in the `PER_PATH` thread-local, typed
`HashMap<(PathBuf, u64), Db>`. Change the value type to `Rc<Db>`
(`HashMap<(PathBuf, u64), Rc<Db>>`). In `with`, clone the `Rc` out **while
holding the borrow**, drop the borrow, then call `f(&db)`. The borrow no longer
spans `f`, so re-entrancy is safe. `Rc` (not `Arc`) — the map is thread-local.
`Db` is only ever borrowed as `&Db` through the pool (no `&mut`/ownership), so
`Rc<Db>` is sufficient and behavior-preserving for all current callers.

*Scope — `PerThread` only.* The same re-entrancy hazard exists on the `Shared`
variant (`DbPool::Shared(Arc<Mutex<Db>>)`), but there it is a **mutex deadlock**
on the second `m.lock()`, not a `RefCell` panic, and the `Rc` fix does not touch
it. `Shared` is the in-memory test-only fallback (a real mount is always
`PerThread`), and the module already carries a doc comment warning about re-entry.
We **document-and-accept** the `Shared` deadlock (extend that warning); we do not
fix it. The re-entrancy test (below) therefore runs against a **file-backed
(`PerThread`) temp DB** only — running it against a `Shared` pool would deadlock by
design.

**(b) Open error lacks path context — FIX.** `Db::open_readonly(path)` returns
`Result<Db, DbError>`; at the `with` site the `?` converts via
`CoreError::Db(#[from] DbError)`, which is `#[error(transparent)]`, so *which* DB
path failed is structurally lost. An open failure is a real reachable runtime path
(permissions, deleted/corrupt DB under a live mount). Honoring the CLAUDE.md
convention (no `map_err` that drops the source) while surfacing the path requires a
**new typed `CoreError` variant** — `#[from]` alone cannot carry the path:

```rust
#[error("failed to open database at {path}")]
DbOpen { path: PathBuf, #[source] source: DbError },
```

At the open call site in `with`, map the open error into `DbOpen { path:
path.clone(), source }` (carrying, not dropping, the source). The `{path}`
interpolation makes the verification's "`Display` contains the path" hold while the
typed `#[source]` preserves the underlying `DbError`.

**(c) Cross-thread Drop leak — DOCUMENT + ACCEPT.** `Drop` clears only the
dropping thread's thread-local entry; connections opened on other worker threads
persist until those threads exit. Under a FUSE mount the worker pool lives for the
whole mount and is torn down at unmount, so a connection's lifetime already
matches its thread's — the "leak" is unreachable. It would only bite a future
caller creating/dropping many `DbPool`s over long-lived shared threads. Closing it
properly needs a cross-thread connection registry; not worth that machinery for an
unreachable case. Record the limitation in a doc comment on `impl Drop for DbPool`
so a future caller who *does* hit that pattern is warned.

**Verification.** New `db_pool.rs` tests (file-backed temp DB):
- re-entrancy: `pool.with(|_| pool.with(|_| Ok(())))` does not panic and returns
  the inner result.
- path context: opening a nonexistent/unreadable path yields an error whose
  `Display` contains the path string.

---

## Cross-cutting verification

- `cargo test --workspace`, `cargo clippy --all-targets`, `cargo fmt --all --check`
  (the CI fmt gate is a pre-push must — see prepush-checks note).
- `cargo +nightly fuzz build ogg` (+ short run) for #88, since the fuzz crate is
  out of the workspace.
- In-diff mutation gate locally before push (`-j$(nproc)`, `TMPDIR` under /home),
  sanity-checking the diff so it isn't a silent false pass — `main` is protected
  by the `ci-ok` aggregator which includes the mutation gate.
- The `#[ignore]` FUSE e2e suite is unaffected (no read/synthesis-path semantics
  change), but run it once on `/dev/fuse` as a regression check before merge.

## Out of scope

- No cross-thread DbPool registry (see #94c).
- No fix for `Shared`-variant re-entrancy (mutex deadlock); documented-and-accepted
  alongside #94a since real mounts are always `PerThread`.
- No change to the existing oversized-art silent-skip path in scan (#91 only adds
  logging for the new MP4 metadata-cap case).
- No new format support, no serve-path semantics change.
