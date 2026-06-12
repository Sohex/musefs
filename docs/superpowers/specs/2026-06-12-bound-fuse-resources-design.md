# Bound FUSE-layer resources against a hostile local client

**Issues:** #307 (directory-handle snapshots), #308 (foreground read queue).
**Tracking:** #280 (audit Task 20).
**Status:** design approved; ready for implementation planning.

## Problem

Two tables in the FUSE adapter (`musefs-fuse/src/lib.rs`) grow without bound
under an adversarial local client, across the **local FUSE client → process
memory** trust boundary:

- **#307 — directory handles.** `opendir` builds a full directory listing and
  stores it in `dir_handles: HashMap<u64, Arc<Vec<(u64, FileType, String)>>>`,
  keyed by a monotonic `dir_fh`. `releasedir` removes the entry; `readdir`
  clones the stored `Arc`. There is no quota: a client can repeatedly open a
  large directory and hold the file descriptors open, and each open duplicates
  the full listing vector. Memory grows with `unreleased handles × entries ×
  name length`. This is weaker than the regular file-handle path, whose
  `sharded_slab::Slab` insertion failure already maps to
  `CoreError::HandleTableFull → ENFILE`.

- **#308 — foreground reads.** `MusefsFs::new` uses `ThreadPool::new`, whose
  queue is an unbounded `std::sync::mpsc::channel`. `max_background` (set in
  `init`) caps only the kernel's *background/readahead* requests; foreground
  reads are bounded only by client concurrency. Every FUSE `read` enqueues a
  closure owning its `ReplyData`, and queued replies keep request state alive
  until processed. A client driving many simultaneous foreground reads grows the
  process queue independently of the worker count.

## Approach

Two independent, non-configurable bounds added to `musefs-fuse/src/lib.rs`. No
`musefs-core`/`-db`/`-format` changes; no new config or CLI surface. Both follow
the existing house pattern — a capacity-bounded table whose overflow maps to a
clean errno — already used by the file-handle slab (`HandleTableFull → ENFILE`).

