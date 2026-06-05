# Replace HeaderCache's hand-rolled sharded LRU with quick_cache

**Date:** 2026-06-05
**Issue:** #136 — `reader.rs` hand-rolls a sharded LRU cache
**Status:** Approved

## Problem

`HeaderCache` (`musefs-core/src/reader.rs`) implements a byte-budgeted LRU
from scratch: `CACHE_SHARDS` (8) `Mutex<Shard>`s, each a `HashMap<i64,
LruNode>` plus a doubly-linked list expressed as `Option<i64>` prev/next
indices maintained by manual `unlink`/`push_front` pointer surgery, with
hand-kept byte accounting against a per-shard budget. This is exactly the
edge-case-prone logic maintained concurrent-cache crates already harden, and
it drags along local plumbing: shard routing, per-shard budget division, and
a `Clearable` poison-recovery impl in `lock.rs`. It is also the last shared
lock on the serve path (taken during every `resolve`).

## Crate choice

Requirements: concurrent `get(&self)` from FUSE worker threads (sync, no
async runtime), byte-weighted budget (64 MiB default over `cache_bytes`),
single-key `remove`, prune-to-live-set `retain`, eager eviction preferred so
the bound holds deterministically, small dependency tree.

A full sweep (2026-06-05) of moka, mini-moka, micro-moka, quick_cache,
stretto, foyer, clru, lru, schnellru, sieve-cache, and cached landed on
**quick_cache** (v0.6.x, S3-FIFO eviction): it is the only candidate hitting
every requirement — internally sharded `&self` API, `u64` `Weighter`, eager
synchronous eviction on `insert`, native `retain()` predicate, `remove()`,
2 required deps, no background threads, actively maintained.

Runners-up and dismissals: **moka** (the prior presumption from the v1
review triage) has lazy/amortized eviction — the byte bound is soft without
`run_pending_tasks()` — plus an opt-in, also-lazy `invalidate_entries_if`, a
`u32` weigher, and the heaviest dep tree; **clru** keeps the weighted-LRU
core but is `&mut self`, so all the Mutex-shard plumbing would survive;
**sieve-cache** is zero-dep and concurrent but lacks a documented `retain()`
on its sharded weighted variant; **mini-moka** is stale (last release
2024-01); **lru** has no byte weighting; **stretto** can miss freshly
inserted keys (Ristretto write-buffer semantics); **foyer** is an
async-centric hybrid mem+disk engine; **cached** is a memoization macro
framework.

## Design

All changes in `musefs-core` (`Cargo.toml`, `src/reader.rs`, `src/lock.rs`).

### Dependency

Add `quick_cache = "0.6.23"` to `musefs-core/Cargo.toml`. The patch-level
pin matters: `retain()` — the load-bearing primitive that won quick_cache
the comparison — does not exist in early 0.6.x (verified absent in 0.6.0,
0.6.2, 0.6.9); 0.6.23 provably has it. quick_cache's `parking_lot` feature
is on by default and matches musefs-core's existing direct `parking_lot`
dependency — do not set `default-features = false`.

### HeaderCache shape — public API unchanged

```rust
pub struct HeaderCache {
    cache: quick_cache::sync::Cache<i64, Arc<ResolvedFile>, CacheBytesWeighter>,
    mode: Mode,
}
```

The five public methods keep their exact signatures — `new(mode)`,
`with_budget(mode, budget)`, `resolve(db, track_id)`, `retain(live)`,
`remove(id)` — so `facade.rs`, `benches/read_throughput.rs`, and the
integration tests under `tests/` need no changes. Internals map 1:1:

| Today | After |
|---|---|
| `self.shard(id).get(id)` | `self.cache.get(&id)` |
| `self.shard(id).insert(id, v)` | `self.cache.insert(id, v)` (eviction eager, inside the call) |
| per-shard `retain_keys(live)` loop | `self.cache.retain(\|id, _\| live.contains(id))` |
| `self.shard(id).remove_key(id)` | `self.cache.remove(&id)` |

Deleted outright: `Shard`, `LruNode`, `CACHE_SHARDS`, `Shard::{new, unlink,
push_front, get, insert, retain_keys, remove_key}`, `HeaderCache::shard`,
the `impl Clearable for Shard` in reader.rs, and reader.rs's `lock_or_clear`
calls. `lock.rs` itself stays — the Ogg last-page memo
(`ogg_index.rs`) still uses `lock_or_clear` with `Option<T>` — only its
module doc's mention of HeaderCache shards is updated.

The `resolve` flow is untouched: validate backing size/mtime
(`BackingChanged`) → cache get + `content_version` check against the freshly
read track row → on miss/mismatch, `build` with no lock held → insert.

### Weigher and budget semantics

```rust
#[derive(Clone)]
struct CacheBytesWeighter;
impl Weighter<i64, Arc<ResolvedFile>> for CacheBytesWeighter {
    fn weight(&self, _: &i64, v: &Arc<ResolvedFile>) -> u64 {
        v.cache_bytes.max(1)
    }
}
```

