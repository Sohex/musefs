# The serving model

## The segment model

A synthesized virtual file is described by a `RegionLayout`
(`musefs-format/src/layout.rs`): an ordered list of `Segment`s whose lengths
sum to the served file size. Six variants:

- `Inline(Vec<u8>)` — generated framing/text bytes (an ID3v2 tag, FLAC
  metadata blocks, a RIFF front), fully materialized at resolve time.
- `ArtImage { art_id, len }` — embedded cover art; only the length lives in
  the layout. Image bytes stream from the DB blob in chunks at read time and
  are never buffered whole. This invariant also holds for Ogg synthesis,
  where page CRCs are computed from page-bounded `ArtSource` windows
  (previously the documented exception).
- `BackingAudio { offset, len }` — a run of the original file's audio frames,
  served by positioned reads (`read_exact_at`) against the backing file.
- `OggAudio { offset, len, seq_delta }` — original Ogg audio pages served
  with each page's sequence number shifted by `seq_delta` and its CRC
  recomputed in place (a resized header changes the page count). The byte
  length is unchanged — renumbering patches, never recopies.
- `OggArtSlice { art_id, offset, len, base64, art_total }` — a window of an
  embedded picture served lazily from the blob store; when `base64`, the
  window is base64-encoded incrementally at read time.
- `BinaryTag { payload_id, len }` — an opaque binary tag payload (e.g. an ID3
  `PRIV` frame body or a FLAC `APPLICATION` block body) streamed from the DB
  at read time.

`read_at` (`musefs-core/src/reader.rs`) serves a byte range by walking the
segments and splicing: inline bytes are copied, art and binary-tag payloads
are read from the DB in chunks, backing audio comes from positioned reads of
the original file, and Ogg pages are renumbered and CRC-patched in flight.
This is how the cardinal invariant holds end to end. Layouts that stream
binary tags are flagged (`RegionLayout::has_binary_tag`) so the reader can
wrap those reads in a transactional `content_version` guard — a concurrent
retag cannot interleave bytes from two generations of a tag.

### Backing read-ahead

Every backing read — `BackingAudio` splices and the `serve_ogg_window` page walk
alike — flows through a single `BackingReader::read_exact_at`
(`musefs-core/src/readahead.rs`). It caches *raw backing-file bytes keyed by
absolute backing offset* in a per-handle adaptive window: a sequential miss reads
one large `pread` (geometric growth up to a per-stream cap) instead of the
≤256 KiB FUSE chunk, so a high-latency backing client (NFS, remote) can pipeline
the RPCs behind one syscall; a seek resets the window to the floor. All handles
draw from one process-wide RAM budget (`--read-ahead-budget-mib`, default 64) with
deadlock-free `try_lock` LRU eviction. Keying on the absolute backing offset (not
the synthesized output) makes the cache retag-immune, and serving still flows
through the per-read `validate_opened_backing` re-stat, so the cardinal
audio-bytes invariant and freshness semantics are untouched. An optional Phase-2
background-prefetch layer (`--read-ahead-prefetch`) exists but is off by default —
read amplification carries the whole win (see
[BENCHMARKS.md](../benchmarks.md#backing-read-ahead-255)).

How each format builds its layout differs enough to warrant its own document:
[FLAC](../formats/flac.md), [MP3](../formats/mp3.md), [M4A](../formats/m4a.md),
[Ogg](../formats/ogg.md), [WAV](../formats/wav.md).

## Mount modes

`musefs_core::Mode` selects one of two behaviors at mount time:

- **`Synthesis`** (default) — the metadata region is generated from the DB
  and spliced ahead of the backing audio, as above. Resolve-time validation
  guards the stored audio bounds: if `audio_offset + audio_length` runs past
  the backing file's current length, the row no longer matches the file and
  the resolve fails with a controlled `BackingChanged` error.
- **`StructureOnly`** — pure passthrough: the layout is a single whole-file
  `BackingAudio` segment, so the original bytes are served verbatim under the
  templated tree. Stored audio bounds are irrelevant (the whole file is
  served) and are not validated in this mode.

In `StructureOnly` mode, on kernels with FUSE passthrough (6.9+) and a daemon
holding `CAP_SYS_ADMIN` (kernel-gated: run as root or
`setcap cap_sys_admin=ep` the binary), each open registers the backing fd
with the kernel and reads bypass the daemon entirely. The capability check is
performed at mount time and its absence pre-announced; if registration fails
at runtime anyway, passthrough is disabled for the rest of the session
(later opens skip the doomed ioctl) and reads fall back to the daemon
silently. Freshness for a passthrough handle is open-time-only — it is a
plain POSIX fd onto the backing file. In `Synthesis` mode no single fd
represents the spliced bytes, so passthrough never applies.

## Synthetic telemetry namespace

When `--expose-metrics` is on, the root directory gains a synthetic
`.musefs-metrics/` entry backed by reserved inodes at `u64::MAX - 1` (dir) and
`u64::MAX - 2` (file) — the same "top of the u64 space" trick the Spotlight
marker uses, since `InodeAllocator` starts at 2 and only increments. The
directory and file are disjoint from the macOS Spotlight marker at `u64::MAX`.

The metrics file is `/proc`-style: it advertises `st_size == 0` and is served
via `FOPEN_DIRECT_IO`, so readers must read to EOF rather than trusting the
stated size. Content is rendered at `open` time from a snapshot of
`CoreTelemetry` (header/size caches, read-ahead budget/charge, virtual-tree
footprint, refresh health), `FuseTelemetry` (uptime, read/dir-handle gates,
worker pool, passthrough state), and optional jemalloc/syscall counters
(including read-ahead hit/miss) — see
[`musefs-core/src/telemetry.rs`](../../../musefs-core/src/telemetry.rs) for the full
metric list. This namespace deliberately bypasses the virtual tree
(`VirtualTree`) and the `RegionLayout` / segment model: it is injected into
root-directory `readdir` and resolved by direct inode checks, so the cardinal
audio path is untouched.
