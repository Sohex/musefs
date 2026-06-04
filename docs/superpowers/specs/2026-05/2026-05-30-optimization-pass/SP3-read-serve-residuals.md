# SP3 — Read/serve residuals — design

*Date: 2026-06-01 · Part of the [2026-05-30 optimization pass](./README.md)*

## Goal

Clean up the three read/serve-path residuals the prior pass (Phases 1–3) left
behind. None is a new subsystem; each is a small, well-bounded change to an
existing hot path:

1. `read_segments` allocates a throwaway buffer for every backing-audio splice
   (`vec![0u8;n]` → read → `extend_from_slice`), one heap alloc + one memcpy per
   read of the *dominant byte volume* of every served file.
2. `handles` is a single `Mutex<HashMap<u64, Arc<Handle>>>` taken on the read
   fast-path — every `read()` serializes on one lock just to look up its own fd.
3. `size_cache` is a single `Mutex<HashMap<i64, SizeEntry>>` taken on every
   `getattr`/`lookup`.

The header cache was already sharded in the prior pass; these two maps were not.

## Cardinal invariant (preserved by construction)

**Original audio bytes are never copied or modified, and served audio stays
byte-identical.** SP3 changes only *how bytes are buffered* (1) and *how two
in-memory lookup maps are synchronized* (2, 3). It does not touch synthesis, the
layout, the DB schema, or what is served. The byte-identical guarantee holds by
construction; the existing proptests and the `#[ignore]`d FUSE e2e mount tests
are the hard gate.

## Changes

### 1. `read_segments` — eliminate the backing-audio double-allocation

`musefs-core/src/reader.rs`, the `Segment::BackingAudio` arm of `read_segments`.

Today:

```rust
Segment::BackingAudio { offset: bo, .. } => {
    let f = file.expect("backing segment requires an open backing file");
    let mut buf = vec![0u8; n];
    f.read_exact_at(&mut buf, bo + within)?;
    crate::metrics::on_pread(n as u64);
    out.extend_from_slice(&buf);
}
```

New — read directly into `out`'s reserved tail:

```rust
Segment::BackingAudio { offset: bo, .. } => {
    let f = file.expect("backing segment requires an open backing file");
    let start = out.len();
    out.resize(start + n, 0);
    f.read_exact_at(&mut out[start..], bo + within)?;
    crate::metrics::on_pread(n as u64);
}
```

`out` is allocated with `Vec::with_capacity(end - offset)` at the top of
`read_segments`, and the spliced segment lengths sum to exactly `end - offset`, so
the full output capacity is pre-reserved and `resize` never reallocates. Segments
are appended in order, so `start == out.len()` always and `out[start..]` is
exactly the new tail. `read_exact_at` fills all `n` bytes or returns `Err` (no
partial-read concern — the existing `vec![0u8; n]` code already relies on this),
fully overwriting the zero-fill, which is negligible against the `pread` itself.
This removes one heap allocation and one memcpy of up to the FUSE read size
(~128 KiB) per backing-audio splice.

The `OggAudio` arm already serves into `out` via `serve(&index, f, …, &mut out)`
and is unchanged. The art arms (`ArtImage`, `OggArtSlice`) still go through
`db.read_art_chunk`, which returns an owned `Vec`; they are **out of scope** here
(see "Deferred residuals").

No signature change; covered by the existing byte-identical proptests
(`proptest_read_fidelity`, the per-format `proptest_<fmt>`) and the FUSE e2e read.

### 2. `handles` → lock-free `sharded-slab`

`musefs-core/src/facade.rs`. Replace

```rust
handles: Mutex<HashMap<u64, Arc<Handle>>>,
next_fh: AtomicU64,
```

with a single lock-free slab:

```rust
handles: sharded_slab::Slab<Arc<Handle>>,
```

The slab allocates the key, so `next_fh` is **deleted**. The FUSE file handle
(`fh`) is the slab key offset by one:

- `open_handle`: `let key = self.handles.insert(Arc::new(Handle { resolved, file
  })).ok_or(CoreError::HandleTableFull)?;` then return `fh = key as u64 + 1`.
