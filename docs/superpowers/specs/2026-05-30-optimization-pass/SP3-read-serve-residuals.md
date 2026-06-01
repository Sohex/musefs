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
`read_segments`, so `resize` never reallocates — the zero-fill is immediately
overwritten by the positioned read and is negligible against the `pread` itself.
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
  })).ok_or(/* at-capacity error */)?;` then return `fh = key as u64 + 1`.
- `read` fast-path: `self.handles.get((fh - 1) as usize).map(|g| (*g).clone())`
  — **lock-free**, no mutex on the hottest serve path.
- `release_handle`: `self.handles.remove((fh - 1) as usize);`

Two invariants this preserves:

- **Non-zero `fh`.** This code treats `fh == 0` as "no handle" (`read()` falls
  back to inode resolve). sharded-slab keys start at `0`, so we store `fh = key +
  1` and look up `key = fh - 1`. The `fh != 0` guard in `read()` is unchanged.
- **ABA safety.** sharded-slab encodes a generation in each key, so a removed slot
  that is later reused yields a *different* key. A stale `read` on an
  already-released `fh` returns `None` → the inode fallback path (bounded by the
  entry/attr TTL), never another file's handle. A raw slab index would not give
  this; the generation is why the slab is correct here.

`Slab::insert` returns `None` only at the slab's configured capacity (very large
by default); map that to a `CoreError` (handle-table-full) rather than panicking.

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

- `getattr` hit/miss: `self.size_cache.get(&track_id).map(|e| *e)`; on a miss,
  `self.size_cache.insert(track_id, SizeEntry { … })`.
- `poll_refresh` prune: `self.size_cache.retain(|k, _| live.contains(k));`

The map stays unbounded-but-tiny by design (one small `Copy` entry per live
track, self-invalidating on `content_version`); DashMap's internal sharding
removes the single-mutex contention without a hand-rolled LRU.

### Accessors and lock-order comment

Delete the `fn handles()` and `fn size_cache()` `MutexGuard` accessor helpers
(call sites use the slab / DashMap directly). Update the lock-order comment block
in `facade.rs`: `handles` and `size_cache` are no longer locks, so they drop out
of the "in-memory locks" ordering entirely. The remaining ordering (pool
connection first, then `inodes` / header-cache shards) is unchanged.

### Dependencies

Add to `musefs-core/Cargo.toml`:

- `sharded-slab` (lock-free slab; backs `tracing`)
- `dashmap` (sharded concurrent map)

Consistent with the crate already taking `arc-swap`, `im`, and `once_cell`.

## Validation

- **Functional gate (hard):** full workspace `cargo test` green, plus
  `cargo test -p musefs-fuse -- --ignored` (real-mount byte-identical e2e) green.
  The byte-identical audio round-trip is the non-negotiable gate.
- **Concurrency correctness:** the existing facade tests exercise
  open/read/release and refresh-prune against the new slab/DashMap; add coverage
  if a path is newly reachable (e.g. at-capacity insert error), otherwise the
  current suite is sufficient since behavior is unchanged.
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
