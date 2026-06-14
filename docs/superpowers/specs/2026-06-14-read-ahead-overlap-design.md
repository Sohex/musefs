# Single-stream backing read-ahead (issue #255)

## Problem

In synthesis mode, a single sequential read through the mount is served one
FUSE chunk (~256 KiB) at a time. Each chunk's backing bytes come from a
positioned `pread` on the original file in `read_segments_into`
(`musefs-core/src/reader.rs`) — for PCM via the `BackingAudio` arm, for Ogg via
`serve_ogg_window` (`musefs-core/src/ogg_index.rs`). The kernel issues these
reads strictly serially for one stream: it waits for each reply before issuing
the next. Throughput is therefore `chunk_count × per-read latency` and is
unaffected by `--max-readahead-kib`.

Measured cold single-stream throughput (`BENCHMARKS.md#storage-tunables`):

| backing            | MB/s |
| ------------------ | ---- |
| local HDD          | ~250 |
| NFS 8 ms RTT       | ~31  |
| NFS 50 ms RTT      | ~5   |
| NFS 200 ms RTT     | ~1.3 |

Concurrent streams over distinct files already hide the latency (16 NFS streams
reach ~10× the single-stream aggregate), so the backing serves parallel reads
fine — only the single-stream path lacks overlap.

Pure-passthrough (structure-only) files dodge this entirely via kernel FUSE
passthrough (Linux 6.9+): the kernel reads the backing fd directly. Synthesis
files cannot — the metadata splice shifts byte offsets, so the daemon must serve
them. This spec addresses synthesis mode only.

## Non-goals

- **No async runtime.** musefs deliberately rejected async-fuse as net-negative.
  The prefetch pipeline uses the existing synchronous thread-pool model:
  background OS threads issuing blocking `pread`s in parallel.
- **Issue #173 (SQLite mmap for read-only serve connections) is out of scope.**
  It targets a different layer (DB page access, not backing audio bytes), is an
  investigation rather than a known win, and would not move the #255 numbers.
  It gets its own spec.
- **No per-medium / window-size knob.** The window sizing is fully adaptive
  (see below). The only operator-facing control is the global memory ceiling.

## Approach

A **per-handle backing read-ahead buffer** that caches *raw backing-file bytes
keyed by absolute backing-file offset* — not synthesized output. The backing
`pread` is the only high-latency source; the synthesized header/art/tag segments
are small, near offset 0, and already served cheaply (and `HeaderCache`-cached).
Caching backing bytes (rather than synthesized output) also leaves the metadata
freshness / retag semantics in `Musefs::read_into` (`facade.rs`) completely
untouched.

Delivered in two phases sharing one buffer abstraction:

- **Phase 1 — read amplification (synchronous).** On a buffer miss, one large
  `pread` refills the window; subsequent sequential chunks are served from it
  with no syscall. Cuts round-trips by the amplification factor (e.g. ~16× for a
  4 MiB window vs 256 KiB chunks). Latency is amortized, not hidden. This phase
  builds and proves the buffer + invalidation substrate under the existing
  retag/refresh races with no added concurrency.
- **Phase 2 — parallel prefetch.** Background workers read ahead of the kernel's
  position into a ring of windows, fully overlapping backing RTT
  (bandwidth-bound, not RTT-bound). Reuses the Phase 1 buffer unchanged; only the
  fill strategy changes.

## Architecture

### `BackingReader`

A new type wrapping a handle's backing fd plus an adaptive read-ahead buffer,
exposing the single method every backing read goes through:

```
read_exact_at(&self, buf: &mut [u8], abs_offset: u64) -> Result<()>
```

- **Hit** (requested range ⊆ current window): memcpy from the buffer, no syscall.
- **Miss**: one large `pread` refilling the window starting at `abs_offset`,
  sized to the current adaptive window; then serve from it.

The unifying rule: **every read of the backing fd goes through
`read_exact_at`.** This is what brings Ogg into scope rather than special-casing
it:

- `BackingAudio` arm of `read_segments_into`: today
  `backing_read_exact_at(f, buf, bo + within)` → routes through the reader.
- `OggAudio` arm: `serve_ogg_window`, `read_counted`, and `find_page_start`
  (`ogg_index.rs`) take a `&BackingReader` instead of `&std::fs::File`; their
  header and payload preads consult the same buffer. This is a wider signature
  change through `ogg_index.rs`, accepted to keep Ogg first-class.

`read_segments_into` / `read_at_with_file_into` thread an
`Option<&BackingReader>` where they currently thread `Option<&std::fs::File>`.

### Placement