- `read` fast-path: `self.handles.get((fh - 1) as usize).map(|g| (**g).clone())`
  — **lock-free**, no mutex on the hottest serve path. `get` returns a borrow
  *guard*; clone the `Arc` out of it and let the guard drop **before** entering
  `pool.with`, exactly as today's `self.handles().get(&fh).cloned()` drops its
  `MutexGuard` before the pool call (`facade.rs:611`). This preserves the current
  "no in-memory lock held across I/O" property.
- `release_handle`: `self.handles.remove((fh - 1) as usize);`

`Slab` keys are `usize`; the `fh` wire type is `u64`. `key as u64 + 1` is lossless
on all targets, and `(fh - 1) as usize` always round-trips because every `fh` we
ever hand out originated from a real `usize` key (no truncation possible).

Three invariants this preserves:

- **Non-zero `fh`.** This code treats `fh == 0` as "no handle" (`read()` falls
  back to inode resolve). sharded-slab keys start at `0`, so we store `fh = key +
  1` and look up `key = fh - 1`. The `fh != 0` guard in `read()` is unchanged.
- **ABA safety.** sharded-slab encodes a generation in each key, so a removed slot
  that is later reused yields a *different* key. A stale `read` on an
  already-released `fh` returns `None` → the inode fallback path (bounded by the
  entry/attr TTL), never another file's handle. A raw slab index would not give
  this; the generation is why the slab is correct here. (The generation counter is
  finite-width and would alias only after astronomically many reuse cycles of the
  *same* slot — unreachable within real FUSE fd lifetimes, so not defended
  against.)
- **At-capacity is an explicit error, never a panic.** `Slab::insert` returns
  `None` only at the slab's configured capacity. We use sharded-slab's **default
  `Config`**, which is effectively unbounded for FUSE fd lifetimes; an exhausted
  slab surfaces as a new `CoreError::HandleTableFull` rather than `unwrap`.

**New error variant + FUSE mapping.** Add `CoreError::HandleTableFull` to
`musefs-core/src/error.rs` (e.g. `#[error("handle table full")]`). The FUSE
`errno()` mapper (`musefs-fuse/src/lib.rs:64-72`) is an **exhaustive** match over
`CoreError`, so it will not compile without a new arm; add
`CoreError::HandleTableFull => fuser::Errno::ENFILE` (POSIX "file table overflow",
the apt code for a process-wide handle-table exhaustion and distinguishable from
the generic `EIO` bucket). `next_fh` is removed; delete `AtomicU64` from the
`use std::sync::atomic::{…}` import in `facade.rs` (`AtomicBool`/`AtomicI64`/
`Ordering` remain in use) so the change lands warning-clean under the workspace's
`clippy::pedantic`.

### 3. `size_cache` → `DashMap`

`musefs-core/src/facade.rs`. Replace

```rust
size_cache: Mutex<HashMap<i64, SizeEntry>>,
```

with

```rust
size_cache: dashmap::DashMap<i64, SizeEntry>,
```

`SizeEntry` is `Copy`, so:

- `getattr` hit/miss: `self.size_cache.get(&track_id).map(|e| *e)` — the `*e`
  copies the entry out and **drops the read guard immediately**; on a miss,
  `self.size_cache.insert(track_id, SizeEntry { … })`. The drop-before-insert is
  load-bearing: DashMap's `get` returns a `Ref` holding that key's *shard* guard,
  and `insert` re-locks the same shard. The naive translation of today's
  `.get(&track_id).copied()` that held the `Ref` across the `insert` would
  deadlock on the same-shard re-entrancy; copying out first avoids it.
- `poll_refresh` prune: `self.size_cache.retain(|k, _| live.contains(k));` —
  `retain` locks every shard in turn, so it must not run while any `Ref`/`RefMut`
  into the map is held (it isn't: the prune at `facade.rs:435` runs outside any
  `get`).

The map stays unbounded-but-tiny by design (one small `Copy` entry per live
track, self-invalidating on `content_version`); DashMap's internal sharding
removes the single-mutex contention without a hand-rolled LRU.

### Accessors and lock-order comment

Delete the `fn handles()` and `fn size_cache()` `MutexGuard` accessor helpers
(call sites use the slab / DashMap directly). Update the lock-order comment block
in `facade.rs:340-348` to tell the truth:

- `handles` becomes a lock-free slab; its `get` guard is dropped before any
  `pool.with` call (as today), so it never participates in lock ordering.
