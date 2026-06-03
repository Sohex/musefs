# musefs Roadmap

## Status: MVP complete; formats, integrations, and a performance/concurrency pass delivered since

musefs is a read-only passthrough FUSE filesystem that presents a virtually
reorganized, re-tagged view of a music library backed by a SQLite store, without
modifying or duplicating the original audio bytes.

### Delivered in v0.1.0

- **Formats:** FLAC, MP3, M4A/M4B, Ogg Opus/Vorbis/FLAC-in-Ogg, and WAV —
  metadata synthesized on the fly from the DB and spliced in front of the
  byte-identical backing audio (no audio bytes copied).
- **Embedded art:** synthesized into the served file and streamed (never
  materialized in memory), content-addressed and deduplicated in the store.
- **Virtual tree:** beets-style `$field` / `${field}` path templates with
  fallbacks and deterministic collision disambiguation.
- **Mount modes:** `synthesis` (default — re-tagged view) and `structure-only`
  (pure passthrough: original backing bytes served verbatim under the templated
  tree).
- **Auto-refresh:** external DB edits (a `scan`, a beets/picard retag on another
  connection) are picked up automatically via `PRAGMA data_version` polling on
  metadata operations — no remount required.
- **Maintenance:** `scan` ingests a backing directory; `scan --revalidate` skips
  unchanged files (preserving external tag edits), prunes tracks whose backing
  file is gone, and garbage-collects orphaned art.
- **CLI:** `musefs scan` and `musefs mount` with `--mode`, `--template`,
  `--default-fallback`, and `--revalidate`.

### Delivered since v0.1.0

- **beets plugin** (`contrib/beets/`) — syncs beets' canonical tags and cover
  art into the SQLite store, keyed by each file's real path, so a live mount
  presents the re-tagged view with no remount and no audio rewrite. It
  auto-scans via the `musefs` binary (no separate `scan` step; `musefs scan`
  also gained single-file support), reconciles file moves/renames (pruning rows
  whose backing file is gone), and runs both as a `beet musefs` command and via
  import/write hooks. Verified end-to-end (beets import + retag + FUSE mount)
  with byte-identical audio.
- **python-musefs** (`contrib/python-musefs/`) — the shared store-contract
  library both plugins depend on for schema checks, tag/art writes, art
  content-addressing, path normalization, and the scan shell-out. beets
  installs it via pip; Picard vendors it into `musefs/_common/`.
- **picard plugin** (`contrib/picard/`) — a MusicBrainz Picard plugin whose
  "Sync to musefs" context-menu action pushes Picard's in-memory tags and front
  cover into the SQLite store, keyed by each file's canonical path, without ever
  saving (rewriting) the audio file. Picard has no pre-save hook, so the action
  is the no-rewrite analog of `beet musefs`: it autoscans each selected file via
  the `musefs` binary to create its row, then writes tags/art the live mount
  surfaces with no remount. Shares the DB-contract logic and the realpath
  path-matching gate with the beets plugin; the sync core is unit/integration
  tested without a Picard install, with the GUI path covered by a documented
  manual smoke test.
- **M4A / M4B (MP4):** metadata synthesized by rebuilding the `moov` atom and
  patching `stco`/`co64` chunk offsets so the `mdat` audio is served
  byte-identically. Embedded art included.
- **Ogg container — Opus, Vorbis, and FLAC-in-Ogg:** re-tagged VorbisComments
  (Opus `OpusTags`, Vorbis comment header, OggFLAC native blocks) plus embedded
  cover art, with audio served byte-identically. Because a resized metadata
  header changes the Ogg page count, the original audio pages are served verbatim
  except for their page sequence numbers and CRCs, which are patched in place; the
  per-page index is built lazily on first read (constant-memory, cached) so
  `open()`/`stat` do no audio I/O. Cover art is re-embedded without ever holding
  the image in the cached layout — read serves any base64 window from a bounded
  input range, so a full-library scan stays cheap on SSD/HDD/NFS. Multiplexed or
  chained Ogg (more than one logical bitstream) is detected at scan and skipped.
  Verified end-to-end (real FUSE mount + independent demux) for all three codecs,
  including byte-identical cover-art round-trips.
