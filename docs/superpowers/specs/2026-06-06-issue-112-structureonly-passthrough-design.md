# Issue #112: Native FUSE Passthrough for StructureOnly Reads

## Problem

In `StructureOnly` mode every read is served verbatim from the backing file,
yet each one still round-trips through the FUSE daemon: kernel → userspace
`read` handler → worker pool → positioned read against the backing fd → copy
back to the kernel. musefs adds no byte-level transformation in this mode —
only the virtual tree — so that per-read overhead is pure cost.

Mainline kernels (6.9+) support FUSE passthrough: the daemon registers a
backing fd at open time and the kernel serves subsequent reads (and mmap)
directly from it. fuser 0.17 — the version already in use — exposes the full
API: `KernelConfig::set_max_stack_depth`, `ReplyOpen::open_backing` →
`BackingId` (the `FUSE_DEV_IOC_BACKING_OPEN` ioctl), and
`ReplyOpen::opened_passthrough`.

## Goals

- StructureOnly reads served by the kernel at near-native throughput, with no
  userspace round-trip after open.
- Lookup, readdir, getattr, and poll-refresh behavior unchanged.
- Strict best-effort: every passthrough failure degrades to the existing
  daemon-served read path, never to a user-visible error.
- No new dependencies, no new CLI surface.

## Non-Goals

- No passthrough in `Synthesis` mode — the served bytes there are spliced, so
  there is no backing fd that represents the virtual file.
- No CLI flag. Passthrough is an internal optimization of StructureOnly mode;
  a flag can be added later if a real need appears.
- No `BackingId` reuse cache across concurrent opens of the same track.
  Per-open registration is one cheap ioctl; dedup can be layered on later
  without design change.
- No write support. The mount is `MountOption::RO`; passthrough cannot open a
  write hole.

## Chosen Approach

Core exposes a per-handle passthrough fd; the FUSE layer owns the `BackingId`
lifecycle. (Rejected: passing the mode into `FuseConfig` and re-opening the
path in the FUSE layer — it duplicates mode knowledge across layers and the
re-opened fd would not be the one `open_handle` validated.)

### Core (`musefs-core/src/facade.rs`)

One new method on `Musefs`: `passthrough_fd(fh)`, returning `Some` only when
the mount mode is `StructureOnly` (gate on the existing `self.config.mode`).
It hands back an owned wrapper around the `Arc<Handle>` implementing `AsFd`,
exposing the backing `File` that `open_handle` already opened and validated —
no lifetime entanglement with the handle slab.

No other core changes. `open_handle`, `read_into`, and `release_handle` are
untouched; in `Synthesis` mode the feature is completely inert.

### FUSE (`musefs-fuse/src/lib.rs`)

- `init`: request `add_capabilities(InitFlags::FUSE_PASSTHROUGH)` **and**
  `set_max_stack_depth(2)`, both best-effort, alongside the existing
  per-bit capability requests. Both calls are required: fuser only copies
  `max_stack_depth` into the init reply when the `FUSE_PASSTHROUGH`
  capability bit was negotiated (fuser `ll/request.rs`), and the stack depth
  must be ≥ 1 or passthrough is disabled. Depth 2 (matching fuser's own
  passthrough example) lets backing files live on a stacked fs themselves
  (e.g. a music library on overlayfs); the cost is nil and the failure mode
  at depth 1 would be a silent fallback the user never sees.
- `open`: after `core.open_handle(ino)` succeeds, ask
  `core.passthrough_fd(fh)`. If `Some`, try `reply.open_backing(fd)`; on
  success, insert the `BackingId` into the `fh → BackingId` map **before**
  sending `opened_passthrough(fh, flags, &backing_id)` — the kernel cannot
  send `release` for an fh before it receives the open reply, so
  insert-before-reply makes the map entry's existence an invariant for every
  live passthrough handle (no orphan-leak window). Otherwise reply plain
  `opened` as today. The open handler runs inside `pool.execute`, which
  captures only what it is given — the map and the sticky-disable flag are
  therefore `Arc`-shared state cloned into the closure (the map as
  `Arc<Mutex<HashMap<u64, BackingId>>>`), exactly like `core` and
  `poll_pending` already are.
- Open flags for passthrough replies: strip `FOPEN_KEEP_CACHE`. Page-cache
  ownership for a passthrough handle belongs to the backing inode, so the
  flag is meaningless there; plain (fallback and Synthesis) opens keep
  today's flags unchanged.
