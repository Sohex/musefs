# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

> The `contrib/` Python packages have their own decoupled version and changelog:
> see [the contrib changelog](integrations/overview.md#contrib-changelog).

For curated, upgrade-focused notes (highlights and per-version migration steps),
see the [Release notes](release-notes.md).

## [Unreleased]

## [2.0.0] - 2026-09-14

### Added

- **`musefs migrate --repair`, and the row-rejection pre-flight it exists for.**
  The 2.0.0 migration tightens constraints an existing store can already
  violate — an embedded NUL in a tag key or an art mime, a picture dimension
  past `u32`, a value whose storage class is not what the Rust side reads. Such
  a row aborts the migration, atomically, which is safe but a poor way to find
  out: the upgrade can have been copying blobs for a while before it meets one.

  So `migrate` now checks first, before anything is copied or written, and
  reports what would be refused per table. It does **not** repair on its own —
  dropping a row an external writer chose is exactly the class of thing that
  must not happen unasked — so it stops and names `--repair`. With that flag the
  rows are deleted *after* the snapshot is taken, so they are still in the copy
  the user can go back to, and the report says plainly that deleting a track
  takes its tags and art links with it.

  The check does not describe the constraints a second time. It builds the
  target tables using the migration itself — a scratch store migrated to the
  target is by definition the shape this one is about to become — attaches them
  to the store, and offers every row to them with `INSERT OR IGNORE`, which
  skips exactly what a `CHECK`, `NOT NULL` or `UNIQUE` would refuse. What did
  not arrive is the answer. A hand-written copy of the rules would have drifted
  the first time one changed, and there were seven issues' worth of changes to
  drift from.

  Foreign keys are off for the pass, because `INSERT OR IGNORE` cannot help with
  them: conflict resolution does not apply to foreign keys, so a violation
  aborts the statement rather than skipping the row. That leaves one shape the
  row-by-row pass cannot see — a child pointing at a parent that is not there,
  which an external tool can leave behind with enforcement turned off, and which
  fails the refill when the upgrade puts it back. A second pass covers it, and
  covers the same question for a child whose parent this repair is about to
  delete, since to a child those are the same thing.

  So the count is what will actually go, children included, rather than only the
  rows with something wrong of their own. Deletes run in an order the references
  require: `tracks` first so its cascade takes what it owns, and `art` last,
  because `track_art.art_id` references it with no `ON DELETE` clause and
  deleting a refused blob while a link survives fails outright.

  `--repair` refuses alongside `--no-snapshot`. The rows go for good, and the
  snapshot is the only copy they survive in.

- **`musefs migrate`.** The command the gate above names: an explicit,
  confirmed store upgrade. It refuses a store anything else has open, reports
  the current version, the target version and what each pending step does, and
  reports the free space the upgrade needs against what the filesystem has,
  refusing up front rather than failing part-way through the transaction.
  Before touching anything it writes a compacted copy of the store to
  `<db>.v<version>.bak` with `VACUUM INTO`, which is what turns an irreversible
  upgrade into a reversible one; `--snapshot PATH` moves it and `--no-snapshot`
  skips it, and an existing destination is refused rather than overwritten.
  Afterwards it reports that the store grew and offers a vacuum, and reports
  how many tracks lost the fingerprint this upgrade retires and offers a
  `revalidate` over the directory the library shares, which recomputes them.
  Prompts come from `dialoguer`, the console-rs sibling of the `indicatif`
  dependency the scan progress bar already uses, so a question is lifted clear
  of a live progress frame by the same path a log record takes. Off a terminal
  nothing blocks on stdin: the upgrade needs `--yes` or it refuses and names
  the flag, and the two offers decline themselves unless `--vacuum` or
  `--revalidate` asked for them
  ([#705](https://github.com/Sohex/musefs/issues/705)).

- `readdirplus` is implemented, folding the per-entry `lookup` into the
  directory read: a client that stats what it lists — `ls -l`, every media
  scanner — spends one round trip on the directory instead of one more per
  entry. Expect a few tens of percent off a repeat traversal rather than a
  multiple; a cold one is dominated by synthesis, where the round trip is a
  few percent. Directories and the synthetic entries are answered inline, and
  the file entries fan out across the worker pool in rounds, because concurrent
  `lookup`s already spread across that pool and a serially resolved page would
  be slower for a threaded scanner than what it replaces.
  `FUSE_READDIRPLUS_AUTO` is requested too, so the kernel keeps using plain
  `readdir` for a listing nobody stats, where the larger entries would only cost
  reply pages. `musefs_readdirplus_total` reports whether the kernel is sending
  the op at all
  ([#667](https://github.com/Sohex/musefs/issues/667)).

- `--trust-backing-mtime` skips the backing re-stat that `getattr` performs on
  a metadata-cache hit, serving the cached size and mtime instead. Off by
  default, and scoped to `getattr` alone. The re-stat exists to catch an
  on-disk change that left `content_version` untouched
  ([#279](https://github.com/Sohex/musefs/issues/279)), which is the right
  default and stays the default; it has no escape hatch for backings where a
  `stat` is not roughly a microsecond. On NFS, SMB, or a spun-down array it is
  a network round trip or a head seek, and a warm cache does not help: a
  scanner walking ten thousand tracks pays ten thousand synchronous stats on
  every pass, and `--attr-ttl-ms` cannot debounce them because each track is
  stated once per traversal and a traversal outlives any TTL worth setting.
  `open` and the read paths validate unconditionally either way, so a replaced
  backing file is still caught before a byte is served, and the cold traversal
  that populates the cache stats regardless — the hit-path stat is the cost of
  every pass *after* the first, not of the first.
  `musefs_trust_backing_mtime` reports the flag state, which is what tells a
  quiet `musefs_backing_stats_total` from a disabled counter
  ([#668](https://github.com/Sohex/musefs/issues/668)).

- Chaptered `.m4b` files are supported. A `moov` may now hold chapter tracks
  (`text`, `sbtl`) alongside its single audio (`soun`) track, and every track's
  `stco`/`co64` chunk offsets are relocated when the `moov` is regenerated, not
  just the first track's. A Nero chapter list (`moov/udta/chpl`) is copied
  verbatim into the rebuilt `udta`. Previously every chaptered file — which is
  to say most of an audiobook library, and chapters are why the `.m4b`
  extension exists — was counted as `unparseable` at scan time
  ([#672](https://github.com/Sohex/musefs/issues/672)).

- `Musefs::drain_prefetch` waits for the Phase-2 prefetch pool to finish every
  job it accepted, or a timeout to elapse. Serving never needs it — prefetch is
  fire-and-forget there — but sampling the prefetch counters without it misses
  reads still in flight, and a caller that owns the backing filesystem itself
  (the latency-injecting mount the read benches use) can otherwise tear it down
  under a worker mid-read and park that thread in uninterruptible sleep (#671).

- `musefs_readahead_prefetch_reads_total` and
  `musefs_readahead_prefetch_bytes_total`
  ([#671](https://github.com/Sohex/musefs/issues/671)) report the positioned
  backing reads issued by the Phase-2 prefetch workers. The serve-path
  `musefs_backing_pread_*` counters never saw those threads, so a prefetcher
  re-reading the stream several times over was invisible to every counter the
  daemon exposes and showed up only as backing I/O nothing accounted for. That
  is exactly the shape of the amplification bug fixed in this release, which is
  why the counters exist.

- `musefs_dir_handle_rejections_total` ([#626](https://github.com/Sohex/musefs/issues/626)), a monotonic counter of
  `opendir` calls that could not be given a cached directory snapshot. The
  existing `musefs_dir_handles` gauge cannot stand in for it: saturation is
  bursty, and a walk that produced 7,525 rejections never showed a gauge sample
  above 593. It is also the signal that the stateless path from
  [#616](https://github.com/Sohex/musefs/issues/616) is in use and directories are being rebuilt on every
  `readdir`.

- `musefs_serve_warns_suppressed_total`
  ([#653](https://github.com/Sohex/musefs/issues/653)), a monotonic counter of
  serve-path failure warnings the rate limiter downgraded to `debug`. The count
  previously escaped only as a parenthetical inside the next admitted warning,
  which is both unscrapeable and carried by the admitted lines alone. The
  failure mode matches
  [#626](https://github.com/Sohex/musefs/issues/626)'s: suppression is bursty by
  construction — 10 admitted per 30-second window, the rest dropped — so a
  scrape landing between bursts sees nothing, and "quiet" and "failing faster
  than it can log" look identical. Since the limiter moved into `musefs-core`
  ([#650](https://github.com/Sohex/musefs/issues/650)) the counter covers the
  synthesis warns too, not just the FUSE errno path.

- `--workers` (env `MUSEFS_WORKERS`) sizes the FUSE worker pool explicitly
  ([#631](https://github.com/Sohex/musefs/issues/631)). The default stays auto,
  twice the CPU count, because the work is I/O-bound, but each worker lazily
  opens its own read-only SQLite connection, so steady-state memory scales with
  the pool rather than with the library alone. A many-core host serving few
  concurrent readers can now cap that component instead of paying for
  connections it never uses.

- `musefs_process_resident_bytes` (Linux) reports the whole process's resident
  set, and `musefs_sqlite_memory_bytes` what SQLite holds across all connections
  ([#631](https://github.com/Sohex/musefs/issues/631)). SQLite allocates through
  libc rather than the global allocator, so the jemalloc `musefs_alloc_*` gauges
  never saw it: a full-library walk grew the process by hundreds of megabytes
  while those gauges barely moved, and the metrics surface could not say how
  much memory the daemon was using.

### Changed

- **The serve path no longer zero-fills buffers a read is about to overwrite**
  ([#670](https://github.com/Sohex/musefs/issues/670)). Each backing-audio
  segment and Ogg audio page a read touched was zero-filled and then overwritten
  by the positioned read, and each read-ahead window was allocated zeroed and then
  filled — about 1.7 µs per 128 KiB segment and 69 µs per 8 MiB window as
  measured in the issue, roughly 18% of a page-cached fill. Those reads now land
  in the buffer's uninitialized spare capacity instead, committed by one audited
  `unsafe` `set_len` covering only the bytes `pread` reports initialized, and a
  read served from a cached window is copied straight into the output. Against
  high-latency backing the I/O dominates and nothing visible changes; against
  page-cached or NVMe reads it is one of the few costs left on the path.
  `musefs-core` now depends on `rustix` directly.

- **The public enums a downstream crate matches on are `#[non_exhaustive]`**
  ([#708](https://github.com/Sohex/musefs/issues/708)). Until now adding a
  variant to any of them was a breaking change, so the next audio format or
  error case would have cost a 3.0.0. A `match` on one of these outside its
  crate now needs a wildcard arm:
  - errors: `DbError`, `CoreError`, `FormatError`, `LayoutError`,
    `TemplateError`;
  - `Format`, so a new audio format is a minor release;
  - scan and mount inputs: `ChecksumTier`, `MatchStrictness`, `Mode`,
    `NodeKind`, and the `ScanProgress` events;
  - `musefs-cli`'s `Command`, `CliMode`, `ChecksumMode` and `MatchMode`.

  Deliberately left exhaustive, because a new variant there should fail to
  compile rather than reach a wildcard: `Segment`, which `read_at` splices audio
  bytes from; `Extent`, a two-state probe protocol; `ogg::Codec` and
  `Mp4ScanError`, which the scanner and reader translate variant by variant; and
  `WarnDecision`, which `serve_warn!` matches inside other crates. Where the
  workspace itself lost an exhaustive check across a crate boundary, it has a
  stand-in: a test fails as soon as `Format` gains a variant, naming the
  synthesis dispatch to add it to, and `musefs-fuse` maps a `CoreError` it has
  not placed to
  `EIO`, the collapse it already documents for structural errors.

- **Public structs that grow are `#[non_exhaustive]` too**
  ([#743](https://github.com/Sohex/musefs/issues/743)). #708 did it for enums,
  but adding a field to a public struct with public fields was just as breaking.
  A non-exhaustive struct cannot be built with a literal outside its crate at
  all — not even with `..Default::default()` — so the split follows who builds
  each one. Outside its crate, build a marked configuration struct with
  `default()` and assign the fields you change.
  - Configuration a caller fills in from defaults: `ScanOptions`, `MountConfig`
    (which gains a `Default` matching a bare `musefs mount`, pinned by a test),
    `FuseConfig`, and `musefs-cli`'s `Cli`, `MountArgs` and `MigrateArgs`.
  - Results no other crate builds: `ScanStats`, `RevalidateStats`,
    `CoreTelemetry`, `ProcessStats`, `metrics::Snapshot` and the tree's `Node`;
    `Track`, `TrackIdentity`, `Tag`, `Art`, `ArtMeta`, `BinaryTagRow`,
    `ChangelogRead` and `PendingStep`; and the format scanners' results —
    `FlacScan`, `FlacMeta`, `Mp3Bounds`, `Mp4Bounds`, `Mp4Scan`, `BoxHeader`,
    `OversizeDrop`, `OggHeader`, `OggScan`, `PageHeader`, `PictureDrop`,
    `B64Window` and `WavBounds`.

  Deliberately left exhaustive: the structs another crate builds field by field
  in production — the store's write inputs (`NewTrack`, `NewArt`, `TrackArt`,
  `BinaryTag`, `StructuralBlock`), the synthesis inputs (`ArtInput`,
  `BinaryTagInput`, `TagInput`, `MetadataBlock`, `OggArt`), and the telemetry
  `musefs-fuse` and the binary fill in. A new field on one of those is a value
  every producer must supply, and the compile error is what makes each one do
  it — the reason `Segment` stayed exhaustive. So a new store column remains a
  breaking change for Rust code that writes rows. Also left: the value types
  tests build directly (`Attr`, `VirtualMtime`, `BackingStamp`, `ResolvedFile`,
  `WavScan`, `EmbeddedPicture`, `EmbeddedBinaryTag`).

- **`tracks` is rebuilt by the 2.0.0 store migration.** This is the step that
  makes the upgrade gated: `musefs migrate` runs it, and afterwards the store no
  longer opens with an older musefs. It is one rebuild because SQLite can add
  neither `AUTOINCREMENT` nor a column type nor a changed `CHECK` in place, and
  `tracks` is the only table with children — three tables reference it
  `ON DELETE CASCADE` and thirteen triggers are on it or name it — so it is the
  expensive, once-only event every `tracks` schema change in this release rides.

  The rebuild keeps foreign keys enforced throughout. With enforcement on,
  `DROP TABLE` performs an implicit `DELETE` that cascades into the children and
  fires their delete triggers, so the migration drops all thirteen triggers up
  front, copies the parent and the three children into holding tables, recreates
  `tracks` under its final name (no `ALTER TABLE ... RENAME`, whose schema
  reparse is what V3's `art_ad` dance existed for), refills, and recreates the
  index and the triggers. `content_version` is carried across rather than
  bumped — every cache keys on it, and from this release the served virtual
  mtime derives from it, so an accidental bump would be visible outside musefs
  for a migration that changed no audio.

  What rides it:

  - `tracks.id` becomes `INTEGER PRIMARY KEY AUTOINCREMENT`
    ([#678](https://github.com/Sohex/musefs/issues/678)), which is what forces
    the rebuild. Without it SQLite's default allocator hands a deleted rowid
    back out, and the incremental refresh treats a track id as a persistent
    identity: a pruned track and a freshly ingested replacement collided on the
    reused id, the format *and* the initial `content_version` (a fresh ingest
    bumps a fixed number of times for two files of the same shape), so
    `partition_changelog` classified the whole substitution as unchanged. The
    mount kept listing the pruned track and never showed the new one until it
    was remounted. Reading the ghost returned `EIO` rather than the wrong audio,
    because the backing stamp guard still failed closed.

    An id that is an identity must also not change, and nothing refused
    `UPDATE tracks SET id = …`
    ([#762](https://github.com/Sohex/musefs/issues/762)). Foreign keys stop it
    for a track with children, but a childless track — a file with no tags, art
    or structural blocks — rekeyed freely, including onto an id that had been
    deleted, which is #678's reuse through a different door. And
    `tracks_changelog_au` logged only `NEW.id`, so the refresh classified the
    new id as an addition and never saw the old one leave: the mount listed a
    ghost for the old id beside the rekeyed track. `tracks_reject_rekey` now
    refuses a changed id, in the shape of #717's reparent refusals, and the
    changelog trigger logs `OLD.id` and, only when it differs, `NEW.id` — so
    the refresh is correct on its own terms against a writer that drops the
    refusal, while an ordinary update still spends one ring slot rather than two.
  - `tracks.backing_path` becomes a `BLOB`
    ([#680](https://github.com/Sohex/musefs/issues/680)), and so does the Rust
    model — see the Fixed entry below for the half that stops the mangling. The
    refill casts, which is what preserves identity:
    SQLite never compares a `TEXT` value equal to a `BLOB`, so a refill that
    copied the column unchanged would leave every existing row unreachable to a
    byte-binding reader while the unique index failed to fire, silently giving
    each track a second row. A `typeof` `CHECK` pins the storage class, since
    declaring the column `BLOB` is an affinity and would still accept `TEXT`
    through the same door
    ([#718](https://github.com/Sohex/musefs/issues/718)).
  - `tracks.backing_ino` is added
    ([#674](https://github.com/Sohex/musefs/issues/674)). On filesystems that
    truncate sub-second timestamps — ext3, HFS+, some SMB and NFS mounts — a
    same-size in-place rewrite inside the granularity window left all three
    stamp fields identical and the freshness guard passed on changed bytes. The
    inode catches the *replacement* shape, which is what almost every tagger
    actually does (write a temporary file, rename over the original). Zero is
    the sentinel for "not yet known", matching the `backing_ctime_ns`
    precedent, so an upgraded store is not taken dark and the guard arms per row
    as scans happen. It joins `tracks_geometry_au`'s bump set, since a changed
    inode means the backing file was replaced.

    It is recorded only where the filesystem keeps inode numbers
    ([#757](https://github.com/Sohex/musefs/issues/757)). FAT and exFAT assign
    one each time a file enters the inode cache, so every remount renumbers an
    untouched file, and recording it there would have failed every serve after
    a replug until a revalidate. The scanner checks the probed file's filesystem
    type and records no inode on those two, and `revalidate` asks the same
    question live rather than re-probing such a row on every pass. No device
    number is recorded beside the inode: the kernel reassigns device numbers
    across reboots, which would fail a whole library at once. FAT and exFAT
    are now documented as not recommended for backing storage.
  - The lower bounds on `backing_mtime_ns` and `backing_ctime_ns` are dropped
    ([#696](https://github.com/Sohex/musefs/issues/696)), so a file dated before
    1970 — an archival rip, a restored backup, anything whose mtime came from
    the original media's metadata — reaches the mount instead of probing
    cleanly and then dying at a `CHECK` as a failed file.
  - Storage-class constraints land on every column
    ([#718](https://github.com/Sohex/musefs/issues/718)). SQLite's declared
    types are affinities, so a row satisfying every `CHECK` could still be a
    Rust conversion failure — which surfaces as a store-wide error rather than
    as the malformed row it is.
  - Both checksum columns ban an embedded NUL
    ([#693](https://github.com/Sohex/musefs/issues/693)). `length()` on `TEXT`
    counts characters only up to the first NUL, so 64 valid hex characters
    followed by NUL and any amount of anything satisfied `length() = 64` while
    storing something that is not a 64-character identity.
  - Every fingerprint is retired
    ([#691](https://github.com/Sohex/musefs/issues/691)), folded into the refill
    rather than run as a statement of its own — the refill rewrites every row
    anyway. `musefs migrate` reports how many rows are owed a rescan and offers
    to run one.

  `content_hash` is the one column the refill sanitizes rather than aborting on:
  it is scanner-owned and a rescan recomputes it, which is the case the
  sanitize-only-under-a-flag policy carves out. Every other tightened column is
  structural or `NOT NULL`, so a row violating one fails the migration. The
  failure is atomic — every step runs in one transaction, so nothing is
  half-applied and the store is exactly as it was.

  **External writers break here.** The path column changes type under every
  query, and because SQLite never compares `TEXT` equal to `BLOB` the failure is
  silent: a lookup binding a string matches nothing instead of erroring. The
  `contrib` helpers gained `path_param`/`path_value` and encode at the boundary;
  third-party writers must do the same.

- **`tags` and `track_art` are rebuilt by the same migration**, for four reasons
  that each needed a `CHECK` or a key SQLite cannot alter in place.

  **The ordinal space splits in two**
  ([#663](https://github.com/Sohex/musefs/issues/663)). `tags`' primary key was
  `(track_id, key, ordinal)`, which did not discriminate on `value_blob`, so a
  track's text rows and its binary rows were numbered in one space per key. The
  scanner is not affected — it writes both classes together — but an external
  writer that rewrites the text rows alone is, and that is exactly what both
  `contrib` helpers do: they scope their `DELETE` to `value_blob IS NULL` so
  scanner-written binary payloads survive a sync. Such a writer could land a text
  row on an ordinal a binary row already held and get
  `UNIQUE constraint failed`. The primary key is replaced by a unique index on
  `(track_id, key, ordinal, (value_blob IS NULL))`: the expression yields 0 or 1
  and never NULL, so uniqueness is per class while one row of each may share a
  triple. The rowid is deliberately untouched: a binary tag payload is addressed
  by it from the served layout.

  One index rather than the two partial ones the issue sketched, because a
  partial index can only serve a query whose `WHERE` implies its predicate.
  Every Rust reader constrains `value_blob`, but `tags_for_track` in the
  `contrib` helpers deliberately does not — it reads both classes at once — and
  against two partial indexes that plans as `SCAN tags` plus a temp B-tree for
  the `ORDER BY`, where the primary key used to serve it. A query-plan test now
  pins every read shape to an index.

  **Row ownership becomes immutable**
  ([#717](https://github.com/Sohex/musefs/issues/717)). `tags_au` and
  `track_art_au` bumped only `NEW.track_id`, so moving a row between tracks left
  the *old* owner serving a stale synthesized header under a `content_version`
  that still matched — and for binary tags, a cached layout still holding a rowid
  that had moved to another track, which is the wrong-row class #502 hardened
  against. Two `BEFORE UPDATE OF track_id` triggers now refuse the reparent, the
  way `art_reject_content_update` already refuses art mutation. No writer needs
  it: both `contrib` helpers replace by delete-then-insert, and moving a tag
  between tracks is not one edit to one thing — two tracks change, and
  delete-then-insert says so. The two `_au` bumps widen to
  `WHERE id IN (OLD.track_id, NEW.track_id)` anyway, so the invalidation is
  correct on its own terms rather than only because something else forbids the
  case it mishandled — which matters against a writer that drops triggers
  through `writable_schema`. Naming `track_id` in a `SET` list without changing
  it is not a reparent and still works.

  **The picture metadata moves to the link**
  ([#716](https://github.com/Sohex/musefs/issues/716)).
  `track_art` gains `mime`, `width`, `height`, `depth` and `colors`, and `art`
  gives the first three up in the same step. `art` is
  deduplicated on `sha256(data)` but owned those columns, which is the wrong
  functional dependency: they describe one file's `PICTURE`/`APIC` block, not the
  bytes every file shares. Since `upsert_art` is `ON CONFLICT(sha256) DO
  NOTHING`, whichever occurrence was ingested first permanently chose them for
  every track referencing the blob — so two files holding byte-identical art
  served whichever one the scan happened to reach first, *including its declared
  MIME type*. A library holding the same cover as FLAC and MP3 hits this
  routinely, because `APIC` carries no dimensions and `PICTURE` does. `depth` and
  `colors` are new storage for values FLAC's parser already reads and discards.

  The backfill copies the shared `art` values to every link, because the true
  per-embedding ones were destroyed at ingest; a `revalidate` restores them for every picture a file embeds itself — the pass `musefs migrate` offers once it finishes. Then
  `art` is rebuilt without them — see the `art` rebuild below, whose ordering
  constraint is what makes the window for that exist. What is left on the row is
  the content and its identity: `id`, `sha256`, `byte_len`, `data`.

  External writers using the `contrib` Python library see one change:
  `upsert_art(conn, data, mime)` is now `upsert_art(conn, data)`. The argument
  was already ignored whenever the image had been seen before — that is what
  `ON CONFLICT(sha256) DO NOTHING` means — and there is no longer a column for
  it to write. `replace_track_art`, which gained the mime in the previous step of
  this same release, is now where it goes.

  **And both tables gain the constraint work**: storage classes pinned
  ([#718](https://github.com/Sohex/musefs/issues/718)) — including upper bounds
  tying the geometry to the Rust model's `Option<u32>` — and an embedded NUL
  banned in the tag key and the art description
  ([#693](https://github.com/Sohex/musefs/issues/693)). #693's ban on the art
  mime arrives with the column at its new home, and there is only the one home.

- **`structural_blocks` is rebuilt as well**
  ([#732](https://github.com/Sohex/musefs/issues/732)) — the fourth and last of
  the rebuilds, and what makes "every core table pins its storage classes" true
  rather than nearly true. It
  was the one table V4 would otherwise have left alone — its shape does not
  change — and so the last place a schema-valid row could still reach the reader
  as a `rusqlite` conversion failure: a non-integral `REAL` `ordinal` or a `TEXT`
  `body` is what affinity will not convert, and each surfaced as a store-wide
  error rather than as the malformed row it is. (`kind` needs no such check: its
  `IN` list is strictly stronger, since no non-`TEXT` value compares equal to
  either name.) The value checks were always there and still are; what was
  missing was the storage class that lets them be the thing that fires.

  "Non-integral" is the whole of it, and worth saying: `INTEGER` affinity
  converts an exactly-integral `REAL` — `1.0` is stored as the integer 1 and is
  not a violation at all. A storage-class `CHECK` earns its keep only on what
  affinity cannot convert, which is precisely the set that reaches Rust as a
  conversion failure rather than as a wrong value.

  Low severity on its own — `structural_blocks` is outside the editable
  contract, so reaching it needs a writer that ignores that. It is here on
  timing: the migration **already** holds this table's rows while `tracks` is
  rebuilt and refills them afterwards, so the empty window and the copy both
  exist and this cost a drop and a create. Left for a future major it would have
  cost a gated migration of its own, for a robustness fix nobody would schedule
  one for.

  **Its rows become immutable too**
  ([#759](https://github.com/Sohex/musefs/issues/759)). The table had insert and
  delete triggers only, on V1's reasoning that the owned writer replaces by
  delete-then-insert and no `UPDATE` path exists. That is true of musefs and not
  of SQL: rewriting a row's `body` changed a served FLAC-header input without
  bumping `content_version`, and changing its `track_id` moved one between
  tracks without bumping either owner, so a cached layout kept serving the old
  header. `structural_blocks_reject_update` now refuses every update — there is
  no legitimate one to let through — the way art content and tag and link
  ownership already are, and a new `structural_blocks_au` bumps both the old and
  the new owner for a writer that drops the refusal. V1's other claim, that the
  over-bump from a byte-identical re-probe is harmless churn, stopped holding
  once the served mtime derived from `content_version` (#725); V1's text is
  frozen, so the correction lives in V4's comments.

- **The `track_changes` ring is recreated with its storage class pinned**
  ([#760](https://github.com/Sohex/musefs/issues/760)). It kept V1's
  `track_id INTEGER NOT NULL` with no `typeof`, the one internal table the
  storage-class work left out, and the refresh reads the column straight into an
  `i64`. A text, real or blob row past a mount's watermark was therefore a
  conversion error, and an error — unlike a gap — advances no watermark: every
  later poll re-read the same window and failed on the same row, so the mount
  stopped picking up external edits until 8192 further changes pruned it or the
  mount restarted. The migration drops and recreates the ring with
  `CHECK (typeof(track_id) = 'integer')` instead of carrying its contents: it is
  derived state, the step is gated so no mount holds a watermark into it, and a
  mount takes its watermark from whatever is there. The reader does not rely on
  the `CHECK` either. `changelog_since` skips such a row and reports it in
  `ChangelogRead::malformed`, and the refresh treats that as a gap and falls back
  to a full rebuild.

- **`art` is rebuilt by the same migration** — the third of the four, and the
  one with an ordering constraint the others did not have. `track_art.art_id`
  references `art(id)` with **no** `ON DELETE CASCADE`, so with foreign keys
  enforced `DROP TABLE art` fails outright while any link row exists. The only
  point in the migration where the table can be replaced at all is after
  `track_art` has been recreated and before the children are refilled, which is
  exactly where it sits. It is also the expensive step: every image blob is
  copied twice and the store transiently holds about its own size again, which
  is what `musefs migrate`'s free-space pre-flight checks before starting.

  `art_reject_content_update` comes back with the row's **key** in its guard
  ([#719](https://github.com/Sohex/musefs/issues/719)). `UPDATE art SET id = …`
  changes none of the content columns, so the `WHEN` clause was false and the
  trigger never fired. With foreign keys on, the update fails on its own — but
  this store deliberately defends against writers that turn them off, and under
  that model the id change orphaned every link while nothing bumped
  `content_version`: `art_ad` is `AFTER DELETE` only, so the one trigger that
  fans out to referencing tracks never saw it, and a cached layout kept serving.
  If the old id were later reused by an unrelated row, that layout would point
  at the wrong image.

  The same rebuild is where `art` sheds `mime`, `width` and `height`
  ([#716](https://github.com/Sohex/musefs/issues/716)) — the backfill above has
  already taken their values to the link, and the holding copy of the old table
  is what it reads them from, since the real one has stopped having them by then.

  Storage classes are pinned here too
  ([#718](https://github.com/Sohex/musefs/issues/718)), and `sha256` gains the
  NUL ban ([#693](https://github.com/Sohex/musefs/issues/693)), because a
  64-character prefix followed by NUL and anything satisfied a `length() = 64`
  identity check, which for a content address is the whole meaning of the column.

  The refill is straight rather than sanitizing: `art` carries no scanner-owned
  column a rescan could recompute, so there is nothing the
  sanitize-only-under-a-flag policy would let this step null on its own. A row
  the tightened constraints reject fails the migration, atomically, the way it
  does for the rebuilds above.


- Directory handles on the same directory share one listing instead of copying
  it each. `opendir` took a private snapshot per handle, so the table's memory
  was the directory's width times the handle count: on a template that
  collapses a library into one directory, a client opening the 1,024-handle cap
  on it — which needs no privilege — pinned tens of gigabytes. A listing is now
  keyed by directory and virtual-tree generation, and handles that agree on
  both share it. All but the first also skip the tree walk that builds one,
  which an over-cap `readdir` consults before rebuilding as well.
  `musefs_dir_listings` reports the distinct-listing count behind
  `musefs_dir_handles`
  ([#675](https://github.com/Sohex/musefs/issues/675)).

- An MP4 file skipped for its track layout now reports the handler types found
  (`unsupported MP4 track layout: expected one audio (soun) track, optionally
  with text/sbtl chapter tracks; found [soun, vide]`) instead of a bare "not a
  supported MP4/M4A file", so the skip explains itself
  ([#672](https://github.com/Sohex/musefs/issues/672)).

- `benches/storage_tunables_bench.sh` gains a `prefetch` mode that A/Bs two or
  more musefs binaries (`MUSEFS_PREFETCH_BINS="label=path ..."`) over one
  NFS+netem corpus, and its real-corpus filter now picks up `.opus` files. Its
  NFS modes also disable NFS LOCALIO for the run: on Linux 6.12+ a loopback mount
  negotiates local I/O and bypasses the RPC transport, so `tc netem` on `lo` had
  no effect on the data path and every "NFS" row measured local disk at GB/s
  (#671).

- **Behavior change.** A scan that hits a DB constraint violation on one file
  now runs to completion instead of stopping there
  ([#662](https://github.com/Sohex/musefs/issues/662)). Three observable
  consequences. It exits **2** — `scan` completed, at least one file failed —
  where it previously exited **1** as a hard error, so a pipeline keying on the
  exit code sees a different value for the same library. The store ends up
  holding every file in the library except the rejected one, rather than only
  the batches that committed before the abort, so a rescan after the fix no
  longer has an unknown amount of the walk left to redo. And the rejected file
  is reported rather than fatal: it is named in the log with the constraint
  text, counted in the new `rejected` bucket of the `failed N: …` summary, and
  missing from the mount while everything else is served. Anything that treated
  a constraint violation as a signal to stop the scan no longer gets one; the
  exit code and that summary are the signals to key on. The engineering is in
  Fixed below. See [Scanning](guide/scanning.md#scan) and
  [Exit codes](guide/troubleshooting.md#exit-codes).

- **Schema migration (`user_version` 3 → 4).** `MIGRATION_V4` sets every
  `tracks.fingerprint` to NULL. The fingerprint's input domain changed (it now
  includes sampled audio), so a value computed by an earlier musefs claims to be
  something this build no longer produces. Nulling is honest and costs nothing
  that leaving them would have saved: an old value cannot match a new one
  either. The next `revalidate` recomputes them — it re-probes a row missing the
  checksum its tier asks for, which a plain `scan` of a tracked file does not —
  and until then those
  rows cannot be move-recovered, exactly as an unfingerprinted row never could.
  `content_hash` is nulled with it. Its meaning did not change, but before #689 a
  fingerprint-tier rescan of a rewritten file kept the old bytes' hash, so no
  stored value can be trusted to describe its file; `musefs revalidate
  --checksum=full` recomputes them. Like every migration this is one-way: the store will no longer open
  with an older musefs
  ([#691](https://github.com/Sohex/musefs/issues/691)).

- **Migrations are classified transparent or gated.** Every schema step up to
  1.3.0 was applied as a side effect of opening the store. That is right for a
  step whose cost and consequences nobody notices, and wrong for one that
  rewrites data the user did not ask to have rewritten, transiently needs the
  store's size again in free disk, or ends compatibility with the binary they
  were running yesterday. Each entry in `MIGRATIONS` now declares which it is.
  An open that finds a gated step pending applies nothing — not even the
  transparent steps ahead of it, each of which would already lock the previous
  release out ([#749](https://github.com/Sohex/musefs/issues/749)) — and
  refuses with `DbError::StoreNeedsMigration`, which names the `musefs migrate`
  command. That is the opposite direction from `StoreTooNew` and carries the
  opposite remedy — upgrade the store, not the binary — so the two are separate
  variants with separate messages. Nothing is stored: the binary owns the
  classification, so a user jumping from 1.2 straight to 2.1 is still gated on
  the step that needs it. A store being *created* is exempt, having no data to
  endanger, or `scan` could never build a new library. The consequence for two
  callers is visible: `mount` and `vacuum` stop upgrading an older store
  implicitly, and `vacuum`'s help text, which advertised that it did, no longer
  says so. `MIGRATION_V4` is the first gated step
  ([#706](https://github.com/Sohex/musefs/issues/706)).

  Which release a step came from is recorded alongside it, and the contract
  that follows is enforced by a `const` assertion rather than by review: **a
  gated step may only be introduced by a major release**, so musefs will not
  build if one is added to a minor or a patch. That turns the classification
  into something a user can act on without knowing what a `user_version` is —
  crossing a major boundary may ask for `musefs migrate`, and a minor or patch
  upgrade never will. The converse is not asserted: a major release is free to
  carry only transparent steps, as `MIGRATION_V3` does.

- The `tags.value` cap rises from 256 KiB to 16 MiB − 1, and
  `track_art.description` from 1 KiB to 8 KiB (schema `MIGRATION_V3`). The new
  tag cap is FLAC's 24-bit metadata-block ceiling — the largest tag synthesis
  could ever serve — so the store no longer refuses a tag the format itself can
  carry. The step only widens constraints, so it carries every existing row across
  and needs no rescan. On its own it would apply on any open, but in 2.0.0 it
  sits ahead of the gated migration, so `musefs migrate` applies it with the rest
  ([#749](https://github.com/Sohex/musefs/issues/749)).

### Removed

- **`scan --revalidate`**, deprecated since 1.2.0 in favour of the `revalidate`
  subcommand, and its `MUSEFS_REVALIDATE` variable
  ([#707](https://github.com/Sohex/musefs/issues/707)). The flag is now a usage
  error. The variable is refused rather than ignored: clap never reads an
  environment variable no flag declares, so a unit file still setting it would
  have gone on running a full scan where it used to revalidate, and said
  nothing. `scan` stops with a message naming the subcommand instead.
  `musefs_cli::run_scan` loses its `revalidate` parameter. The `contrib`
  packages have called the subcommand since their 1.2.0, so only a copy older
  than that is affected.

- **`scan --fast` and `--strict`, replaced by `--match=auto|fast|strict`**, and
  `MUSEFS_FAST`/`MUSEFS_STRICT` by `MUSEFS_MATCH`
  ([#709](https://github.com/Sohex/musefs/issues/709)). Match strictness has
  three states, and two booleans spent a fourth on a combination the CLI had to
  detect and reject; one value has no such combination, and takes the same
  shape as `--checksum` beside it. `auto` is the default and behaves exactly as
  passing neither flag did. The old flags are usage errors, and the old
  variables are refused with the `MUSEFS_MATCH` value to use — ignoring
  `MUSEFS_STRICT=true` would have quietly weakened how a moved file is
  confirmed. `musefs_cli::run_scan` takes a `MatchMode` in place of the two
  booleans.

- **Test scaffolding is no longer published API**
  ([#710](https://github.com/Sohex/musefs/issues/710)).
  `musefs_core::scan_directory_full_oracle`, the `*_for_test` methods on
  `Musefs` and `Db`, and `musefs_format::ogg::page_test_support` were `pub` so
  the crates' own integration tests could reach them — most of them behind
  `#[doc(hidden)]`, which keeps a symbol out of rustdoc but not out of semver,
  and `Musefs::refresh_for_test` not even that. They are compiled for tests
  only now: `musefs-core` and `musefs-db` each gain a `test-support` feature
  their own test builds switch on, `page_test_support` joins `fuzz_check`
  behind `musefs-format`'s `fuzzing`, and the five helpers only a crate's own
  unit tests call are `pub(crate)`. Nothing outside the test suites called any
  of them. `musefs_db::seed_store_at_version`, which builds a store at an old
  schema version, was missed at first and followed under `musefs-db`'s
  `test-support` ([#751](https://github.com/Sohex/musefs/issues/751)).

### Fixed

- **Re-probing an unchanged file no longer moves its served mtime**
  ([#757](https://github.com/Sohex/musefs/issues/757)). A synthesized file's
  modification time follows two values a re-probe wrote whether or not anything
  had changed. Its whole second follows the row's `updated_at`, which every
  re-probe stamped with the current time. Its nanoseconds follow
  `content_version` (#725), which every FLAC re-probe bumped by deleting and
  re-inserting the track's `STREAMINFO`/`SEEKTABLE` rows. A revalidate over
  unchanged files, to raise the checksum tier say, made every file look modified
  to rsync, Syncthing and backup tools. A re-probe now stamps `updated_at` only
  when a column it writes differs from what is stored, and leaves a
  byte-identical block set alone. Any real difference, down to one byte or a
  swapped ordinal, still records a change.

- **A revalidate no longer keeps a `content_hash` its row cannot vouch for.**
  Deciding whether an uncomputed checksum may be kept compared the stored stamp
  with the live file the way serving does, where an unrecorded inode matches
  any. That is too weak to prove the bytes are unchanged. Every row an upgraded
  store starts with has no inode, and a hash a pre-#689 rescan left stale would
  have survived the first revalidate and been treated as current from then on.
  Such a row's uncomputed checksums are now cleared rather than kept
  ([#689](https://github.com/Sohex/musefs/issues/689)).

- **`musefs migrate` exits `2` when the revalidate it ran counted failures**
  ([#750](https://github.com/Sohex/musefs/issues/750)). It discarded the count
  and exited `0`, so `musefs migrate --yes --revalidate && …` treated a partial
  revalidate as a clean one, while `musefs revalidate` run on its own exits `2`
  for the same result. The store upgrade has succeeded either way, and the run
  says so before exiting. `musefs_cli::run_migrate` returns the failure count.
  An explicit `--revalidate` that cannot run, because the stored tracks share
  no directory below `/`, now fails the command after the upgrade instead of
  exiting `0`. `run_migrate` also refuses `--repair` with `--no-snapshot` itself,
  rather than relying on the parser: called directly with both, it used to
  delete the refused rows without a snapshot and then panic.

- **A chained Ogg file stored by 1.3.0 no longer fails every revalidate for
  good.** 2.0.0 refuses chained Ogg at scan time, which left the rows an older
  binary stored with no way out: `revalidate` re-probed each one, was refused,
  wrote nothing, and counted it in `failed` — exiting `2` — on every run, while
  neither `scan --force` nor `--prune` (which only removed rows whose file was
  gone) could remove it. `revalidate --prune` now also deletes a row whose
  file is present but refused as unsupported, only for that refusal and only
  while the file is unchanged since; without `--prune` the run says how many
  there are. The failure breakdown gains an `unsupported` reason
  ([#747](https://github.com/Sohex/musefs/issues/747)).

- **A revalidate restores each file's own picture metadata.** The schema v4
  migration can only copy one blob's MIME type and dimensions onto every link
  to it, with FLAC's depth and colour count at 0, and said the real values
  came back on a rescan — but the revalidate `musefs migrate` offers never
  touched art, so only `scan --force`, which replaces curated tags and art,
  restored them. A revalidate now restores the MIME type, dimensions, depth
  and colours of every link whose image, picture type and description are one
  the file embeds, and leaves every other link alone
  ([#746](https://github.com/Sohex/musefs/issues/746)).

- **Metadata work can no longer grow the worker-pool queue without bound.**
  Reads were capped; `lookup`, `getattr`, `open`, `opendir` and directory
  listings queued without limit. They now pass an admission gate of 4096 jobs,
  and over it run on the submitting thread, which throttles the kernel instead
  of refusing the call; a `readdirplus` entry over the cap is listed with
  uncached attributes. Store refreshes moved to a thread of their own. The new
  `musefs_pool_over_cap_total` counter shows when the cap is met.
  ([#694](https://github.com/Sohex/musefs/issues/694))

- **A directory listed without a handle can no longer repeat or skip entries
  when the library changes mid-listing**
  ([#695](https://github.com/Sohex/musefs/issues/695)). Past the 1,024
  directory-handle cap, which a parallel walker over a large mount routinely
  reaches, each `readdir` page was rebuilt from whatever tree was current and
  resumed at a plain index. A store change landing between two pages shifted
  the entries under that index, so one enumeration could return an entry twice
  or miss one. The first page of such an enumeration now pins its listing and
  tags the cookies it returns with that generation, so every later page reads
  the same listing. Up to 64 listings stay pinned. One evicted before its
  enumeration finishes is rebuilt and resumed if the tree has not changed since.
  If a refresh has replaced its generation, that `readdir` fails with `ESTALE`
  rather than paging the new listing at the old position, and a new enumeration
  of the directory succeeds.

- **A crafted `art` row can no longer hand one image's bytes to a file embedding
  another** ([#724](https://github.com/Sohex/musefs/issues/724)). `art` is
  deduplicated by `sha256`, and on a conflict the writers returned the row
  already filed under the digest without looking at its bytes. Nothing in the
  schema can tie the digest to the data, so a store holding a row whose `sha256`
  names bytes it does not hold linked that row to every file embedding the real
  image. Both writers now compare a conflicting row's bytes with the incoming
  image before using it. `musefs scan` fails only the file that would link it,
  counted under `rejected`, and compares each distinct row at most once per
  scan, so a cover shared across an album costs one comparison; the `contrib`
  helper `upsert_art` raises `ArtDigestMismatch`, which `sync_one` skips like a
  constraint violation. A poisoned row nothing dedups onto is not detected, and
  the readers still serve what a link points at.

- **A backing file rewritten mid-read fails that read, not the next one**
  ([#682](https://github.com/Sohex/musefs/issues/682)). Both read paths checked
  the backing file against its stored stamp *before* reading from it, so an
  in-place rewrite landing between the check and the positioned read returned
  bytes from the new file spliced into the layout synthesized for the old one;
  only the following read noticed. The check now runs after the bytes are
  acquired, replacing the earlier one rather than adding a second stat, and a
  detected change takes precedence over whatever the read itself reported,
  since a rewrite can surface as a short read first. Bytes served from the
  read-ahead window were acquired by an earlier read whose own check covered
  them.

- **A symlink retargeted during a `--follow-symlinks` scan can no longer store
  one file's geometry against another file's path**
  ([#684](https://github.com/Sohex/musefs/issues/684)). The scan probed the
  walked name and then canonicalized it — two lookups of the same symlink — so a
  retarget between them ingested a row whose stamp and bounds described one file
  and whose path named another. It failed closed (every read refused it with
  `BackingChanged`), but the track was permanently unreadable while the file
  looked fine. The path is now resolved once and the probe reads what was
  resolved. A probe failure under `--follow-symlinks` therefore names the
  resolved target rather than the link.

- **The backing-freshness guarantee is documented for the reads it actually
  covers** ([#683](https://github.com/Sohex/musefs/issues/683)). The docs said
  the held descriptor is re-stated on every read, and that re-tags keep cached
  bytes from going stale. Both are true of what reaches musefs; with
  `--keep-cache`, on by default, a read the kernel serves from its page cache
  never does. So an in-place rewrite of a backing file behind a file that is
  already open and cached is caught at the next open — which fails with `EIO` —
  rather than on those cached reads, provided the rewrite moved the file's size,
  mtime, ctime or inode; a same-size rewrite in place on a filesystem with
  coarse timestamps can move none of them, and then no open catches it. The behaviour is unchanged and deliberate;
  the architecture page and the tuning table now say so.

- **`musefs vacuum` refuses a store a mount has open, as it always said it did**
  ([#721](https://github.com/Sohex/musefs/issues/721)). It relied on `VACUUM`
  failing on a busy lock, and in WAL mode a mount idle between reads holds none,
  so it compacted the store underneath a live mount. It now claims the store the
  way `musefs migrate` does — a claim that needs SQLite's shared-memory index to
  itself, so an idle mount and a read-only connection both count — and holds it
  until it is done. The one case it cannot see in advance is a process that has
  opened the store but not yet read from it; that process is kept out from the
  moment it tries.

- **A crafted `structural_blocks.kind` is refused before it is read**
  ([#715](https://github.com/Sohex/musefs/issues/715)). The reader checked the
  value against its two-name allowlist only after materializing it, and a store
  written under `PRAGMA ignore_check_constraints` can hold any length there, so a
  hostile row cost its full size twice — once read, once more debug-escaped into
  the error — before being rejected. The column is now bounded from its
  character and byte lengths first, like every other text column the readers
  guard, at a cap pinned by a test to the longest allowlisted kind.

- **A synthesized file's mtime now moves whenever its bytes do, and a pre-epoch
  backing file is stored and served.** Two fixes in the same type, because both are
  about what the mount reports as a timestamp
  ([#696](https://github.com/Sohex/musefs/issues/696),
  [#725](https://github.com/Sohex/musefs/issues/725)).

  The reported second is the later of the backing file's and the row's
  `updated_at`, which triggers stamp in whole seconds. So two metadata edits
  inside one wall-clock second left the same second behind, and a same-length
  tag rewrite looked untouched to anything comparing size and mtime — rsync
  without `--checksum`, Syncthing, media scanners. A backing mtime in the future
  was worse: `max` meant it masked *every* metadata edit for as long as the skew
  lasted. musefs itself was never wrong, because its caches key on
  `content_version`; the contract presented outward was. The mount now reports
  that same counter as the timestamp's nanoseconds, so a store change that
  changes the synthesized bytes always moves the mtime. Nothing in the store
  holds nanoseconds — the precision is derived where the timestamp is built, and
  only in synthesis mode: `--mode structure-only` serves the backing file
  verbatim, where a tag edit changes nothing and must not claim to.

  Separately, `Attr`'s `mtime_secs` was a bare `i64` in which zero meant both
  "synthetic directory" and "the Unix epoch", and the mount substituted the
  mount time for anything at or below zero. A file whose mtime really was the
  epoch therefore reported the wrong time, and every pre-epoch file would have
  once v4 stopped refusing one. The field is an `Option` now, so the fallback
  fires for the synthetic case and nothing else. A pre-epoch second is what
  `--mode structure-only` reports; synthesis mode reports the later of it and
  the row's `updated_at`, which is never before 1970.

  What the virtual mtime promises is written down for the first time, in
  [the serving model](architecture/serving.md).

- Two audio files whose names differ only in bytes that are not valid UTF-8 are
  no longer silently merged into one track
  ([#680](https://github.com/Sohex/musefs/issues/680)). A filename on Unix is an
  arbitrary byte string, and the scanner stored `to_string_lossy()` as the row's
  identity — every invalid sequence became `U+FFFD`.

  Two things followed, and the second is the serious one. The stored path did
  not exist on disk, so the track could never be served: every resolve failed
  its `metadata` call. And two distinct byte paths converged on the same
  `U+FFFD`-bearing string, where `ON CONFLICT(backing_path) DO UPDATE` merged
  them into a single row carrying one file's identity and the other's tags. The
  scan reported success — nothing in the `skipped` or `failed` counts, no
  warning, one track simply gone.

  The extension filter offered no protection: `is_supported_audio` looks only at
  the extension, so `bad\x80name.flac` probed fine and reached the insert with
  its identity already mangled.

  The models, the scanner's in-flight identity, the already-present set, the
  retarget writer, the backing-path listing and the error variants that name a
  file are all byte-typed now (`PathBuf`, which *is* bytes on Unix). Lossy
  conversion survives only where a path is genuinely rendered for a person to
  read: log lines, the progress bar's label, error messages.

  One knock-on worth naming: `CoreError::BackingChanged` had been carrying a
  `String` that was sometimes a path and sometimes a sentence. The sentences
  moved to a new `CoreError::DerivedStateStale`, which travels with it
  everywhere it matters — same errno, same retry, same attr-cache drop — but
  says what it actually means.

- The freshness guard no longer passes on changed bytes when the backing
  filesystem has no sub-second timestamps
  ([#674](https://github.com/Sohex/musefs/issues/674)). The column landed in
  the 2.0.0 migration above; this is the half that writes and reads it.

  `BackingStamp` was `(size, mtime_ns, ctime_ns)`. #276 had already strengthened
  it past size plus whole-second mtime, with `ctime` as the adversarial backstop
  a writer cannot set backward. The residual hole was filesystems that store no
  sub-second times at all — FAT32's two-second granularity and no ctime, ext3,
  HFS+, some SMB and NFS mounts truncating the nanosecond fields — where a
  same-size replacement inside the granularity window leaves all three fields
  identical to what was scanned. The guard passed, and the reader was served a
  mix of new audio bytes and a metadata region synthesized for the old content.

  The inode closes the *replacement* shape: a new file moved over the old one
  gets a fresh one. It does not close a true in-place rewrite, which is a POSIX
  timestamp limitation rather than something musefs can fix — but the
  replacement shape is what almost every tagger actually does, writing a
  temporary file and renaming over the original. The false-positive cost is
  nil: an inode changes when a file is copied, restored from backup or moved
  across devices, and all three already invalidate the stamp today, because
  `ctime` cannot be preserved by `cp -a` or rsync either.

  **A stored inode of zero means "not recorded", not "inode zero"** — the state
  every row in an upgraded store starts in — and such a row is compared on the
  other three fields alone. Failing closed on a field the store has nothing to
  say about would take an entire library offline on the first serve after an
  upgrade. `musefs revalidate` re-probes exactly the rows still holding
  the sentinel and fills it in, which makes it the complete repopulation path
  for an upgraded store alongside the structural-block and checksum backfills it
  already covered.

  The column stores the inode's **two's-complement bit pattern**, so a file
  whose inode is above `i64::MAX` records a negative value. SQLite has no
  unsigned 64-bit integer and `st_ino` is a full `u64`, so an encoding is
  forced — and the alternative is not a lost guard but a failed scan: rusqlite
  refuses the bind, and a bind failure is not a constraint violation, so the
  scanner classifies it as fatal and the whole run aborts. That range is not
  theoretical; FUSE and network filesystems synthesize inode numbers freely,
  and several pooling and cloud-mount filesystems hash to produce them. The
  encoding is a bijection and the column is only ever compared for equality, so
  it costs nothing it is used for. The v4 `CHECK` therefore pins the storage
  class without a lower bound.

  The comparison is a named, asymmetric method rather than `==`: one side is
  stored and may know nothing, the other is live and always knows, and the
  sentinel rule is not transitive — a stamp with no recorded inode matches two
  live files that do not match each other. Spelling that as equality would hand
  the next reader an `==` that breaks the `Eq` contract.

- Chained Ogg is now detected and skipped at scan time, and refused at serve
  time ([#722](https://github.com/Sohex/musefs/issues/722)). A chain is complete
  logical bitstreams concatenated end to end, which RFC 3533 allows and the docs
  claimed musefs rejected. It did not: `validate_single_bitstream` walks pages
  only up to `audio_offset`, which proves the *header* region carries one serial
  and one beginning-of-stream page, and a chain's second stream necessarily
  begins after the first stream's audio does. Everything from `audio_offset` to
  EOF was taken as one audio region, and the serve path applied one constant
  sequence delta across all of it with no serial check. That delta is zero only
  while the synthesized header happens to occupy as many pages as the original,
  so a tag edit that changed the page count shifted every page of the second
  stream — whose own numbering restarts at zero by spec — with the first
  stream's delta, recomputing each CRC to match. The output was corrupt in a way
  that passed a page-level integrity check.

  Detection is the file's final page: a chain's last page belongs to its last
  stream, so a serial other than the header's proves chaining for the whole
  well-formed-chain class. The scanner reads one page-sized window
  (`ogg::MAX_PAGE_BYTES`) from the end of the file and classifies it
  (`ogg::classify_tail`) — one bounded read per Ogg file, rather than the walk
  over the whole audio region that a page-by-page check would cost. A file whose
  final page does not end on its last byte, which is what truncation looks like,
  is reported as indeterminate and still scans and serves as before.

  Independently, `serve_ogg_window` now refuses a page whose serial is not the
  resolved file's, so a row written by an older binary — or by an external
  writer — fails closed instead of serving renumbered nonsense. The audio
  segment carries the serial for that purpose. Existing rows keep their too-wide
  bounds. No rescan fixes one, since the scanner now refuses the file;
  `revalidate --prune` removes it ([#747](https://github.com/Sohex/musefs/issues/747)).

- FLAC-in-Ogg files whose mapping header declares a header-packet count of zero
  now ingest their tags and art, and synthesize correctly
  ([#723](https://github.com/Sohex/musefs/issues/723)). The 16-bit count in
  packet 0 is the number of metadata packets that follow, but both RFC 9639 and
  Xiph's mapping define zero as *unknown* — one or more metadata packets still
  follow. Reading it as "none" ended the header run at packet 0, so the real
  `VORBIS_COMMENT`, and any `PICTURE`, `SEEKTABLE` or `CUESHEET`, landed inside
  what the store then recorded as audio. They were never ingested, so the mount
  served the file with no tags and no art — and worse, synthesis emitted a
  mapping packet plus one terminal `VORBIS_COMMENT` followed immediately by
  those same original metadata packets, replayed verbatim as audio and
  renumbered, so a decoder met metadata blocks where audio frames must be.

  An unknown count is now resolved by the rule the format itself defines:
  metadata blocks run until one sets the last-block flag, and a `STREAMINFO`
  flagged last ends the run at packet 0. A run that reaches the audio packet
  without ever flagging its last block has no discoverable end and is malformed.
  A *nonzero* count is still taken at its word: the mapping requires a count it
  gives to be the true number of following packets, and reserves zero for the
  unknown case, so a compliant encoder writing zero is exactly the input this
  fixes rather than an exception to it. Synthesis additionally
  clears `STREAMINFO`'s last-block flag, since the regenerated comment block
  always follows it. An existing row keeps its wrong `audio_offset` until a
  revalidate re-probes it, and the tags and art 1.3.0 never read from such a
  file arrive only through `scan --force`.

- Both checksums are now derived inside the probe's stability transaction, from
  the descriptor it already holds, instead of reopening the pathname afterwards.
  The stamp was always captured first, so the skew was one-directional: a row
  could record one generation's stamp, geometry and tags alongside a hash of the
  next generation, or of a torn mid-write state. Serving failed closed on the
  stamp later, but the value stored as the authoritative content identity was
  garbage, and the retarget confirm — which decides an identity question and has
  no such backstop — could compare a torn hash against a stored one. A file that
  changes during hashing is now discarded as raced like any other, and the
  retarget confirm refuses to answer unless the file still matches the stamp the
  probe committed to. Separately, a `--checksum=full` run that cannot hash a
  file now **fails** that file — counted in `failed` under a `checksum-failed`
  bucket, and so reaching the exit-`2` signal — rather than committing a row one
  tier below what the flag promised behind a warning nothing counted
  ([#690](https://github.com/Sohex/musefs/issues/690)).

- A row no longer claims a `content_hash` its bytes do not have. The checksum
  write read a `None` argument as "leave the stored value alone", which is
  right for a pass that computed nothing and wrong for a pass that watched the
  file change, so a file rewritten in place and then re-probed at the
  `fingerprint` tier kept the *previous* bytes' hash beside its new stamp,
  geometry and tags. That broke the documented forensic identity and poisoned
  move recovery: a later move of the rewritten file was refused on a hash
  comparison it could never satisfy, orphaning the curated row. Each checksum
  write now carries an explicit intent — keep, set, or clear — and a pass below
  the `full` tier clears the column whenever it observes that the recorded bytes
  changed, while still keeping a higher tier's value for a file that has not
  changed. A `--match=fast` retarget, which confirms nothing by design, likewise no
  longer inherits the departed file's hash
  ([#689](https://github.com/Sohex/musefs/issues/689)).

- A move no longer risks retargeting a curated row onto the wrong file. The
  cheap `fingerprint` — the default tier — hashed only the probe's *parsed*
  output, and structural blocks are FLAC-only, so for MP3, M4A, Ogg and WAV it
  contained no audio bytes at all: two different files with the same tags, the
  same art and an equal audio-region length shared one fingerprint. The default
  strictness accepts a fingerprint-only candidate, so scanning such a file
  where an orphaned row was the unique match moved that row's tags and art onto
  audio they were never written for, silently. The fingerprint now folds in
  three bounded windows of audio, sampled at the start, midpoint and end of the
  audio region — at most 24 KiB of positioned reads per file against a
  descriptor the probe already holds, so the tier still rides the probe rather
  than becoming a whole-file pass
  ([#691](https://github.com/Sohex/musefs/issues/691)).

- A track that renders to `.musefs-metrics` or `.metadata_never_index` at the
  mount root no longer collides with the synthetic entry the FUSE layer injects
  there ([#681](https://github.com/Sohex/musefs/issues/681)). `readdir` appended
  the synthetic name to the root listing while `lookup` intercepted it before
  consulting the tree, so the name appeared twice with different inodes, `ls`
  reported the synthetic inode for both rows, and the user's own subtree was
  unreachable anywhere under the mount. Both names are now reserved in the
  virtual-tree namespace (`musefs_core::RESERVED_ROOT_NAMES`): a rendered root
  component that lands on one is pushed to its ` (2)` rank, exactly as a
  colliding rendered name already is, so the synthetic entry keeps the base name
  and the user's data keeps a reachable one. The reservation is applied once,
  when the tree is built, and does not depend on `--expose-metrics` or on the
  platform — the same library therefore mounts to the same paths and inodes
  whichever way the flag is set and wherever it runs.

- A directory that had been ranked away from its base name by a collision is no
  longer re-created once per track. Which directory a path component belongs to
  is now decided by the rendered name rather than the stored one, so a library
  where a *file* renders to the same name as a *directory* — say a track that
  renders to `Live` next to an album directory `Live` — serves that directory's
  tracks from one `Live (2)` instead of scattering them across `Live (2)`,
  `Live (3)`, `Live (4)`, one directory per track. By the same correction, a
  directory whose rendered name is literally another's rank keeps its own
  contents instead of absorbing them: `Live` (ranked to `Live (2)`) and a real
  `Live (2)` are two directories again. Surfaced while reserving the injected
  root names, which reach the same collision path
  ([#681](https://github.com/Sohex/musefs/issues/681)).

- Moving a backing file no longer wedges its track for the life of the mount
  ([#679](https://github.com/Sohex/musefs/issues/679)). A scan that retargets a
  row to a relocated file rewrites the path and the freshness stamp and
  correctly leaves `content_version` alone — the served bytes did not change —
  but the `getattr` size cache, the layout cache and every open file handle
  accepted their cached entry on `content_version` alone while holding the
  pre-move path and stamp. Each then validated the live file against the old
  stamp and failed, permanently: the file listed as `-????????? ?` and every
  read returned `EIO` until a remount. Both caches now compare the row's
  backing-source identity as well as its content identity, and a poll whose
  changelog names any track advances the refresh generation, so open handles
  re-resolve too. A move is not the only way in: any in-place re-stamp that
  leaves the content unchanged reached the same wedge.

- One unparseable `METADATA_BLOCK_PICTURE` no longer discards every other
  embedded picture in the same Ogg file, and the drop is logged instead of
  being swallowed by the scan path. Base64 decoding also tolerates ASCII
  whitespace, so a value wrapped in the older 76-column MIME style decodes
  rather than failing at the first line break
  ([#673](https://github.com/Sohex/musefs/issues/673)).

- A panicking worker-pool task permanently leaked a SQLite read connection and
  up to three file descriptors ([#669](https://github.com/Sohex/musefs/issues/669)).
  `threadpool` retires a worker that unwinds and spawns a replacement, and the
  replacement gets a fresh `ThreadId` — the key `DbPool::PerThread` stores
  connections under, and never evicts. The dead worker's connection therefore
  stayed in the map for the life of the mount while `musefs_pool_workers` kept
  reading healthy. `read`, `lookup`, `getattr` and `open` already ran their
  synthesis inside a `catch_unwind`, but `opendir`, the `readdir` stateless
  fallback and both `poll_refresh` tasks did not. Every pool submission now goes
  through one outer panic boundary, so no task can unwind out of a worker, and
  the two directory paths additionally guard their listing build so a panic
  there is answered with `EIO` instead of dropping the reply and hanging the
  syscall. Per-worker connection counts are now genuinely bounded by
  `--workers`.

- Phase-2 read-ahead prefetch (`--read-ahead-prefetch`) amplified reads instead
  of merely adding overhead ([#671](https://github.com/Sohex/musefs/issues/671)).
  `ReadAhead::insert_window` trimmed the ring to the first window lying fully
  behind the read frontier and fell back to index 0 when it found none. Windows
  are sorted by start, so index 0 is the lowest offset — the window the reader is
  currently inside. The frontier (`next_expected`) is the end of the last served
  *read*, not the end of the window that served it, so a 512 KiB window feeding
  128 KiB reads is never "fully behind" until the reader has consumed all of it,
  and under ring pressure the fallback dropped exactly that window. Driving the
  real `ReadAhead` with the real `plan_prefetch`/`prefetch_depth` logic over 400
  sequential 128 KiB reads: 398 of 400 reads missed and refilled synchronously,
  3159 MiB of foreground backing reads for 50 MiB asked. The window keeps
  doubling across those refills because `off == next_expected` still holds, so it
  is pure amplification rather than seek thrash. The eviction order now prefers a
  window already fully consumed, then the furthest-future window the reader is
  not inside, and `read_into` advances the frontier before inserting so the trim
  sees where the reader actually is.

  Fixing that exposed a second defect it had been masking. A prefetch dispatch
  stops at the first window boundary at or past the horizon, so `prefetched_upto`
  legitimately ends up *beyond* the horizon; `plan_prefetch` read that overshoot
  as a backward seek and re-dispatched the whole horizon from the reader's
  position on the very next read, once the adaptive window outgrew the FUSE read
  size. With the eviction fix alone, a real 866 MiB FLAC read through a real
  kernel mount cost 8.5 GiB of backing reads (9.9x). `plan_prefetch` now tolerates
  an overshoot of up to one window before treating the watermark as a seek, and
  the same read costs 1.01x the file.

  With both fixed, the Phase-2 story changes on high-latency backends. On a real
  loopback NFS mount at 200 ms RTT (`tc netem`, LOCALIO disabled) prefetch is now
  a ~30% single-stream win, 6.3 → 8.2 MB/s, and a ~5% win on four concurrent
  streams; before the fix it was a regression on both, which is what the earlier
  "~10% overhead" finding was actually measuring. It stays opt-in: on local disk
  and over `musefs-latencyfs` it still reads the stream a second time
  speculatively (≈2x the backing bytes) for wall time within noise of
  amplification alone, so the win is one backend and one run. Measurements and
  method are in [Benchmarks](benchmarks.md).

- A DB constraint violation raised while ingesting one file no longer aborts the
  whole scan ([#662](https://github.com/Sohex/musefs/issues/662)). This changes
  observable behavior — the exit code and what the store holds afterwards — and
  Changed above states that part; what follows is why and how. Cap
  violations were pre-checked in `check_storable`
  ([#644](https://github.com/Sohex/musefs/issues/644)), but that covers only the
  caps the scanner knows to look for; every other constraint the schema enforces
  — the `CHECK`s and the `UNIQUE`/primary-key constraints — was discovered by
  SQLite inside the ingest transaction, where the per-file context is gone, and
  propagated out as fatal. The reported case
  ([#659](https://github.com/Sohex/musefs/issues/659)) killed a scan 41% into an
  891k-file library, about an hour in, leaving whatever the earlier batches had
  committed and no record of where the walk stopped. Pre-checking each newly
  discovered constraint does not converge, so the error is now classified at the
  ingest boundary instead: a constraint violation (`SQLITE_CONSTRAINT`, any
  extended code) is attributable to the rows one file wrote, and becomes one
  `failed` file in a new `rejected` bucket, named in the log with the constraint
  text and reported in the end-of-scan breakdown. Errors that say the run itself
  cannot proceed — `SQLITE_CORRUPT`, `SQLITE_FULL`, `SQLITE_IOERR`,
  `SQLITE_READONLY`, `SQLITE_NOTADB`, and any code this build does not recognise
  — still abort with the message they always did; `SQLITE_BUSY` is neither and
  stays with the writer's locking policy. Because the production path commits
  through `BulkWriter`, whose transaction holds a whole batch, each file is now
  ingested inside a `SAVEPOINT` (`BulkWriter::item`): a statement-level `ABORT`
  undoes only the statement that hit the constraint, so without one the batch
  would commit a half-ingested track — a `tracks` row whose tags never landed.
  The savepoint rolls the rejected file back whole and leaves the rest of the
  batch committable.
- A scan no longer aborts on a backing file that carries the same tag key as
  both a text value and a binary payload
  ([#659](https://github.com/Sohex/musefs/issues/659)). `tags`' primary key is
  `(track_id, key, ordinal)`; it does not discriminate on `value_blob`, so a
  track's text rows and binary rows occupy one ordinal space per key. `ingest`
  numbered them independently — text rows from a per-key counter, binary rows
  from a single running index across the track — so a key present in both
  classes produced two rows at ordinal 0 and the ingest transaction failed with
  `UNIQUE constraint failed: tags.track_id, tags.key, tags.ordinal`. Unlike a
  cap violation ([#644](https://github.com/Sohex/musefs/issues/644)) this was
  not routed to a per-file failure, so it killed the whole scan — the reported
  case died 41% into a 891k-file library after an hour. Generalising that
  containment to any constraint violation is tracked separately in
  [#662](https://github.com/Sohex/musefs/issues/662). The two classes now
  draw from one shared per-key counter, text first, which also makes binary
  ordinals per-key rather than track-wide. Reachable shapes: a FLAC `CUESHEET`
  Vorbis comment beside a CUESHEET metadata block, an MP3 `TXXX` frame whose
  description names a binary frame the same tag carries (`PRIV`, `GEOB`,
  `MCDI`, a non-MusicBrainz `UFID`), and an MP4 freeform atom written with both
  a text and a binary `data` box.

- Scan log records and the progress bar no longer clobber each other on an
  interactive terminal ([#648](https://github.com/Sohex/musefs/issues/648)).
  `ScanReporter` renders an `indicatif` bar on stderr and the `log` facade
  writes to the same stderr, with nothing coordinating the two: a record was
  emitted at whatever column the last bar frame left the cursor on, and the
  next 120 ms tick issued a clear-line that ate part of it. Both the warning
  and the bar came out mangled. This was reachable on essentially the first
  interactive scan of any real library, because the end-of-walk skip tally
  ([#341](https://github.com/Sohex/musefs/issues/341)) warns about `cover.jpg`
  / `.cue` / `.log` / `.nfo` sidecars, and every unparseable file warns from
  inside the pipeline. The CLI now owns a single process-wide stderr draw
  target: the scan bar draws through it, and the binary's `env_logger` is
  installed wrapped in a sink that emits each record while that target is
  suspended — bar cleared, record written, bar redrawn below it. The per-target
  summary line (`scanned N: …`, on stdout) is suspended the same way; it used
  to be glued onto a bar frame whenever no log record happened to precede it.
  Off a terminal the draw target is hidden and suspending is a no-op, so this
  change left the `--quiet` and piped milestone paths byte-for-byte alone, as it
  did the verbosity policy (`-v`/`-vv`/`-vvv`, `RUST_LOG` taking precedence),
  which stays in the binary. (The milestone line is renamed by the progress-bar
  convergence fix below, in this same release.)
- The scan progress indicator now reaches 100% when files fail
  ([#655](https://github.com/Sohex/musefs/issues/655)). The bar's length is the
  walked file count, but its position only advanced on a committed file, so any
  failure left it permanently short — a run with 12 unparseable files out of 42
  finished at `30/42 (71%)` and was then cleared, which reads as an aborted scan
  rather than a completed one about to report `failed 12`. On the piped path the
  final `100%` milestone was simply never printed, so a log-scraping consumer
  waited for a line that could not arrive. A dispatched file that fails or races
  now advances the same progress sequence as a committed one; the two together
  always account for the walked total, and a debug assertion pins that. The
  piped milestone line is renamed `ingested N/M (P%)` → `processed N/M (P%)`,
  because it now counts every file the pipeline finished with rather than only
  the successes — scripts matching the old prefix need updating.
- A tag larger than the store's cap no longer aborts the entire scan
  ([#644](https://github.com/Sohex/musefs/issues/644)). The scanner had no
  length check on text tags, so an over-cap value reached the DB `CHECK` inside
  a batch commit and failed the whole run with
  `CHECK constraint failed: length(CAST(value AS BLOB)) <= 262144` — naming
  neither the offending file nor what the number meant. Every cap the scanner
  can trip is now checked in one place before anything is written, and a
  violation fails only that file, with a message naming it. The same applies to
  the `tags.key`, `art.mime` and `track_art.description` caps, which had the
  identical unattributed-abort failure mode. Should a store write still fail
  fatally, the error now names the file it died on.
- A FLAC whose tags outgrow what a `VORBIS_COMMENT` block can hold is rejected
  at scan time rather than stored and then served `EIO` on every read. This is
  reachable by merging a leading ID3v2 tag's fields into a FLAC's own comments
  ([#602](https://github.com/Sohex/musefs/issues/602)): ID3v2's tag size is
  synchsafe 28-bit (256 MiB) while a FLAC metadata block is 24-bit.
- A leading ID3v2 tag on an MP3 (or a FLAC) is stepped over by its declared size
  even when its major version is not one musefs can parse the frames of, instead
  of the whole file being rejected. The ID3v2 header has the same shape in every
  version, so its size is enough to step over the tag, and the spec's rule for a
  version a reader does not understand is to ignore it. Such a tag's frames are
  still not read.
- Over-cap `opendir` degrades to a stateless directory handle instead of
  replying `ENFILE` ([#616](https://github.com/Sohex/musefs/issues/616)). The 1024-handle cap was assumed to sit
  well above any real client, but `bfs` — an ordinary parallel `find`, and the
  default `find` in several distributions — exceeded it immediately: 7,525
  rejections over a 200,000-track mount, 17,910 files never enumerated. The
  rejection surfaced to the operator as "Too many open files in system", which
  points at the kernel rather than the mount, and an indexer that logs and
  continues would simply present an incomplete library. Serving over-cap
  directories through the existing stateless fallback keeps listings complete
  and preserves the memory bound the cap was added for, at the cost of an O(N)
  rebuild per `readdir`.
- Unmount helpers are resolved against `/usr/bin`, `/bin` and `/usr/local/bin`
  before falling back to a bare-name `PATH` lookup ([#620](https://github.com/Sohex/musefs/issues/620)). The
  mounting guide steers operators toward running as root for kernel passthrough
  in `StructureOnly` mode, and nothing sanitizes `PATH` there, so a writable
  `PATH` entry meant attacker-chosen code executed as root on `SIGTERM`.
- A failed batch commit during a scan winds the pipeline down before the error
  propagates ([#618](https://github.com/Sohex/musefs/issues/618)). `ByteBudget` gained a `close()` that wakes
  waiters, so a worker parked in `acquire` on a condvar only `flush` ever
  signalled is no longer stranded. This was benign for the CLI, where process
  exit reaps everything, but `scan_directory_with` / `revalidate_with` are
  public API and an embedder that caught the error accumulated leaked threads
  and their in-flight art bytes. The two state-mutex `unwrap()`s adopted the
  daemon's `lock_recover` policy at the same time, retiring the open note in
  `lock.rs`.
- `find_page_start` bounds the number of candidate pages it CRC-validates per
  call ([#619](https://github.com/Sohex/musefs/issues/619)). The header pre-filter almost never admits a false
  `OggS` on real audio, but a file whose audio region is deliberately packed
  with `OggS\x00\x00` cleared it at every offset, allowing up to ~65,000 CRC
  validations — each with its own positioned reads — for a single seeking read.
  Hardening rather than a live vulnerability: the attacker is whoever can place
  a file in the scanned library.
- `access` is implemented and replies `ok` ([#624](https://github.com/Sohex/musefs/issues/624)), so fuser's default
  no longer logs `[Not Implemented]` per mount. The mount carries `RO` and, with
  `allow_other`, `DefaultPermissions`, so the kernel already enforces the
  presented mode bits.

### Internal

- The `ogg_page` fuzz target round-trips the page machinery the serve path
  actually depends on — `verify_page_crc` and `patch_page_header_algebraic` —
  instead of only decoding a header ([#625](https://github.com/Sohex/musefs/issues/625)). Coverage against the
  committed seed rose from 51 edges / 69 features to 129 / 267.
- The read-ahead budget invariant restored by #536 now has a concurrent
  regression test that the ASan and TSan CI legs actually reach
  ([#628](https://github.com/Sohex/musefs/issues/628)). The sanitizer legs previously ran a test that builds
  `ReadAheadPool::new(0)`, leaving the pool disabled throughout. No live bug was
  found; this closes the coverage gap.
- `musefs-core/tests/tree_footprint.rs` gates the virtual tree's per-track
  resident cost ([#629](https://github.com/Sohex/musefs/issues/629)), turning #617's throwaway probe into a
  committed ceiling. It samples `VmRSS` either side of `VirtualTree::build_with`
  with the rendered paths materialized beforehand, so the delta is the tree's
  own marginal cost. The ceiling is deliberately loose — a gate that catches
  only a large regression is worth more than one that reddens on a loaded
  runner — and a floor assertion keeps a broken measurement from passing
  vacuously.
- `deny.toml`'s allow and ignore lists can no longer rot ([#622](https://github.com/Sohex/musefs/issues/622)). A
  stale `RUSTSEC-2025-0167` ignore and an unmatched `ISC` license allowance both
  warned and exited 0, so neither surfaced on a PR — which is how an entry ends
  up silently pre-exempting the next real advisory for the same crate. Both were
  dropped, and `advisory-not-detected` / `license-not-encountered` are now
  errors in the `deny` job. The promotion is scoped to the root graph, which is
  what the lists are authored against; the fuzz-lockfile scan in `audit.yml`
  allows the advisory diagnostic explicitly, since off that graph it is noise.
- Issue, pull-request and `CODEOWNERS` templates ([#627](https://github.com/Sohex/musefs/issues/627)). The PR
  checklist covers the steps that are easy to forget and silently break
  something: the pre-commit hook, `cargo +nightly fuzz build` after a
  format-layer API change, regenerating the Python schema mirror after a
  `musefs-db` change, and a changelog entry.

## [1.3.0] - 2026-08-19

### Added

- FLAC files carrying one or more ID3v2 tags in front of the `fLaC` marker are
  now scanned instead of being skipped with "no parseable audio metadata"
  ([#602](https://github.com/Sohex/musefs/issues/602)). The tag run is stepped over to reach the FLAC stream, and its text
  frames and `APIC` pictures are ingested as a fallback beneath the file's own
  `VORBIS_COMMENT` / `PICTURE` blocks — so a FLAC whose tags live only in the
  ID3 header lands in the store with its tags. A trailing 128-byte ID3v1 tag is
  trimmed from the audio length (checked only for files with a leading ID3v2
  tag, so a stock FLAC pays no extra read). Neither tag survives into the
  synthesized file, which is a stock FLAC starting at `fLaC`.

### Changed

- `musefs-core` now builds its persistent virtual-tree collections on
  [`imbl`](https://crates.io/crates/imbl) 7 instead of the archived `im` 15.
  Clears RUSTSEC-2026-0248 / RUSTSEC-2023-0126 (`im`) and
  RUSTSEC-2026-0251 / RUSTSEC-2026-0255 (`sized-chunks`).
- `VirtualTree::children` now yields an opaque
  `impl ExactSizeIterator<Item = (&str, u64)>` instead of borrowing the backing
  `OrdMap`, taking the persistent-collection crate out of `musefs-core`'s
  public API so swapping it stays an internal detail. In-tree the only caller
  is `readdir`, which iterates; name lookups have always had
  `VirtualTree::lookup`.

### Fixed

- Bumped `crossbeam-epoch` 0.9.18 -> 0.9.20 (RUSTSEC-2026-0204, invalid
  pointer dereference in the `fmt::Pointer` impls) and `num-bigint`
  0.4.7 -> 0.4.8 (0.4.7 was yanked). Both are dev-dependency-only paths
  (criterion -> rayon, mp4 -> num-rational).

## [1.2.0] - 2026-06-18

### Changed

- Bare `scan` is now additive: it skips rows already in the DB instead of
  re-seeding them from disk. Use `scan --force` when you want the old
  full-reimport behavior.
- `revalidate` is now its own subcommand and no longer prunes by default. It
  refreshes changed rows' structural data while preserving curated tags/art;
  use `revalidate --prune` to drop rows whose backing file is gone and
  garbage-collect orphaned art.

### Deprecated

- `scan --revalidate` is a deprecated, warned alias for `revalidate` and will
  be removed next release. It does not prune; use `revalidate --prune` when you
  need deletion.

### Fixed

- Revalidating a changed file no longer clobbers curated tags, art, or binary
  tags in the DB.

## [1.1.0] - 2026-06-17

### Added

- **Runtime telemetry (`.musefs-metrics`):** an opt-in `--expose-metrics` flag
  (env `MUSEFS_EXPOSE_METRICS`) surfaces a synthetic `.musefs-metrics` file at
  the mount root rendering Prometheus-format counters — getattr/read/open
  activity, backing read-ahead behavior, and (when built with jemalloc)
  allocator stats. Off by default; the file is absent unless enabled. See the
  README [Metrics](guide/tuning.md#metrics) section (#394).
- **Scan progress indicator:** `scan` and `scan --revalidate` render a live
  progress bar (indicatif) with an elapsed-time summary on an interactive
  terminal, falling back to periodic `ingested N/M (P%)` log lines when output
  is non-interactive. A new `--quiet`/`-q` flag suppresses it (#406).
- **`--skip-on-missing` template flag:** an opt-in `--skip-on-missing` (env
  `MUSEFS_SKIP_ON_MISSING`) drops a track from the mount when a top-level
  template field stays unresolved, instead of substituting `--default-fallback`.
  Per-field `--fallback` chains and `[...]` optional sections are unaffected (a
  field resolved via its fallback counts as present). The motivating case is
  `--template '$!{beets_path}' --skip-on-missing`, which hides tracks beets left
  without a `beets_path` rather than collapsing them into an `Unknown` bucket
  (#408).
- **`--read-ahead-prefetch` flag:** opt-in background prefetch threads layered on
  top of read amplification, default off — benchmarks found amplification alone
  delivers the entire read-ahead win, while the threads add ~10% overhead with no
  measured benefit. Enable only when profiling a backend where a single large
  read does not self-pipeline (#255).
- **riscv64 release platform:** prebuilt `riscv64gc-unknown-linux-{gnu,musl}`
  binaries and `linux/riscv64` Docker images now ship with each tagged release.
  Container bases bumped to current stable: glibc Debian bookworm → trixie
  (bookworm has no riscv64 image), musl Alpine 3.20 → 3.23 (3.20 is end-of-life).
- **`statfs` reply:** the mount now reports a non-zero synthetic capacity with
  ample free space instead of fuser's all-zero default, so `df` no longer shows a
  0-byte filesystem and capacity-checking importers (Lidarr et al.) don't balk
  (#368).
- **Per-extension skip breakdown:** at end of scan, a summary line breaks the
  `skipped` count down by lowercased extension (e.g. `skipped 42: jpg=20,
  cue=10, log=8, <none>=4`), logged at `warn` so it shows by default, so a large
  skip count is diagnosable — expected sidecars versus genuinely unexpected
  files. Log-only; the `ScanStats` struct and CLI summary are unchanged (#341).
- **`musefs vacuum` command:** compact the SQLite store, reclaiming free pages
  left by prunes, orphan-art GC, and the schema migration. Runs `VACUUM` + a WAL
  checkpoint and reports the space reclaimed; run it while unmounted (#566).

### Fixed

- **Art/serve rowid-reuse consistency:** the read fast path's WAL-snapshot +
  `content_version` guard, previously gated only on binary-tag layouts, now
  covers all DB-rowid segments (art `ArtImage`/`OggArtSlice` too) via
  `RegionLayout::streams_db_rowid`, and the stateless no-fh read fallback now
  applies the same snapshot/recheck and re-validates its freshly opened backing
  fd against the resolved stamp. A concurrent external retag + `gc_orphan_art` +
  reinsert can no longer splice a wrong image or stale tag bytes mid-read (the
  audio-bytes invariant was never affected) (#502, #503).
- **Per-field `--fallback` case-insensitivity:** fallback keys are now ASCII
  lowercased to match template field names, so `--fallback AlbumArtist=…` (any
  uppercase) is honored instead of silently never matching (#504).
- **Tag value byte cap:** both the schema `CHECK` (rebuilt in the `MIGRATION_V2`
  upgrade) and the read-time `tags.value` guard now count bytes, not UTF-8
  characters, so the 256 KiB materialized-memory bound is exact rather than up to
  ~4x looser for multibyte text. The upgrade drops any pre-existing over-cap rows
  (already unreadable under the byte-counting reader guard) (#505).
- **Embedded NUL in ID3 metadata:** synthesized ID3 frames now reject a
  DB-sourced tag key, tag value, art mime, or art description containing an
  embedded NUL instead of emitting a frame a downstream parser would misread
  (#506).
- **Orphan-art GC NULL safety:** `gc_orphan_art` uses `NOT EXISTS` rather than
  `NOT IN (subquery)`, so a NULL `art_id` could not silently turn the GC into a
  no-op (#507).
- **Mount usability:** `mount` now warns when the mountpoint is non-empty (its
  contents are shadowed for the mount's lifetime), and a permission-denied mount
  (e.g. an AppArmor-restricted prefix) prints actionable guidance instead of a
  bare "Permission denied" (#508, #509).
- **Silent mp4 oversize drops:** oversized embedded `covr` cover art and binary
  freeform (`----`) values in `.m4a`/`.m4b` files are skipped in the format layer
  before materialization (to avoid building a large image out of a large `moov`),
  which previously dropped them with nothing in the logs. The scan now emits a
  `warn` line for each, matching the logging the other formats already had (#343,
  follow-up to #284).
- **xattr log noise:** `getxattr`/`listxattr`/`setxattr`/`removexattr` now reply
  `ENOTSUP` explicitly (read-only filesystem, no extended attributes) instead of
  falling through to fuser's default, which logged a `[Not Implemented]` warn on
  every xattr probe (`ls -l`, indexers, backup tools). The caller-visible result
  is unchanged (#364).
- **MP4 path-to-`ilst` leniency:** the walk to `moov/udta/meta/ilst` now uses the
  same lenient box scan as the metadata extractors, so a single malformed or
  truncated sibling box anywhere on the path no longer suppresses an otherwise
  well-formed `ilst` and silently drops every tag and cover. The audio/structure
  path stays strict (#542).
- **QuickTime bare `meta` atoms:** the `meta` parser only consumes the 4-byte
  FullBox version/flags prefix when it is actually present (a zero word), so a
  QuickTime-style bare `meta` — which has no such prefix — is read instead of
  landing mid-header and dropping all tags and art (#543).
- **`scan` exit code on ingest failure:** `scan`/`scan --revalidate` now exit `2`
  when any file fails to parse/ingest (`failed > 0`), instead of always exiting
  `0`. A pipeline such as `musefs scan … && musefs mount …` can now detect a
  partial or total ingest failure; a clean scan still exits `0` and a hard error
  still exits `1` (#554).
- **Release smoke audio-bytes check:** `scripts/smoke-binary.sh` (the per-arch
  release gate) now compares the served file's encoded audio stream against the
  untouched backing file, asserting the cardinal byte-identical-audio invariant
  rather than only checking the `fLaC` magic — so a target-specific positioned-read
  or offset regression in a cross-compiled binary is caught (#547).

## [1.0.0] - 2026-06-12

First stable release.

### Added

- **Lidarr integration:** a new `contrib/lidarr/` package that drives
  symlink-based placeholder imports and syncs Lidarr metadata into the musefs
  SQLite store.
- **FUSE mount-access controls:** new `--allow-other`, `--owner`, and `--group`
  flags mount with `allow_other` + `default_permissions` so accounts other than
  the mounting user can reach the view and the presented owner/group/mode bits
  are enforced; `--owner`/`--group` imply `--allow-other`. A non-root
  `allow_other` mount is pre-flight checked against `/etc/fuse.conf`
  `user_allow_other` and fails early with guidance if it is missing. See the
  README [Ownership and permissions](guide/configuration.md#ownership-and-permissions)
  section (#293, #294).
- **Hardened deployment assets:** the container image runs as a dedicated
  unprivileged user with a build-arg-configurable UID/GID, and the
  `musefs-scan.service` systemd unit ships a strong sandbox (the FUSE-mounting
  `musefs.service` deliberately cannot be sandboxed). See
  [the systemd hardening notes](integrations/systemd.md#hardening)
  (#317, #318, #319).
- **crates.io distribution:** the `musefs` binary is published to crates.io as of
  this release and installable with `cargo install musefs`. A new thin `musefs` wrapper crate
  owns the binary (`musefs-cli` is now a library crate), and a tag-triggered
  release workflow publishes all crates in dependency order.
- **Fuzzing & property tests:** coverage-guided `cargo-fuzz` targets for every
  format parser (FLAC, MP3, MP4, Ogg, WAV), the byte-level primitives (Ogg
  page parsing, base64 windowing, VorbisComment), and the serve path — the
  latter drives the full synthesis pipeline over hostile DB rows and binary tags
  via a fuzzing-gated `Db::with_raw_conn`. Plus `proptest` invariants —
  panic-freedom, the byte-identical audio guarantee, and tag round-trip — an
  end-to-end read-fidelity property, and a `mutagen` interop test asserting an
  independent reader sees the tags we synthesize.

### Changed

- **`mount --db` now requires an existing store.** Mounting against a missing
  database path is rejected before any FUSE setup instead of silently creating
  and migrating an empty store, so a mistyped `--db` fails loudly rather than
  mounting an empty view. `scan --db` still creates the store if absent (#309).

### Fixed

- **Scanner no longer drops files and embedded art silently:** embedded cover
  art over `MAX_ART_BYTES` (and binary tags over `MAX_BINARY_TAG_BYTES`) were
  filtered out at ingest with no log line, so a track whose art exceeded the cap
  appeared to simply have none — indistinguishable from a scan bug. The drop is
  now logged (`RUST_LOG=warn`). Likewise, a supported-extension file that fails
  to parse or errors mid-probe was counted `failed` with the underlying error
  discarded; the reason is now logged. Note: oversized art in `.m4a`/`.m4b`
  files is dropped earlier, inside the format layer, and is not yet logged
  (#284, #343).
- **Lidarr custom-script env var casing:** Lidarr stores custom-script
  environment variables in a .NET `StringDictionary`, which lowercases every key,
  so a Linux script actually receives `lidarr_sourcepath` / `lidarr_eventtype`
  rather than the PascalCase names Lidarr's docs list. The integration read the
  PascalCase names, so with a real Lidarr every import failed and every event
  parsed as unsupported. Lidarr env vars are now resolved case-insensitively.
  Found by the issue #141 real-instance smoke run.
- **VorbisComment parse OOM (DoS):** a crafted comment block declaring a huge
  entry count made `Vec::with_capacity` attempt a multi-gigabyte allocation; the
  pre-allocation is now bounded by the readable byte count. Found by the new
  `vorbiscomment` fuzz target.
- **MP4 box-bounds integer overflow:** an untrusted 64-bit extended box size made
  the box-bounds check (`pos + total`) overflow `usize` — a panic in debug and a
  silent wrap in release that accepted a bogus box length. The addition is now
  checked. Found by the `mp4` fuzz target.
- **ID3v2 parsing unbounded allocation (DoS):** the `id3` crate eagerly allocates
  a frame's declared size (ID3v2.3 frame sizes are plain 32-bit, up to 4 GiB), so
  a crafted tag could exhaust memory at scan time — via an MP3 or a WAV embedded
  `id3 ` chunk. Parsing is now gated on validated ID3v2 frame bounds and an
  ID3v2 tag at offset 0 (the `id3` reader scans forward). Found by the `mp3` and
  `wav` fuzz targets.
- **Scan counters now match their documented contract:** `musefs scan` reports
  every non-audio file (any unsupported or missing extension — `.jpg`, `.cue`,
  `.log`, `.nfo`, cover art, etc.) as `skipped`, and supported-extension files
  that fail to parse (e.g. a corrupt `.flac`) as `failed`. Previously malformed
  files were miscounted as `skipped` and unsupported files were not counted at
  all, so expect `skipped` to be larger than before on a real library (#301).
- **Symlink scans no longer double-count:** with `--follow-symlinks`, a file
  reached via both its real path and a symlink is ingested and counted once
  instead of inflating `scanned`; multiple hardlinks to the same inode are
  likewise collapsed to a single track (#302).
- **Stable inodes on case-insensitive mounts:** the inode allocator is now keyed
  on the case-folded path in case-insensitive mode, so an unrelated deletion that
  flips a merged directory's display casing no longer reassigns a survivor's
  inode (#305).
- **Lidarr autoscan now honors the scan timeout:** an import/release-triggered
  autoscan applies the shared 120s scan timeout, matching the beets and Picard
  integrations, so a wedged `musefs scan` fails with a controlled timeout instead
  of blocking the custom-script process indefinitely (#312).

## [0.2.0] - 2026-05-27

First public release.

### Added

- **Formats:** synthesis for M4A/M4B (MP4), Ogg (Opus, Vorbis, FLAC-in-Ogg), and
  WAV, alongside the existing FLAC and MP3 — metadata generated on the fly from
  the SQLite store and spliced in front of byte-identical backing audio.
- **Arbitrary tag support:** a single canonical tag vocabulary maps common fields
  to each format's native slot (ID3 frame / MP4 atom / Vorbis field); any other
  tag round-trips through the format's extension slot (ID3 `TXXX`, MP4 `----`
  freeform, raw Vorbis field). User-defined key casing is preserved.
- **beets plugin** (`contrib/beets/`): syncs beets' canonical tags and cover art
  into the store keyed by each file's real path, with no remount and no audio
  rewrite.
- **Performance, concurrency & caching pass:** worker-pool offload of blocking
  reads, lock-free virtual-tree swap, per-handle I/O, a bounded LRU header-layout
  cache, debounced single-flighted refresh with stable inodes, kernel/mount
  tuning flags, bounded-memory MP4 resolves, and opt-in `--keep-cache` with
  auto-invalidation.

### Notes

- Read-only mount; tag edits happen out-of-band against the SQLite store and are
  picked up automatically (`PRAGMA data_version` polling). See the README
  [Supported formats](formats/overview.md#supported-formats) section and the per-format
  docs for round-trip limitations.

## [0.1.0]

- Initial MVP (FLAC and MP3 synthesis, virtual tree with beets-style templates,
  `synthesis` / `structure-only` mount modes, auto-refresh, `scan` /
  `scan --revalidate`). Never published publicly; superseded by 0.2.0.

[Unreleased]: https://github.com/Sohex/musefs/compare/v2.0.0...HEAD
[2.0.0]: https://github.com/Sohex/musefs/releases/tag/v2.0.0
[1.3.0]: https://github.com/Sohex/musefs/releases/tag/v1.3.0
[1.2.0]: https://github.com/Sohex/musefs/releases/tag/v1.2.0
[1.1.0]: https://github.com/Sohex/musefs/releases/tag/v1.1.0
[1.0.0]: https://github.com/Sohex/musefs/releases/tag/v1.0.0
[0.2.0]: https://github.com/Sohex/musefs/releases/tag/v0.2.0
[0.1.0]: https://github.com/Sohex/musefs/releases/tag/v0.1.0