- **WAV (RIFF/WAVE):** the `data` chunk payload is served verbatim, with a
  synthesized front carrying a native `LIST`/`INFO` chunk and an embedded `id3 `
  chunk (full ID3v2 + art). Verified end-to-end (real FUSE mount + byte-identical
  data payload + tag round-trip).
- **Performance, concurrency & caching (optimization pass — all phases complete):**
  a phased pass hardening the filesystem for real-world media-manager and player
  access patterns across HDD/SSD/NFS backing stores. All eight phases (0–7) are
  merged to `main`:
  - **Phase 0 — Baselines:** `criterion` micro-benches so every later phase ships
    a before/after delta.
  - **Phase 1 — Concurrency:** fuser's single dispatch thread offloads blocking
    reads/`getattr`s to a bounded worker pool and replies from the workers, so a
    slow backing read never stalls metadata ops; the virtual tree is swapped
    lock-free (`ArcSwap`) and each worker opens its own read-only WAL connection.
  - **Phase 2 — Per-handle I/O:** one file open per `open()`/handle, with reads
    keyed by file handle — no per-read `stat`/re-open.
  - **Phase 3 — Caching:** a sharded, byte-bounded O(1) LRU header-layout cache
    plus a size/attr cache, invalidated lazily per track on `content_version`
    change (vanished tracks pruned on refresh).
  - **Phase 4 — Refresh:** `data_version` polling is debounced
    (`--poll-interval-ms`); the tree-build tag query is batched (drops the N+1);
    rebuilds are single-flighted and run off the dispatch thread; and inodes are
    stable across rebuilds (a persistent path→inode allocator), so an open handle
    survives an external edit.
  - **Phase 5 — Kernel/mount tuning:** `Filesystem::init` raises read-ahead and
    background depth and negotiates async-read / parallel-dirops; the entry/attr
    TTL is configurable (`--attr-ttl-ms`), as are `--max-readahead-kib` and
    `--max-background`.
  - **Phase 6 — Bounded memory:** M4A/M4B resolves stream the `moov` atom from
    disk by seeking, never reading the (potentially hundreds-of-MB) `mdat`
    payload — removing the per-resolve memory spike on large audiobooks.
  - **Phase 7 — Safe aggressive caching:** an opt-in `--keep-cache` keeps the
    kernel page cache across opens; an external re-tag auto-invalidates the
    affected inodes on refresh (via the FUSE notifier), so cached bytes are never
    stale. Supersedes the previously-deferred manual `drop_caches` caveat.

  Byte-identical audio held throughout; each phase shipped subagent-driven with
  spec + code-quality + final review and the full `#[ignore]` e2e mount suite
  green on `/dev/fuse`.
- **Fuzzing & property tests:** coverage-guided `cargo-fuzz` targets for every
  format parser (FLAC/MP3/MP4/Ogg/WAV) and the byte-level primitives (Ogg page,
  base64 windowing, VorbisComment), plus `proptest` invariants — panic-freedom,
  the byte-identical audio guarantee, and tag round-trip — an end-to-end read
  fidelity property, and a `mutagen` interop check that an independent reader
  sees the tags we synthesize. The fuzzers run per-PR (build + smoke) and on a
  weekly schedule with an accumulating corpus. Fuzzing already caught and fixed
  three robustness bugs: an MP4 box-bounds integer overflow (a release-build
  silent wrap), an `id3`-crate unbounded allocation reachable via MP3 and WAV's
  embedded `id3 ` chunk, and an unbounded `Vec::with_capacity` in VorbisComment
  parsing — all DoS-class issues on adversarial files.

---