The cap values are hardcoded constants (like `MAX_RETAINED_READ_BUF`), each with
a rationale comment. They are sized from two opposing constraints: comfortably
**above any legitimate client's concurrency**, so real clients never hit them,
and bounding **worst-case adversarial memory** to a finite figure (versus
today's unbounded growth).

### Part A — bound directory-handle snapshots (#307)

The whole `opendir` body runs on the pool thread, and it already takes the
`dir_handles` mutex to insert. Extend that single lock hold to cover the cap
check and the id allocation, so concurrent `opendir` closures (up to `workers`
in flight) cannot race the count past the cap: under the lock, if
`map.len() >= MAX_DIR_HANDLES`, reply `ENFILE`; otherwise `dir_fh.fetch_add(1)`
and insert. `releasedir` already removes entries under the same mutex, so the
count is self-healing — `map.len()` is the gauge, no separate counter.

Two structural points the implementer must preserve (the current code does the
opposite of the first):

- **The `dir_fh.fetch_add` moves *after* the cap check, inside the lock.** Today
  it happens before the lock (`lib.rs:361`). Allocating the id only on the admit
  path keeps a rejected open from consuming an id, and keeping it inside the lock
  keeps check-and-insert atomic. (Monotonicity of `dir_fh` is preserved either
  way; the only behavioural change is that a rejected open no longer burns an
  id.)
- **The check is a pool-thread check under the existing mutex**, not a
  dispatch-thread check like Part B's read gate. An implementer who mirrors Part
  B's synchronous-on-dispatch structure here would build the wrong thing.

`ENFILE` is correct here: a failed `opendir` is the literal "too many open files
in system" condition that every directory walker already handles, and — unlike a
mid-stream read error — it cannot corrupt a playback stream.

**`MAX_DIR_HANDLES = 1024`.** Reasoned over an absurd-but-plausible library:

- *Legitimate ceiling.* A single `ls` holds one handle; depth-first `find`/`du`
  holds O(depth) ≈ tens. The real stress case is a parallel indexer
  (Spotlight / Tracker / `fd -jN`) under the `PARALLEL_DIROPS` capability the
  adapter enables, holding ≈ `threads × depth` handles. A heavy 32-thread
  indexer at depth 10 ≈ 320 concurrent handles. 1024 gives ~3× headroom, so
  legitimate clients essentially never see `ENFILE`.
- *Worst-case memory* = `1024 × widest-directory listing`. The virtual tree is
  template-driven (`musefs-core/src/tree.rs`), so a directory's width is the
  count of distinct rendered child components at that level. For a normal-huge
  templated library (≈1M tracks, widest dir ≈50k entries at ≈70 B/entry ≈
  3.5 MB) the product is ≈3.5 GB — finite and adversarial-only, versus today's
  unbounded. The 3.5 MB base is *inherent*: a single legitimate `ls` of that
  directory already allocates it once. The cap bounds only the *multiplier*
  (the number of unreleased full copies).
- *Degenerate caveat.* A flat template (e.g. `$title`) places the entire library
  in one directory, so the widest listing ≈ total track count and the product
  grows with library size. That is a self-inflicted template choice; it is
  documented as a known edge rather than designed around. Even here the cap is a
  *strict improvement*: it bounds the multiplier to a finite `1024`, where today
  the multiplier is unbounded — just not down to a small constant. (An aggregate
  byte-budget would be the answer if this case ever became real; it was
  considered and discarded in favour of the simpler count cap.)

### Part B — bound foreground read work (#308)

Add `inflight_reads: Arc<AtomicUsize>` to `MusefsFs`. The gate runs
**synchronously on the dispatch thread, in `read`, before `pool.execute`** — this
is the load-bearing fact, because rejecting before submission is what actually
caps the queue. Only `core.read_into` and the guard's `fetch_sub` run on the
worker. In `read`:

1. `fetch_add(1, …)` and bind a drop-guard that `fetch_sub(1, …)` on drop, so
   the reservation is released on worker completion **and** on panic. The guard
   *owns* an `Arc<AtomicUsize>` (it is moved into a `'static` pool closure on the
   admit path), so it cannot reuse the borrow-based `PollPendingGuard`; it is a
   sibling type of the same shape.
2. If the post-increment count `> MAX_INFLIGHT_READS`, reply `EAGAIN` and return
   immediately on the dispatch thread — no enqueue, no read-buffer allocation,
   the `ReplyData` is consumed by the error reply, the guard drops here.
3. Otherwise enqueue as today, **moving the guard into the closure** so it
   decrements when the worker finishes.

Because the single fuser dispatch thread checks reads serially, there is no
TOCTOU among reads. The invariant: at most `MAX_INFLIGHT_READS` guards are held
simultaneously by admitted (queued-or-running) reads; a rejected read transiently
observes `cap + 1` and immediately releases. The dispatch thread never blocks,
the pool queue cannot grow past the cap because submission stops once the count
is exceeded, and nothing blocking is added to the read path: an over-cap read is
rejected, not queued and not run inline.

**Errno: `EAGAIN`.** Because nothing blocks, an over-cap read must surface *some*
errno. No errno makes a blocking `read()` retry transparently (only `EINTR`
does, and fabricating `EINTR` from FUSE collides with the kernel's
request-interruption semantics, so it is excluded). Among the honest choices,
`EAGAIN` ("resource temporarily unavailable") is the canonical transient /
try-again signal and the one robust I/O code is most likely to retry; `ENOMEM`
reads as fatal system-wide OOM, and `EBUSY` is non-idiomatic on `read()`.
Because the cap is sized so a legitimate client never reaches it, `EAGAIN` is an
attack-only response in practice, and its one downside — a naive app treating
`EAGAIN` on a blocking fd as fatal — only bites an adversary.

**`MAX_INFLIGHT_READS = 1024`.** A sequential player has one outstanding
foreground read (kernel readahead is separately bounded by `max_background`);
even parallel scans (`rsync`, a media-server library walk) reach only hundreds.
Queued-but-not-running job state is small — a boxed closure plus the `ReplyData`,
well under 1 KB — so 1024 queued ≈ ~1 MB; the large per-read buffers (≤
`MAX_RETAINED_READ_BUF` = 2 MiB) exist only for the ≤ `workers` jobs actually
running. 1024 is far above any legitimate foreground-read burst and keeps the
bound cheap. The cap is on *concurrently outstanding* reads, not on worker
count: the kernel can hold many foreground FUSE read requests in flight at once
(well above `workers`), and that fan-in — not the drain rate — is the queue's
growth lever, which is exactly what the cap targets.

### Scope boundary for Part B

Gating covers `read` only. It is the operation with the largest payloads and the
one that keeps request state alive in the queue. The other pool users are
already bounded: `opendir` by Part A, `poll_refresh` by its single-flight gate
(#89), and `lookup` / `getattr` / `open` carry tiny replies. Reads are the
genuine queue-growth lever.

## Testing

Mirror the existing house pattern: one direct unit test of the decision logic
per part, plus end-to-end coverage where feasible.

- **Part A.** Extract the *guarded admit* into a pure helper — a function over
  `&mut HashMap<…>`, the `dir_fh` counter, the cap, and the freshly built
  listing, that performs the whole atomic step the caller runs under the lock:
  reject (return `None`) when `len >= cap`, otherwise allocate the id, insert,
  and return `Some(fh)`. Testing the helper then exercises the real
  check-and-insert, not just `len >= cap` arithmetic — so the unit test catches a
  regression that admits past the cap, which a bare predicate would miss.
  Unit-test that a sub-cap state admits and inserts, that `len == cap` rejects
  without inserting or advancing the id, and that removing a handle frees a slot.
  Optional `--ignored` FUSE e2e: open `MAX_DIR_HANDLES + 1` directories and
  assert the final `opendir` returns `ENFILE`.
- **Part B.** Extract the reserve decision (the atomic counter and cap, yielding
  enqueue-vs-reject) into a pure helper. Unit-test the boundary
  (`count == cap` admits, `count > cap` rejects) and that the drop-guard
  decrements on drop. The reject-reply path itself is integration-level; the
  accounting is covered unit-side.

## Documentation

Update the two existing in-code rationale comments to point at the new bounds:
the `ThreadPool` "queue is unbounded" note in `MusefsFs::new` and the
`max_background` field doc on `FuseConfig`. Add a rationale comment at each new
cap site (the `opendir` check and the `read` gate), in the style of the existing
`MAX_RETAINED_READ_BUF` comment. No user-facing documentation changes, since
neither bound adds configuration surface.

## Implementation sequencing

The pre-commit hook runs the full workspace test suite, so each commit must be
green. The extracted decision helpers and their unit tests can land green in one
commit ahead of wiring the call sites, satisfying that constraint cleanly; a
natural split is a Part-A commit and a Part-B commit, each self-contained and
green.

## Out of scope

- Configurable cap values (no evidence any deployment needs different limits).
- An aggregate byte-budget for directory listings (the flat-template degenerate
  case above); revisit only if a real flat-template-on-huge-library deployment
  appears.
- Bounding non-read pool work (`lookup`/`getattr`/`open`), already cheap or
  bounded elsewhere.