- The buffer is a field on `Handle` (`facade.rs`), behind a `Mutex`. `Handle`
  already lives in `sharded_slab::Slab<Arc<Handle>>`, so no new slab is needed;
  the per-handle slot lifecycle is already provided. A single sequential stream
  never contends the mutex; it only serializes concurrent/random reads on the
  same fh.
- The buffer is **allocated lazily** — nothing until the detector sees
  sequential access — so the thousands of handles a scanner opens-and-stats cost
  zero read-ahead RAM. Freed on `release`.
- The no-handle fallback path (`read_at_into`, one-shot open) keeps doing plain
  preads. The FUSE `read` op always carries the fh issued by `open`, so a normal
  sequential stream (player, `cat`, scanner) always resolves to a `Handle` and
  gets read-ahead. The fallback in `read_into` is reached only when `fh` is
  `None` or names a handle absent from the slab — a pathological case no normal
  stream produces — so excluding it costs the headline numbers nothing.

### `BackingReader` ownership

`BackingReader` is constructed per `read_into` call and lives only for that call.
It borrows the `Handle`'s backing fd and a reference to the global budget, and
holds the per-handle `Mutex<Buffer>` guard. `read_exact_at` takes `&self` with
the window/budget mutation behind that mutex (interior mutability) — no `&mut`
threading through the splice loop. Phase 1's large refill `pread` runs on the
foreground worker, which already holds an `MAX_INFLIGHT_READS` slot, so Phase 1
adds no new slot pressure.

## Mechanism: sequential detection & adaptive window

Per handle, track `next_expected_offset` (the backing offset just past the last
served read) and `window` (current size, between a floor and a cap — see Memory
bounding). Each `read_exact_at(buf, abs_offset)` resolves via this decision
table:

| condition                                              | classification  | action                                                                       |
| ------------------------------------------------------ | --------------- | ---------------------------------------------------------------------------- |
| requested range ⊆ current window                       | **hit**         | memcpy from window, no syscall                                               |
| range ⊄ window **and** `abs_offset == next_expected`   | **seq. miss**   | grow `window` (geometric, capped), refill at `abs_offset`, serve             |
| range ⊄ window **and** `abs_offset != next_expected`   | **seek**        | shrink `window` to floor, free old bytes, refill at `abs_offset`, serve      |

After serving, set `next_expected = abs_offset + len`. "Grow" is geometric
(double, capped); successive sequential misses fetch ever-larger chunks, which is
what self-tunes HDD (stays small) vs 200 ms NFS (ramps to MBs). On seek the old
window's bytes are freed immediately (returned to the budget), not retained.

For the Ogg page-walk, a page's header and payload preads are adjacent and the
walk proceeds forward, so once the window covers a page both reads hit and the
stream stays sequential under the same detector. `serve_ogg_window` issues
several `read_exact_at` calls per request (header + payload per page); each
re-locks the per-handle buffer mutex, which is correct (intra-request re-locking
is uncontended for a single stream) but changes the per-request pread count the
`metrics`-feature tests assert — see Testing.

## Memory bounding

Hybrid: lazy per-handle buffers drawing from one global byte budget with
eviction.

- **Global budget:** a single process-wide cap on total buffered bytes, separate
  from the `HeaderCache` budget, held as an `AtomicU64` charged/uncharged with
  `fetch_add`/`fetch_sub`. No lock on the per-read hot path.
- **Window floor / cap / growth.** Floor = one FUSE chunk-class size (e.g.
  512 KiB) so a fresh or just-seeked stream still does useful read-ahead. Growth
  = geometric doubling per sequential miss. Per-stream cap = `min(absolute_cap,
  budget / DIVISOR)` (e.g. `DIVISOR = 4`), so no single stream can monopolize the
  envelope. The division across N active streams is **emergent, not a static
  partition**: each stream grows greedily toward its cap; the global atomic
  budget is the hard ceiling, and LRU eviction (below) reclaims from colder
  streams when a hotter one needs to grow. Under sustained N-stream load this
  settles toward roughly equal shares without any explicit `budget / N`
  computation. Final constants are benchmark-chosen in the plan; the policy
  (floor, geometric growth, per-stream cap, global hard ceiling, LRU balancing)
  is fixed here.
- **Eviction + lock order (deadlock-free by construction).** When charging the
  budget would exceed the cap, the growing stream runs eviction: it scans a small
  guarded **active-stream registry** (live streaming-handle keys + last-served
  counter — bounded by concurrent streams, not total opens), picks the coldest,
  and reclaims it by `try_lock`-ing that victim's per-handle buffer mutex. The
  strict rules that make this deadlock-free:
  1. The budget is a lock-free atomic — never a held lock.
  2. The registry lock is a leaf: victim keys are copied out and it is released
     before touching any buffer mutex.
  3. Eviction **never blocks** on a victim's buffer mutex — it `try_lock`s and
     skips a victim that is mid-read, moving to the next-coldest. So no thread
     ever holds buffer-mutex-A while waiting on buffer-mutex-B or the registry.
  If nothing reclaimable is found (all candidates busy), the grower simply does
  not grow this round and serves at the current window — graceful degradation,
  never a stall. A reclaimed handle re-misses and re-fetches (correctness
  unaffected). `quick_cache` is not used (its keys are content, not live
  handles).