## Open issue backlog (prioritized)

Suggested order for the currently-open issues. Driving logic: stop active data
loss first → make the cheap policy decisions that shape later code → fix
correctness in batches by code area → then perf/cleanup → docs last. The plugin
(Python) and core (Rust) tracks are independent codebases and can run in
parallel. Most of the Rust items came out of the v1 multi-model review triage.

**Phase 0 — Stop the bleeding** — *done (PR #97)*
- ~~#82 — plugin `replace_tags` wipes scanner-written binary tags~~ — done (PR #97):
  scoped the delete to plugin-owned text rows (`value_blob IS NULL`) in both plugins.

**Phase 1 — Cheap policy decisions (resolve before writing the code they shape)** — *done (PR #97)*
- ~~#96 — mutex poison-recovery policy~~ — done (PR #97): recover-by-reset helpers
  (`lock.rs`) over the VFS serving-path mutexes; shapes the lock code in #89/#90/#94.
- ~~#95 — internal error-type convention (`Result<(),()>`, `InvalidLayout`)~~ — done
  (PR #97): `LayoutError`/`RebuildError` carry diagnostics; convention recorded in
  `CLAUDE.md`, so the new error paths in #91/#92 adopt it from the start.

**Phase 2 — Plugin correctness batch** *(parallel track; shared `_core.py` surface)* — *done (PR #103)*
- ~~#83 — beets reconciliation hook not fail-safe (except breadth + scan timeout)~~ — done
  (PR #103): narrowed the `_reconcile_pending` catch to environmental errors so real
  bugs propagate; `SCAN_TIMEOUT_SECONDS` moved into `musefs_common` and passed on scan.
- ~~#84 — Picard drops multi-value tags~~ — done (PR #103): `map_fields` expands an
  allowlist (`artist`/`albumartist`/`genre`/`composer`) to one store row per value.
- ~~#85 — Picard comma field-map mangling~~ — done (PR #103): `parse_field_map` splits on
  newlines only, so commas inside a value survive.
- ~~#86 — beets `genre`/`genres` duplication~~ — done (PR #103): collapsed the list/scalar
  twins (prefer list, dedupe, preserve order); guarded by a cross-plugin contract test.
- ~~#87 — Picard O(n) subprocess (perf rider on the same plugin pass)~~ — done (PR #103):
  `run_scan` + the Rust `musefs scan` CLI take many targets, so autoscan batches into
  one process under one DB open.

**Phase 3 — Safety net + small Rust hardening** *(low-risk, independent)* — *done (PR #104)*
- ~~#88 — Ogg fuzz art coverage~~ — done (PR #104): the Ogg fuzz target now drives the
  art-synthesis path with arbitrary images, landing coverage before later Ogg touches.
- ~~#93 — `byte_budget` overflow asymmetry~~ — done (PR #104): `acquire`'s increment is
  saturating, matching its guard so the two no longer disagree on overflow.
- ~~#92 — `byte_len` non-negative guard~~ — done (PR #104): art rows with a negative
  `byte_len` (from a malformed external DB write) are skipped with a `warn!`; the
  guard is art-only since binary-tag `byte_len` is `length(value_blob)`, always `>= 0`.
- ~~#91 — MP4 `moov`/`ftyp` alloc cap~~ — done (PR #104): `moov`/`ftyp` metadata
  allocation is capped at 256 MiB with skips logged; the serve path carries a
  dedicated `Mp4MetadataTooLarge` diagnostic (mapped to `EIO`) rather than a generic
  malformed error.
- ~~#94 — DbPool thread-local footguns~~ — done (PR #104): `with()` is re-entrancy-safe
  via `Rc<Db>` and open failures carry path context (`CoreError::DbOpen`).

**Phase 4 — Concurrency correctness** — *done*
- ~~#90 — `rebuild_full` holds `inodes` across DB I/O~~ — done: `render_entries`
  does the DB read + render under the pool connection; `rebuild_full` locks
  `inodes` only across the pure-CPU `build_with` (mirrors the incremental path),
  removing the documented lock-order exception.
- ~~#89 — `fire_poll_refresh` floods the threadpool~~ — done: a synchronous
  `poll_due()` gate on the dispatch thread skips submission within the debounce
  window, and a `poll_pending` single-flight gate bounds in-flight poll tasks to
  one (robust even with `--poll-interval-ms 0`).

**Phase 5 — Metrics** — *done*
- ~~#71 — Ogg serve path records no pread/byte metrics~~ — done: every Ogg
  backing read (index scan, CRC probe, header, payload) counts attempt-based
  `preads`/`pread_bytes`, so latency injection and `bench_read_under_latency`
  now cover Ogg.
- ~~#76 — metric counters covered only by direct-call tests~~ — done: serve-site
  tests drive real reads through every counter's segment arm (ArtImage,
  BinaryTag, both OggArtSlice branches, Ogg preads), and CI runs
  `cargo test -p musefs-core --features metrics` on every PR.

**Phase 6 — Performance optimization SPs** *(bench-tracked: before/after in
`BENCHMARKS.md` + the tracking README)*
- #69 — refresh O(library)→O(changed); adjacent to #90 (same facade rebuild path),
  do right after it. Biggest latency win.
- #67 — bounded probe reads an ID3v1 tail per file (scan perf).
- #68 — `ingest_bulk` copies each picture's bytes (scan perf).
- #70 — zero-copy serve path (deferred SP3 residual; largest scope).

**Phase 7 — Docs**
- #64 — README/architecture rework. Last, so it documents settled behavior (and is
  affected by the Phase 2 plugin changes).

---

## Post-MVP (explicitly deferred)

These are intentionally **out of scope for v0.1.0**. They are recorded here so the
boundary stays explicit; none are half-built in the codebase.

### Formats

- All currently targeted formats (FLAC, MP3, M4A/M4B, Ogg Opus/Vorbis/
  FLAC-in-Ogg, and WAV) are delivered — see above. Remaining edges: FLAC-in-Ogg
  only handles the standard `0x7F "FLAC"` 1.x mapping, and chained/multiplexed
  Ogg is intentionally skipped rather than synthesized.
- **WAV (RIFF/WAVE)** is delivered: the `data` chunk payload is served verbatim,
  with a synthesized front carrying a native `LIST`/`INFO` chunk and an embedded
  `id3 ` chunk (full ID3v2 + art). Out of scope: RF64/BW64 (>4 GiB), preserving
  non-essential chunks (`bext`/`cue`/`smpl`), and seek-based scanning of large
  files.

### Editing / writability

- **Writable mount** — intercepting inbound tag writes through the FUSE layer.
  The MVP is read-only; editing happens out-of-band against the SQLite schema
  (e.g. via beets/picard), which auto-refresh then surfaces.
- **Manual per-track path overrides** — only clean with a writable filesystem;
  follows the writable-mount work.

### Distribution / integration

- **picard plugin** — delivered (see "Delivered since v0.1.0"). Both the beets
  and picard plugins now write the SQLite contract.

### Operations

- **Explicit `musefs refresh` / SIGHUP command** — a manual "rebuild now"
  fallback. Deferred because automatic `data_version` polling already covers
  external edits without remounting, and signal handling inside fuser's blocking
  session loop was not worth the complexity for the MVP. Revisit only if a
  forced, synchronous rebuild proves necessary in practice.

---

*The original design spec lives at
`docs/superpowers/specs/2026-05-24-musefs-design.md`; the Ogg container work is
specced at `docs/superpowers/specs/2026-05-26-ogg-container-support-design.md`;
the performance/concurrency pass is specced at
`docs/superpowers/specs/2026-05-26-optimization-pass-design.md`. Per-milestone
implementation plans are under `docs/superpowers/plans/`.*
