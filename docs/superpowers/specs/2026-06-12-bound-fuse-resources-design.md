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

`opendir` already inserts under the `dir_handles` mutex. Add a count check
against the live map **before** insert: if `map.len() >= MAX_DIR_HANDLES`, reply
`ENFILE` without inserting (and without consuming a `dir_fh` id). `releasedir`
already removes entries, so the count is self-healing — `map.len()` is the
gauge, no separate counter.

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
  documented as a known edge rather than designed around. (An aggregate
  byte-budget would be the answer if this case ever became real; it was
  considered and discarded in favour of the simpler count cap.)

### Part B — bound foreground read work (#308)

Add `inflight_reads: Arc<AtomicUsize>` to `MusefsFs`. In `read`:

1. `fetch_add(1, …)`; bind a drop-guard that `fetch_sub(1, …)` on drop, so the
   count is released on worker completion **and** on panic.
2. If the post-increment count `> MAX_INFLIGHT_READS`, reply `EAGAIN` and return
   immediately — no enqueue, no read-buffer allocation, the `ReplyData` is
   consumed by the error reply, the guard drops.
3. Otherwise enqueue as today, **moving the guard into the closure** so it
   decrements when the worker finishes.

The dispatch thread never blocks. The pool queue cannot grow past the cap
because submission stops once the count is exceeded. Nothing blocking is added
to the read path: an over-cap read is rejected, not queued and not run inline.

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
bound cheap.

### Scope boundary for Part B

Gating covers `read` only. It is the operation with the largest payloads and the
one that keeps request state alive in the queue. The other pool users are
already bounded: `opendir` by Part A, `poll_refresh` by its single-flight gate
(#89), and `lookup` / `getattr` / `open` carry tiny replies. Reads are the
genuine queue-growth lever.

## Testing

Mirror the existing house pattern: one direct unit test of the decision logic
per part, plus end-to-end coverage where feasible.

- **Part A.** Extract the cap-and-insert decision into a pure helper (a function
  over the handle map and the cap, returning whether a new handle may be
  admitted). Unit-test that `len >= cap` rejects, that a sub-cap state admits,
  and that removing a handle frees a slot. Optional `--ignored` FUSE e2e: open
  `MAX_DIR_HANDLES + 1` directories and assert the final `opendir` returns
  `ENFILE`.
- **Part B.** Extract the reserve decision (the atomic counter and cap, yielding
  enqueue-vs-reject) into a pure helper. Unit-test the boundary
  (`count == cap` admits, `count > cap` rejects) and that the drop-guard
  decrements on drop. The reject-reply path itself is integration-level; the
  accounting is covered unit-side.

## Documentation

Update the two existing in-code rationale comments to point at the new bounds:
the `ThreadPool` "queue is unbounded" note in `MusefsFs::new` and the
`max_background` field doc on `FuseConfig`. No user-facing documentation changes,
since neither bound adds configuration surface.

## Out of scope

- Configurable cap values (no evidence any deployment needs different limits).
- An aggregate byte-budget for directory listings (the flat-template degenerate
  case above); revisit only if a real flat-template-on-huge-library deployment
  appears.
- Bounding non-read pool work (`lookup`/`getattr`/`open`), already cheap or
  bounded elsewhere.