- **Free on close / seek-shrink:** `release` returns the buffer to the budget; a
  detected seek shrinks the window and frees the old bytes rather than holding
  the high-water mark.

### Configuration

- **One flag: `--read-ahead-budget-mib`** (default = a new internal constant,
  e.g. 64). `0` disables read-ahead entirely — a clean escape hatch back to
  per-chunk preads for debugging or pathological backing.
- Rationale for exposing this one knob (against the project's anti-knob stance):
  the read-ahead budget tracks *concurrent-consumer count*, which only the
  operator knows and the daemon cannot infer — unlike the `HeaderCache` budget,
  which tracks library size and stays an internal constant. A single-user and a
  100-user deployment genuinely need different ceilings.
- This stays a *single* knob: the operator sets the RAM envelope; the adaptive
  window logic divides it across active streams. No window-size or prefetch-depth
  knob is added.
- Implementation seam: mirror `HeaderCache::with_budget` — the constructor takes
  the budget (default backs the flag and tests).

## Correctness & invalidation

The design is safe because the buffer caches raw backing bytes keyed by absolute
backing-file offset, and serving still flows through the existing per-read
validation.

1. **Audio invariant.** The buffer stores backing bytes verbatim and serves them
   verbatim — never transformed. Ogg header *patching* happens on top of the raw
   bytes after the read (unchanged), so caching raw bytes does not touch it. The
   existing one-entry `LastPageMemo` in `serve_ogg_window` (which caches a
   *patched header*) is orthogonal to and independent of the new buffer (which
   caches *raw backing bytes*); they cache different things and do not interact.
2. **Retag/refresh survives the buffer for free.** A retag changes DB metadata —
   the synthesized segments (`Inline`/`ArtImage`/`BinaryTag`) and the virtual
   layout — but not the backing audio bytes nor their absolute offset in the
   original file. Keyed by backing-file offset (not virtual offset), cached bytes
   cannot be made wrong by a retag. For maximal conservatism the buffer is
   nonetheless **dropped on any handle generation bump** (refills are cheap; this
   makes buffer validity trivially track the invariants the facade already
   enforces). A tight retag loop thrashes the buffer harmlessly; retags are rare
   vs reads. The generation bump, a seek, and `release` all advance **one
   per-handle epoch**; any in-flight Phase-2 prefetch checks the epoch before
   storing and discards a fill made against a stale one (§"Phase 2"). So seek,
   release, and refresh are unified under a single invalidation signal rather
   than two mechanisms.
3. **Per-read re-stat guard preserved; the buffer is bound to `resolved.stamp`.**
   `validate_opened_backing` compares the fd's live `BackingStamp` against
   `resolved.stamp` (the stamp captured at resolve time) on every `read_into`
   call, *before* the splice descends, and is terminal (`BackingChanged`) on
   drift. The buffer is bound to that same `resolved`: it can only have been
   filled through this fd, and it is **dropped on any generation bump** (§2),
   which is the only event that swaps `resolved` (and thus `resolved.stamp`).
   Therefore a buffered byte is served only when the current read's validate
   passes — i.e. the fd's stamp still equals the stamp under which those bytes
   were filled. No separate fill-time stamp is needed; `resolved.stamp` *is* the
   fill-time stamp.
   - **Detectable rewrite (size or mtime changes):** the next read's
     `validate_opened_backing` mismatches `resolved.stamp` → `BackingChanged`
     terminal; the buffer is never consulted. Identical behavior with or without
     read-ahead.
   - **Undetectable rewrite (same size *and* same mtime in place):** validate
     passes either way. Without read-ahead a hit-region read preads post-rewrite
     bytes; with read-ahead it may serve pre-rewrite buffered bytes. Both are real
     backing bytes never modified by musefs; this same-size+same-mtime in-place
     case is already outside the guarantee (Finding #15, ESTALE; the
     external-writer contract requires content changes to bump `content_version`
     rather than silently rewrite). Read-ahead's exposure is therefore the same
     *kind* and bound by the same precondition as today's — not wider.
4. **No interaction with the `begin_read` / `content_version` snapshot.** That
   machinery guards DB-sourced segments (`BinaryTag`); read-ahead caches only
   backing bytes and sits outside that path. **Invariant (constrains the Phase-2
   worker):** prefetch workers touch *only* the backing fd via positioned reads —
   never the `Db`, the connection pool, or any WAL snapshot. A foreground read
   may hold an open `db.begin_read()` snapshot while a background prefetch runs;
   keeping prefetch `Db`-free means it cannot open a second snapshot, contend the
   `PerThread` pool, or perturb `content_version` checks.
5. **Concurrency.** The buffer is `Mutex`-guarded and per-handle. A single
   sequential stream never contends; concurrent/random reads on the same fh
   serialize on the buffer mutex only, with no cross-handle effect.
6. **Phase 2 does not widen exposure.** Background prefetch only *fills* the
   buffer; serving still gates on the per-read `validate_opened_backing`.
   Prefetch I/O errors (e.g. NFS ESTALE) are swallowed, leaving the slot empty so
   the serving read re-misses and surfaces the real error synchronously. Prefetch
   is strictly best-effort.

## Phase 2: parallel prefetch detail

- The buffer generalizes from one window to a small **ring of windows**: while
  the kernel consumes window *K*, a background worker fills *K+1* (up to a
  prefetch depth of further windows).
- **Trigger:** serving a read that advances into the buffer enqueues a fill for
  the next not-yet-filled window, if one is not already outstanding. A per-handle
  "fill in flight" state ensures one prefetch per window.
- **Worker bound:** prefetch reads are daemon-internal and must **not** consume
  the FUSE `MAX_INFLIGHT_READS` slots (that cap bounds the kernel-driven pool
  queue, #308). Prefetch gets its own small concurrency bound so it can never
  starve foreground reads.
- **Cancellation without killing preads:** a blocking `pread` cannot be
  interrupted, so we do not try. Each handle carries the single epoch from
  Correctness §2 — a seek, `release`, **or a refresh-generation bump** advances
  it. A prefetch job reads the epoch when dispatched and re-checks it under the
  buffer mutex before storing; if it changed, the fill is discarded. The
  abandoned `pread` completes and its bytes are dropped. This is what makes an
  in-flight prefetch against a layout that was just re-resolved (generation bump)
  safe to throw away.
- **Adaptive depth:** the window-growth signal also drives how many windows ahead
  to prefetch, bounded by the stream's budget share. High-RTT NFS ramps depth up;
  HDD stays shallow. No knob beyond the budget ceiling.

## Testing & rollout

- **Differential correctness (keystone).** For both PCM and Ogg fixtures, bytes
  served through the read-ahead path must be byte-for-byte identical to direct
  preads, across sequential and random access, arbitrary offset/size splits, and
  buffer eviction. The test injects a deliberately tiny budget via the
  `with_budget` seam so eviction is *forced* mid-stream (a default-sized budget
  would silently never exercise the eviction path). Includes a seek that lands
  *partially* back inside a just-freed window region, to catch off-by-one in the
  shrink/refill offset math. Reuses the existing `ogg_serve_tests` / serve
  fixtures in `reader.rs`.
- **Unit.** hit/miss; window grow-on-sequential; reset-on-seek; eviction reclaims
  coldest; drop-on-generation-bump; drop-on-validation-failure.
- **Concurrency.** multi-thread reads of one handle (the existing TSan CI job
  covers this; TSan needs `-Zbuild-std`).
- **Memory.** global budget never exceeded under N concurrent streams; eviction
  reclaims the coldest buffer.
- **Metrics.** read-ahead changes `pread`/`open` counts, and the CI
  `metrics`-feature tests assert exact counts — they need updating, plus a new
  read-ahead hit/miss counter. Run `cargo test -p musefs-core --features metrics`
  before push (local `--workspace` skips the `metrics` feature).
- **Bench + docs.** Extend `benches/storage_tunables_bench.sh` to measure
  single-stream throughput read-ahead on/off; record results in `BENCHMARKS.md`,
  replacing the "latent finding / future work" note with the actual numbers. Doc
  touch-ups: `ARCHITECTURE.md` (reader / segment-model note), `docs/OGG.md`,
  README (`--read-ahead-budget-mib`).
- **Phasing for the pre-commit gate.** Each phase lands as its own green commit
  (Phase 1 buffer + amplification, then Phase 2 prefetch); the pre-commit hook
  runs the full workspace test suite, so each commit must be green.
  `.cargo/mutants.toml` line:col anchors shift when `reader.rs` / `facade.rs` /
  `ogg_index.rs` change — re-anchor in the same commit via each entry's
  `# guard:` tag.
- **Fuzz check.** `serve_ogg_window` lives in `musefs-core` (not the format
  layer), but verify the out-of-workspace `fuzz/` crate still builds if any
  format-layer signature is touched: `cargo +nightly fuzz build`.