- `size_cache` becomes a `DashMap`. Note it is **still accessed inside the
  `pool.with` closure** in `getattr` (`facade.rs:563,574`) — that did not change.
  This is safe because each DashMap op takes and releases its own shard guard
  independently (the `*e` copy drops the read guard before the `insert`), so no
  guard is held across a DB call and there is no cross-lock cycle. The two
  same-shard hazards above (`get` guard not held across `insert`; no `Ref` held
  across `retain`) are the only ordering rules these maps impose.
- The pool-first rule for the *actual* remaining in-memory locks (`inodes`,
  header-cache shards) is unchanged, with `inodes`-held-inside-`pool.with` during
  `refresh` still the one stated exception.

### Dependencies

Add to `musefs-core/Cargo.toml`:

- `sharded-slab` (lock-free slab; backs `tracing`)
- `dashmap` (sharded concurrent map)

Consistent with the crate already taking `arc-swap`, `im`, and `once_cell`.

## Validation

- **Functional gate (hard):** full workspace `cargo test` green, plus
  `cargo test -p musefs-fuse -- --ignored` (real-mount byte-identical e2e) green.
  The byte-identical audio round-trip is the non-negotiable gate. The
  format-layer byte-identical proptests need the `fuzzing` feature, so pin the
  exact commands: `cargo test -p musefs-core --test proptest_read_fidelity` and
  `cargo test -p musefs-format --features fuzzing`.
- **Concurrency correctness:** the existing facade tests already cover the changed
  paths (`open_handle_read_and_release_roundtrip`,
  `open_handle_returns_distinct_ids_and_rejects_dirs` — asserts only `!= 0` +
  distinctness, survives the slab change, `release_handle_forces_fallback_on_next_read`,
  `getattr_reresolves_size_after_content_version_bump`,
  `poll_refresh_keeps_unchanged_entries_and_prunes_vanished`). Add two tests for
  paths the slab newly makes relevant: (a) a **released-`fh` ABA fallback** test
  (read on a removed handle returns `None` from the slab → inode fallback serves
  correct bytes), and (b) a **`HandleTableFull`** test exercising the at-capacity
  insert error → `ENFILE` mapping (constructible by configuring/forcing a tiny
  slab capacity in the test, or asserting the error variant directly).
- **Contention signal (the SP3 win):** SP0's `concurrent_read_walk` Criterion
  bench — its comments already name `handles`/`size_cache` mutex contention as
  the SP3 target. Record before/after medians.
- **Regression gate:** the `ci` `sequential_read` Criterion median must not rise
  **>10 %** run-over-run on the same machine (README convention). The alloc fix
  should, if anything, improve it.
- Record before/after numbers in the README results log and `BENCHMARKS.md`.

## Deferred residuals (explicitly considered, not in SP3)

Recorded here and in the tracking README so they are not silently dropped:

- **Art-chunk zero-copy (Option 2).** Add a `read_art_chunk_into(&mut buf, …)`
  variant in `musefs-db` so the `ArtImage`/`OggArtSlice` arms stop allocating an
  owned `Vec` per read. Deferred: art is a small fraction of served bytes versus
  audio (the audio fix captures the dominant case); it touches the db crate's
  public API (a layer below core, used elsewhere); and `OggArtSlice`
  base64-re-encodes, so it only partially applies. Worth doing only if an
  art-heavy profile later shows it hot.
- **Zero-copy into the FUSE reply buffer (Option 3).** Eliminate the per-read
  `out: Vec` itself by serving into a reusable per-worker buffer across the
  core↔fuse boundary. Deferred: `read_at`/`read_segments` return an owned `Vec`
  today — a clean, heavily-tested interface — and caller-provided buffers change
  signatures throughout and break the many tests asserting on returned `Vec`s;
  per-worker buffer lifecycle adds real complexity; and `reply.data(&[u8])` still
  borrows a slice we own, so the win is reuse, not true kernel zero-copy. A
  candidate for its own SP if read-IOPS allocation pressure ever shows up in a
  profile.

## Out of scope (YAGNI)

- The header cache (already sharded in the prior pass).
- The Ogg first-read whole-region index scan (that is SP4).
- Options 2 and 3 above.