- `release`: remove the map entry — dropping the `BackingId` fires the
  backing-close ioctl — then `core.release_handle` as today. (`release` runs
  synchronously on the dispatch thread; a map remove is cheap enough, same
  rationale as the existing `release_handle` comment.)
- `read`: unchanged. The kernel never sends reads for passthrough handles;
  non-passthrough handles (Synthesis mounts, fallback opens) keep the existing
  path.

Resulting read path in StructureOnly: kernel serves reads and mmap directly
from the registered backing fd.

## Error Handling and Fallback

- The `add_capabilities(FUSE_PASSTHROUGH)` and `set_max_stack_depth` results
  are discarded like the existing capability requests; an old kernel just
  means the flag is not advertised.
- `open_backing` failure (kernel < 6.9 did not ack `FUSE_PASSTHROUGH`, ioctl
  error, fd problem): log once at info level and reply with plain `opened` —
  the handle works exactly as today. The first failure flips an `AtomicBool`
  on `MusefsFs` and subsequent opens skip the attempt, so a long-running mount
  on an old kernel does not pay a doomed ioctl per open.
- `open_handle` failure: unchanged — the error is replied before passthrough
  is considered.
- `BackingId` drop safety: fuser's `Drop` impl holds only a `Weak` to the
  channel, so a `BackingId` dropped after session teardown is a no-op;
  unmount ordering is safe.

Deliberate non-handling: if the first failure was transient rather than
"kernel lacks support", the sticky disable means the mount never retries.
The failure modes are overwhelmingly static per kernel, and the cost of being
wrong is reverting to today's behavior.

## Freshness, Caching, and Metrics

- **Refresh / `content_version`:** open-time validation only. `open_handle`
  still resolves the layout and validates backing size+mtime; once a
  passthrough handle is open it serves the original inode like a plain POSIX
  fd, across backing-file replacement or DB edits. New opens always
  re-resolve and pick up changes. In StructureOnly the bytes are verbatim, so
  a stale handle serves the file it opened — never corrupted splice output.
  Documented on `Mode::StructureOnly` and in CLAUDE.md's mode description.
- **`--keep-cache` / `inval_inode`:** for passthrough handles, page-cache
  ownership moves to the backing inode (and `FOPEN_KEEP_CACHE` is stripped
  from their open replies, per above), so the `poll_refresh_notify` →
  `inval_inode` flow is irrelevant but harmless for them (it targets the FUSE
  inode's cache, which passthrough reads do not populate). No changes; a
  comment notes why.
- **Metrics:** `metrics::on_open` still counts opens. Per-read counters
  (`on_pread` etc.) will not see passthrough reads — they measure daemon
  work, not user traffic, which is now true by design. Noted in the
  `metrics.rs` module docs. This is also the observable the e2e test asserts
  on.
- **`poll_refresh` cadence:** unchanged — it is driven by lookup, readdir,
  and getattr, which all still arrive.

## Testing

- **Core unit tests** (`facade.rs`): `passthrough_fd` returns `Some` for an
  open handle under `StructureOnly` and `None` under `Synthesis`; the exposed
  fd refers to the same inode as the backing file (compare fstat dev/ino).
- **FUSE e2e** (new `#[ignore]`d test alongside
  `end_to_end_read_through_mount`): mount StructureOnly on a kernel ≥ 6.9,
  read a file through the mount, assert (a) bytes identical to the backing
  file, and (b) — gated on the `metrics` feature, which musefs-fuse already
  forwards to core (the existing metrics-gated `concurrency.rs` test is the
  template) — the serve-path pread counter (`PREADS`) delta is zero, proving
  the kernel served the reads without calling the daemon. Counter sequencing
  matters because `on_open`/`on_stat` fire during open and other counters
  during mount warmup: open the file through the mount first, then
  `metrics::reset()`, then read, then assert the preads delta against that
  clean baseline. Exercise open → read → close → unmount to cover the
  `BackingId` release path. The test skips (not fails) when the kernel
  predates passthrough, mirroring the silent-fallback contract.
- **Regression:** existing Synthesis e2e tests guard that passthrough stays
  inert outside StructureOnly; no changes to them.
- **Mutation gate:** standard in-diff `cargo mutants` run per CI parity.
- **Benchmark:** before/after StructureOnly sequential-read throughput
  through a real mount, recorded in `BENCHMARKS.md` at landing time.

Known limitation: the old-kernel fallback path cannot be exercised on the
development machine (kernel 7.0). Unit-level coverage of the sticky-disable
flag plus e2e coverage of the modern path is the practical ceiling.