The `.max(1)` is load-bearing: in `StructureOnly` mode every layout is a
single `BackingAudio` segment, so `cache_bytes == 0` for *all* entries, and
quick_cache's documented footgun is that zero-weight entries escape the
weighted bound entirely. Weighting them 1 bounds StructureOnly mounts too —
the current code never evicts them either, so this is strictly an
improvement.

`with_budget` passes the budget as the cache's weight capacity, via
`Cache::with_weighter(estimated_items_capacity, weight_capacity, weighter)`.
For `estimated_items_capacity` use `budget / 4096` (4 KiB as a typical
inline tag region) — it is a sizing hint for the admission ghost structures,
not a bound; the implementer may adjust if profiling says otherwise.
Whether to pin a shard count is decided against the real 0.6.23 API at
implementation time. quick_cache's per-shard weight capacity is a non-issue
here: entries
are KB-scale inline tag framing (`cache_bytes` counts only
`Segment::Inline` bytes; art is streamed via `ArtImage`/`OggArtSlice` and
never counted) against a 64 MiB default budget.

### Accepted behavior changes

- Eviction policy becomes S3-FIFO instead of strict LRU; victim selection is
  no longer externally predictable.
- quick_cache may decline to *admit* a cold entry under pressure. `resolve`
  already tolerates this shape — it returns the freshly built `Arc`
  regardless of what the cache did with it, so a non-admitted entry only
  means a rebuild on the next open. Correctness (`content_version`
  revalidation, `BackingChanged`) never depended on cache residency.
- No `Mutex<Shard>` is left to poison; the cache's poison-recovery category
  in `lock.rs`'s doc comment shrinks to the Ogg memo.

### Invalidation paths — semantics preserved

- **`content_version`:** unchanged. Key stays the `i64` track id; a hit is
  revalidated against the track row and a mismatch falls through to
  rebuild + reinsert.
- **`poll_refresh` prune:** `retain`/`remove` keep their `facade.rs` call
  sites; quick_cache's `retain` and `remove` are the direct primitives, and
  both take effect synchronously (no lazy-invalidation window).

## Testing

`cache_bound_tests` splits three ways:

**Survive as-is** (semantics-level, no internals touched):
`header_cache_resolve_caches_by_content_version`,
`resolve_is_safe_under_concurrent_access`,
`header_cache_retain_drops_absent_tracks`,
`header_cache_remove_drops_one_track_only`,
`default_cache_budget_is_64_mib`, and the `build_*` audio-bounds and
`cache_bytes`-accounting tests (those test `build`, not the cache).
One contingency to watch:
`header_cache_resolve_caches_by_content_version` asserts `Arc::ptr_eq`
across two back-to-back resolves, which now depends on quick_cache
*admitting* the lone entry rather than on a guaranteed insert. A single
small entry in an otherwise empty cache is admitted in practice, but if it
proves otherwise the test — not the design — is what flexes.

**Rewritten property-style** (intent kept, mechanism-free):

- *Budget bound:* insert entries totaling far over budget through the public
  path; assert the cache's total `weight() <= budget` **as an end-state
  check after the flood**, not after each individual insert. quick_cache
  does not document whether eviction inside `insert` is strictly synchronous
  or amortized; the end-state assertion is robust either way. If the
  implementer confirms per-insert eagerness against 0.6.23, the assertion
  may be tightened — but the spec'd form must not assume it.
- *Zero-weight bound:* a `StructureOnly` entry (`cache_bytes == 0`) still
  counts ≥ 1 against the bound, pinning the `.max(1)` weigher behavior
  against mutants.

No hot-entry-survival test: S3-FIFO admission is probabilistic, so "the hot
entry survives a cold flood" is an exact-victim assertion in disguise and a
flaky-CI trap. The agreed bar is "bounded by byte budget, sensible eviction"
— retention guarantees are explicitly out; budget-bound plus zero-weight
cover the intent, and such a test kills no weigher/retain mutants anyway.

**Deleted with the implementation** (they test the removed linked
list/sharding): `shard_evicts_least_recently_used_over_byte_budget`,
`shard_insert_reaccounts_bytes_on_reinsert`,
`shard_evicts_and_subtracts_evicted_bytes`,
`shard_keeps_both_entries_at_exactly_budget`,
`shard_never_evicts_the_sole_entry_even_over_budget`,
`shard_reset_clears_all_entries`, `shard_remove_key_reaccounts_bytes`,
`shard_remove_key_is_noop_for_absent_id`,
`shard_retain_keys_drops_dead_and_reaccounts`,
`with_budget_divides_evenly_across_shards`,
`shard_routes_by_modulo_not_division`.

## Validation

`cargo test` (workspace), `cargo clippy --all-targets` (benches and `tests/`
consume `HeaderCache` and compile only under `--all-targets`),
`cargo fmt --all --check`, and the in-diff mutation gate
(`cargo mutants --in-diff … -j2` per CLAUDE.md) — the weigher and the
`retain` predicate are prime mutant targets the property tests must kill.
Because this touches the serve path's last shared lock, run
`benches/read_throughput.rs` before/after and record the comparison in
BENCHMARKS.md.
