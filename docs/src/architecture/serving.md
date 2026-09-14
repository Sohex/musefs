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
This is how the cardinal invariant holds end to end. Layouts that stream any
payload from the DB by rowid — binary tags **and** art (`ArtImage` /
`OggArtSlice`) — are flagged (`RegionLayout::streams_db_rowid`) so the reader
wraps those reads in a single WAL snapshot with a `content_version` recheck.
A concurrent retag (delete + reinsert reusing a freed rowid) cannot interleave
bytes from two generations of a tag or splice the wrong image. Both the
per-handle fast path and the stateless no-fh fallback apply the guard, and both
validate the backing fd against the resolved stamp *after* acquiring its bytes,
so a rewrite that lands mid-read fails that read rather than the next one
([#682](https://github.com/Sohex/musefs/issues/682)).

### Backing read-ahead

Every backing read — `BackingAudio` splices and the `serve_ogg_window` page walk
alike — flows through a single `BackingReader` (`read_append` on the splice paths)
(`musefs-core/src/readahead.rs`). It caches *raw backing-file bytes keyed by
absolute backing offset* in a per-handle adaptive window: a sequential miss reads
one large `pread` (geometric growth up to a per-stream cap) instead of the
≤256 KiB FUSE chunk, so a high-latency backing client (NFS, remote) can pipeline
the RPCs behind one syscall; a seek resets the window to the floor. All handles
draw from one process-wide RAM budget (`--read-ahead-budget-mib`, default 64) with
deadlock-free `try_lock` LRU eviction. Keying on the absolute backing offset (not
the synthesized output) makes the cache retag-immune. The windows carry no stamp
of their own, though, and serving validates the held descriptor, not the window:
a backing file rewritten in place and then restamped (`musefs revalidate`,
`scan --force`) matches that descriptor again, so a window cached before the
rewrite would pass the post-read `validate_opened_backing` re-stat. A handle
that re-resolves onto a different stamp therefore drops its windows, and moves
its prefetch epoch so an in-flight prefetch of the old bytes is refused, before
it serves the new layout. Every backing byte a read serves was then read under
the stamp that read validates against, so the cardinal audio-bytes invariant and
freshness semantics are untouched. An optional Phase-2
background-prefetch layer (`--read-ahead-prefetch`) exists and is off by default:
amplification alone carries the win on local and low-latency backing, while the
threads add a measured ~30 % on top of it only once per-read latency is high
enough for the overlap to hide something (see
[the backing read-ahead benchmarks](../benchmarks.md#backing-read-ahead-255)).

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

## Directory listings

`opendir` builds a directory's entries once and holds them, so a paginated
`readdir` is a map lookup rather than a fresh tree walk per call — enumeration
stays O(n) in the directory, not O(n²). Handles are capped at 1024 concurrent
so a client that opens directories without closing them cannot pin unbounded
memory.

Handles do not each pay for their own copy. A listing is keyed by the directory
and the virtual-tree generation it was built from, and every handle that agrees
on both shares it: a thousand opens of one directory cost one listing and a
thousand refcounts, not a thousand copies of a listing whose size scales with
the directory's width. All but the first also skip the tree walk outright,
which is what an over-cap `readdir` consults before rebuilding. `opendir` pins
the generation it read, so the address that identifies it cannot be reused
while any handle still names it, and a refresh simply means the next `opendir`
builds against the new generation while open handles keep serving the view they
were opened on. `musefs_dir_listings` is the distinct-listing count behind
`musefs_dir_handles`: the gap between them is the sharing, and equality means
every open handle is on a different directory.

Over that cap, `opendir` degrades rather than failing: it returns the stateless
handle. Listings stay complete — parallel walkers routinely exceed 1024
concurrent directory handles on a large mount — and they stay stable too. A
stateless handle cannot tell one enumeration from another, so the first
`readdir` of an enumeration pins the current generation's listing in a small
shared cache instead, and tags the cookies it hands out with that generation;
every later page resolves its cookie back to the same listing. Paging whatever
generation was current, as it used to, let a refresh landing between two pages
shift entries under the cursor, so one enumeration could return an entry twice
or skip one ([#695](https://github.com/Sohex/musefs/issues/695)). The cache
holds 64 listings. Evicting one costs its enumeration nothing while no refresh
has landed: the same generation's listing is rebuilt and the enumeration resumes
where it was. If a refresh *has* landed, the listing the cookie points into no
longer exists, and paging the new one at the old index would repeat or skip
entries, so that `readdir` fails with `ESTALE` instead. A client sees "stale file
handle" for that one directory, and a fresh enumeration of it succeeds. It takes
more than 64 stateless enumerations in flight at once and a store change
mid-listing, which is rare even for a large parallel walk; an error it can see
is the better failure than a listing that is silently wrong. The work
runs on the worker pool, like every other blocking operation, not on the single
fuser dispatch thread. `musefs_dir_handle_rejections_total` counts the opens that took the
fallback; the `musefs_dir_handles` gauge cannot show this, because
saturation is bursty enough to read healthy in every sample while thousands of
opens are degraded between them.

`readdirplus` answers the same listing with each entry's attributes inline, so
a client that stats what it lists — `ls -l`, and every media scanner — spends
one round trip on the directory rather than one more per entry. The mount
requests `FUSE_READDIRPLUS_AUTO` alongside it, which lets the kernel fall back
to plain `readdir` when the caller is not statting: an attribute-laden entry is
several times the size of a bare one, so fewer fit in a reply page, and for a
bare `ls` that is a pessimization rather than a saving.

Directories cost nothing to answer — `getattr` returns immediately for one
without touching the store — and the synthetic entries have static attributes,
so only the file entries need work. Those fan out across the worker pool, in
rounds of 64, and the reply is assembled by whichever resolution finishes last.
The fan-out is the point: concurrent `lookup`s already spread across the pool,
so resolving a page serially would be slower for a threaded scanner than the
round trips it removes. It is also why nothing here waits — a worker blocking
on tasks it queued to its own bounded pool could deadlock behind them, so a
round is a countdown rather than a join. Rounds exist because the reply buffer's
size is not visible to the handler: resolving a whole wide directory to fill one
page would front-load enormous latency onto the first call, and anything
over-resolved lands in the size cache for the page that does ask for it.

An entry is never sent with attributes that are not the file's own. A zero TTL
does not make them harmless: the kernel applies a `readdirplus` entry's
attributes to the inode it already holds for that name whatever the timeout, so
the size-0 placeholder musefs used to send truncated the page cache of a file
another process had open, and a program with the file mapped was killed by
`SIGBUS`. Instead the reply page ends before the first entry that has no
attributes, and the kernel asks again from that entry's cookie. A short page is
just a short page; the two other ways to stop would each lose names. An empty
reply reads as the end of the directory, and an error fails the whole
`getdents`.

Every page still makes progress. A page's first entry is resolved even when
the pool is over its admission cap (below), and an entry whose resolution
failed partway down a page is tried again as the first entry of the next one,
which is what a transient failure, a blip on an NFS backing, needs. An entry
that still cannot be resolved as a page's first is listed with the attributes
musefs last sent the kernel for that inode, and a zero TTL. The mount keeps
those attributes for every file inode from each `lookup`, `getattr` and
`readdirplus` reply, once the kernel has negotiated `readdirplus` at all, and
drops them when the kernel forgets the inode or a refresh invalidates it. Linux
sends that `FORGET` when it evicts the inode, and also for an entry it was sent
but could not link, so every count a reply hands it comes back. The record
therefore never outgrows the file inodes the kernel caches, at about 150 to 300
bytes each against the tree's ~1.3 KB per track. A kernel that never negotiates
`readdirplus`, such as FreeBSD's, never gets a record, which also matters
because FreeBSD can answer a `lookup` it then fails to link without sending
`FORGET`. If the kernel holds
attributes for the inode at all, it holds those, so linking them changes
nothing, and the zero TTL sends the client's next access back to `getattr` and
its real error. The file neither vanishes from `ls` nor stalls the listing.
None of this changes what `getattr` serves or when its size cache lets go of a
stale entry.

Only an inode with no such record is listed with a size no file can have. The
kernel emits the name and refuses the entry before it links anything, in
`fuse_invalid_attr`. That check arrived with "fuse: verify attributes"
(eb59bd17, Linux 5.5) and was backported to 5.4.3, 4.19.89, 4.14.159, 4.9.207,
4.4.207 and 3.16.85. musefs documents no minimum kernel. On a kernel without
the check — 3.10, a series that reached end of life before December 2019, or a
vendor kernel that did not take the patch — the size would be written into an
inode the kernel holds as a negative `i_size`, and that inode's page cache
truncated. That is why the out-of-range size goes only where the kernel holds no
attributes musefs sent: an inode it has never been told about, or one whose
attributes it has discarded, such as an inode a refresh handed to another track
([#778](https://github.com/Sohex/musefs/issues/778)). The over-cap path depends
on none of this.

### Admission to the worker pool

The pool's queue is unbounded, so nothing reaches it ungated. Reads reserve one
of 1024 in-flight slots first and are refused with `EAGAIN` over that. Every
other job — `lookup`, `getattr`, `open`, `opendir`, a stateless listing, a
`readdirplus` round's resolutions — passes one admission gate capped at 4096
queued or running ([#694](https://github.com/Sohex/musefs/issues/694)), and none
of it is refused. A job that finds the gate full runs on the thread that
submitted it. From the dispatch thread that is the backpressure: fuser reads no
further request until the job is done, so the backlog waits in the kernel
rather than in musefs' memory. The one exception is a `readdirplus` entry's
attributes, other than the first of a reply page: over the cap those are not
run at all, and the page ends before them, as above. The kernel asks again for
the rest, and the next page's first entry runs in place. Running every entry in
place would let a wide directory chain round after round on one thread.
`musefs_pool_over_cap_total` counts the jobs that met the cap; on a healthy
mount it stays at zero.

Store refreshes run on a lane of their own, a single thread, so a metadata
backlog never delays freshness and a refresh never runs in place on the
dispatch thread, where its kernel invalidations are written.
`musefs_readdirplus_total` counts the calls; zero means the kernel is not using
the op, which is otherwise invisible from the daemon since the capability is
negotiated at mount.

## What a synthesized file's timestamp promises

A served file's bytes come from two places — the backing file, and the tags and
art in the store — so its mtime has to move when *either* does. The mount
reports the later of the backing file's second and the row's `updated_at`, which
covers a backing rewrite and a metadata edit alike. `updated_at` moves only when
the store records a change: a re-probe that finds the file exactly as recorded
leaves it, and `content_version`, alone
([#757](https://github.com/Sohex/musefs/issues/757)), so a revalidate over
unchanged files does not make them look modified.

Whole seconds are not enough on their own. Every trigger stamps `updated_at`
with `strftime('%s','now')`, so two metadata edits inside one wall-clock second
leave the same second behind; if they happen to synthesize to the same length —
which same-length tag rewrites routinely do — the file looks untouched to
anything comparing size and mtime. And because the reported second is a `max`, a
backing mtime in the future masks every metadata edit for as long as the skew
lasts, which a restored archive or a bad clock on a NAS can sustain
indefinitely.

So the mount reports the row's `content_version` as the timestamp's
**nanoseconds** ([#725](https://github.com/Sohex/musefs/issues/725)). That
counter already increments on every change musefs makes to the served bytes — it
is what every internal cache keys on — so the guarantee it buys is:

> **In synthesis mode, the reported mtime changes whenever a change recorded in
> the store changes the synthesized bytes.**

Each qualifier in that sentence is load-bearing.

*In synthesis mode*, because `--mode structure-only` serves the backing file
verbatim. A tag edit does not change those bytes, so it must not move their
timestamp either — signalling a change there would send every consumer to
re-copy a byte-identical file, which is the same failure in the opposite
direction. Passthrough reports no sub-second part at all.

*A change recorded in the store*, because a backing file rewritten behind
musefs's back bumps no counter. That case is not handled by the timestamp: the
freshness stamp catches it and the serve fails closed with `BackingChanged`
rather than quietly reporting stale attributes. The exception is
`--trust-backing-mtime`, which opts out of the `getattr` re-stat and so accepts
exactly that staleness until the next `open`
([Freshness](tree-scanning.md#freshness-two-version-counters)).

Two further limits. The nanosecond field is a change counter, not a duration: it
does not measure anything, and two versions exactly one billion apart report the
same one. And a consumer that truncates to whole seconds gets exactly what it
got before — including the future-backing-mtime masking — because the second is
unchanged. The precision exists for tools that read a full `timespec`.

Nothing in the store holds nanoseconds. `updated_at` is still whole seconds, and
the sub-second part is derived where the timestamp is built, so no column claims
a precision nobody wrote.

**A pre-epoch backing file is stored as one.** An archival rip or a restored
backup can carry an mtime before 1970, and the store accepts it from v4 on
([#696](https://github.com/Sohex/musefs/issues/696)). A synthetic directory has
no row and therefore no timestamp; it reports the mount time. Those two cases
used to be the same value — zero — so a file whose mtime really was the Unix
epoch reported the mount time instead.

What a pre-epoch file reports depends on the mode. `--mode structure-only`
serves the backing file's own mtime, pre-1970 seconds included. Synthesis mode
reports the later of that and the row's `updated_at`, the second the store last
changed the file, which is never before 1970. So there a pre-epoch file reports
a second after 1970.

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
footprint, refresh health, suppressed serve-path warnings), `FuseTelemetry`
(uptime, read/dir-handle gates,
worker pool, passthrough state), and optional jemalloc/syscall counters
(including read-ahead hit/miss) — see
[`musefs-core/src/telemetry.rs`](../../../musefs-core/src/telemetry.rs) for the full
metric list. This namespace deliberately bypasses the virtual tree
(`VirtualTree`) and the `RegionLayout` / segment model: it is injected into
root-directory `readdir` and resolved by direct inode checks, so the cardinal
audio path is untouched.

Injecting into `readdir` while intercepting `lookup` is only coherent because
the tree cannot supply a root child of the same name: `.musefs-metrics` and the
Spotlight marker are reserved in the virtual-tree namespace, and a track that
renders to one is ranked to ` (2)` at build time — see
the [virtual tree](tree-scanning.md#virtual-tree) (#681). The injected
entry therefore needs no dedup, and the two surfaces cannot disagree.
