# Release notes

Curated, upgrade-focused notes for each release. For the exhaustive,
per-change list see the [Changelog](changelog.md); for the external-writer
`contrib/` packages (which version independently) see the
[contrib changelog](integrations/overview.md#contrib-changelog).

## v2.0.0

The first major release. At its centre is the store: one schema migration, to
version 4, that makes a track's identity, its backing path and its picture
metadata mean what they say. It is applied by an explicit `musefs migrate`
rather than silently on open. Around it ride the breaking cleanups a major
version allows, and everything merged since v1.3.0. There is no 1.4.0: the
features built during 1.x ship here, so the highlights below include them. Read
[Upgrading from v1.3.0](#upgrading-from-v130) before installing it, because the
store upgrade is one-way without the snapshot `migrate` takes.

### Highlights

- **`musefs migrate`** ([#705], [#706], [#749]). A store change too invasive to
  apply on open — one that rewrites data, needs the store's size again in free
  disk, or locks older builds out — now happens only when you ask. `migrate`
  reports what it will do, checks every row against the new schema, snapshots
  the store, then upgrades it. Until it has run, every other command refuses
  the store without changing its data or schema version, so the previous
  release still opens it.
- **A store that means what it says** ([#674], [#678], [#680], [#693], [#716],
  [#717], [#718]):
  - track ids are never reused, so a deleted track's id cannot bless another;
  - backing paths are the filesystem's bytes, so two non-UTF-8 names no longer
    collapse into one track;
  - the freshness stamp carries the inode, wherever the filesystem keeps one;
  - picture metadata belongs to each file rather than the shared image;
  - tag and art rows cannot move between tracks;
  - every column's storage class is enforced.
- **Modification times that move with the bytes** ([#725], [#696]). A tag edit
  is visible to anything comparing size and mtime, and a pre-1970 backing file
  is stored instead of refused.
- **`readdirplus`** ([#667]). A client that stats what it lists — `ls -l`, every
  media scanner — spends one round trip on the directory instead of one per
  entry.
- **Chaptered `.m4b` audiobooks scan** ([#672]) instead of counting as
  unparseable.
- **`--trust-backing-mtime`** ([#668]) skips the backing `stat` on a metadata
  cache hit, for network or spun-down backings.
- **A scan survives a rejected file** ([#662]). A row the store refuses fails
  that one file, and the scan runs to completion and exits `2`.
- **Hardening across the serve path and the store**:
  - backing changes are validated after the read, not before ([#682]), and an
    open file drops the read-ahead it cached before a backing rewrite once the
    row is restamped to match;
  - an over-cap directory listing stays stable across a refresh ([#695]);
  - metadata work on the worker pool is admission-controlled ([#694]);
  - content-addressed art is verified ([#724]);
  - under `--follow-symlinks`, a link is scanned by its target's format, not
    its own name, whether the walk reaches it or it is the scan root ([#766]);
  - chained Ogg is refused, and an old row for one is removable ([#722],
    [#747]).
- **Directory handles share one listing** ([#675]). Handles open on the same
  directory at the same tree generation share a listing instead of each copying
  it, so a client holding many handles on a wide directory no longer pins memory
  in proportion to the handle count. `musefs_dir_listings` reports how many
  distinct listings are held.
- **Wider tag and description caps.** A `tags.value` may hold up to
  16 MiB − 1 bytes (was 256 KiB) and a `track_art.description` up to 8,192
  characters (was 1,024). This is schema version 3, which only widens
  constraints and carries every row across; `musefs migrate` applies it together
  with version 4.

See the [Changelog](changelog.md#200---2026-09-14) for the full list.

### Upgrading from v1.3.0

The store's schema changes in this release, and part of that change is one
musefs will not make without being asked. Steps 1 to 4 are that upgrade and
what follows it. Do them in order, and before anything else below.

**1. Run `musefs migrate`** ([#705],
[#706]). `mount`, `scan`, `revalidate` and `vacuum` refuse a 1.3.0 store and
name the command:

```bash
musefs migrate --db library.db
```

Stop the old mount and any scheduled scan first. The commands that refuse the
store change neither its data nor its schema version, so 1.3.0 still opens it
until `migrate` has run ([#749]). Its file bytes can still change: closing the
store checkpoints any write-ahead-log frames left pending into the database.

`migrate` refuses a store anything else has open — a mount, even an idle one, a
running scan, another `migrate`. In a script it needs `--yes`, since there is no
terminal to confirm on, and its two follow-up offers (step 4) decline unless
`--vacuum` / `--revalidate` ask for them. Once the store is upgraded, musefs
1.1.0 to 1.3.0 refuse to open it. 1.0.x has no such check and does not refuse a
newer store, so never run a 1.0.x binary against an upgraded one. The
[maintenance guide](guide/maintenance.md#upgrading-the-store-musefs-migrate) has
the full walkthrough and flag table.

**Containers.** The floating `:latest` and `:musl` image tags move to 2.0.0
with this release. A container that pulls them automatically refuses its 1.x
store, and will not start until `musefs migrate` has run against the store
volume. Pin `:1.3.0` (or `:1.3.0-musl`) until you are ready to upgrade.
[Running in containers](guide/containers.md) shows how to run `migrate` from
the image.

**2. Disk space, and the way back** ([#705]). Before asking anything, `migrate`
checks for free space next to the store: the store's on-disk size (the database
with its `-wal` and `-shm`) for the rewrite, and the same again for the
snapshot when it is written to the same filesystem as the store — beside it by
default, or at a `--snapshot` path on that filesystem. If that is not there it
refuses up front, rather than failing part-way.

The snapshot is a single compacted copy at `<db>.v<version>.bak`, where the
version is the one the store is at when `migrate` runs — `library.db.v2.bak` for
a store 1.3.0 left. `--snapshot PATH` puts it elsewhere; `--no-snapshot` skips
it, and the upgrade is then one-way. `migrate` refuses to overwrite an existing
snapshot. There is no restore command: to go back, stop everything, replace the
store with the snapshot, delete any leftover `library.db-wal` and
`library.db-shm`, and run 1.3.0.

The upgraded store is larger than the old one. `migrate` says by how much and
offers a `vacuum`.

**3. Rows the new schema refuses** ([#705]). This release tightens what a store
row may hold ([#693], [#716], [#718]). Before anything is copied or written,
`migrate` offers every row to the new tables. If any are refused, it reports how
many per table and stops, with nothing changed. The refused shapes are ones a
writer has to produce by binding the wrong type or by turning the store's
constraints off:

- a value stored in the wrong class — text in a blob column, bytes in a text
  column, a fractional number where an integer belongs;
- an embedded NUL in a tag key, a picture's MIME type or description, or an art
  row's digest;
- a picture dimension past `u32`, an empty `backing_path`, or one over 64 KiB
  ([#758]);
- an art row whose digest is not 64 lowercase hex characters ([#761]);
- a tag, picture link or structural block whose track is gone, or a link whose
  image is.

Fix the rows with whatever wrote them, or pass `--repair` to have `migrate`
delete them. It does so after the confirmation and the snapshot, so the rows are
still in the copy — which is why `--repair` refuses `--no-snapshot`. Deleting a
track takes its tags and picture links with it, and the report counts those too.

An art row with a non-canonical digest can only have come from a tool other than
musefs or its plugins, which have always written lowercase. `--repair` deletes
that row **and every picture link to it**, so each track using it loses that
picture. It is not lowercased for you, because a correctly filed row for the
same image may already exist. Fixing it by hand first means inserting a
correctly filed row, relinking each `track_art` row to it and deleting the old
one — art rows cannot be updated — and the
[maintenance guide](guide/maintenance.md#rows-the-new-schema-refuses) walks
through the catches.

**4. Revalidate afterwards.** Accept `migrate`'s offer to revalidate your
library, or run `musefs revalidate /path/to/music --db library.db` yourself. The
upgrade leaves several things only a revalidate puts right, and it is the
**first** revalidate that does it, as long as it runs at the default checksum
tier or above, as the offer does. A `--checksum=none` revalidate skips an
unchanged file on a filesystem whose inode numbers musefs does not record (see
[Freshness](architecture/tree-scanning.md#freshness-two-version-counters)), so
there it restores neither picture metadata nor an Ogg FLAC's bounds.

The offer revalidates the deepest directory every stored track shares. Stored
paths are already resolved, symlinks included, so that walk reaches every track
without following symlinks. If your tracks resolve into unrelated trees — a
library of symlinks pointing into two different disks, say — they share no
directory below `/`, so `migrate` makes no offer and prints the command instead:
run `musefs revalidate` over each tree yourself. Until
every track has been re-probed, `mount`, `scan` and `revalidate` each warn with
the number still waiting ([#705]). That number counts tracks with neither a
fingerprint nor an inode, so on Linux a track on FAT or exFAT that was scanned at
`--checksum=none` stays in it until a default-tier `revalidate` records its
fingerprint. What waits for the revalidate:

- **Fingerprints are cleared** ([#691]). Until they are recomputed, a moved file
  is not recognised: `scan` ingests it as a new track and leaves its curated
  row behind. A plain `scan` does not recompute them for files already in the
  store; `revalidate` does.
- **Full-file hashes are cleared too** ([#689]). A 1.x rescan could leave a
  row's `content_hash` describing bytes its file no longer holds, so none is
  carried across. If you scan with `--checksum=full`, run
  `musefs revalidate --checksum=full` to recompute them. Until then
  `--match=auto` confirms a move by fingerprint alone, and `--match=strict`
  refuses to retarget.
- **Stored inodes start unknown** ([#674]). The check that catches a backing
  file replaced in place cannot use the inode until a revalidate records it.
  On Linux, none is ever recorded on FAT and exFAT ([#757]): those filesystems
  renumber files on every mount, so an inode there would fail every file after a
  replug.
  The check is weaker on them as a result, and they are
  [not recommended](guide/installation.md) for the backing library.
- **Picture metadata is copied, not per file** ([#716], [#746]). 1.3.0 kept one
  MIME type and one set of dimensions per image, so the upgrade copies those
  onto every file that embeds it, with FLAC's bit depth and colour count at 0.
  The revalidate restores each file's own values for the pictures the file
  itself embeds. A picture a plugin linked keeps what the plugin wrote.

A few files need more than that:

- **Ogg FLAC with a zero header-packet count** ([#723]). The revalidate corrects
  where their audio starts. Tags and art that 1.3.0 never read from those files
  arrive only through `musefs scan --force <file>`, which replaces that file's
  curated tags and art with what it embeds.
- **Chained Ogg** stored by 1.3.0 ([#722], [#747]). 2.0.0's scan refuses these,
  so they cannot be refreshed. Each counts as `failed` (reason `unsupported`),
  and `revalidate` exits `2` while any remain. Until they are removed, the mount
  still plays a chain's first stream, but reads into its second stream fail with
  `EIO`. `musefs revalidate --prune`
  removes them. The run that does so still exits `2`, and the next one does not.
  `migrate`'s offer never prunes; if its revalidate counts failures,
  `migrate` exits `2` ([#750]).
- **Files whose names are not valid UTF-8** ([#680]). 1.3.0 stored each under
  a mangled path it could not serve, and merged two whose names differed only in
  those bytes into one track. A `scan` adds the real files as new tracks. Until
  `revalidate --prune` removes the mangled row, along with any tags you had put
  on it, the mount lists that row as an entry that fails to open, and a real
  file with the same displayed name appears with a ` (2)` suffix.

**Sync tools will see changed files, twice unless you revalidate first**
([#725], [#696]). A synthesized file's modification time now carries its
content version in the nanoseconds, so a tag edit is visible to a tool that
compares size and mtime. 1.3.0 served whole seconds, so on the first mount
nearly every synthesized file's mtime changes. The first revalidate then moves
it again. The upgrade cleared every fingerprint, so at the default checksum tier
that revalidate re-probes every file, and a re-probe moves a file's mtime only
where it records something the store did not hold ([#757]). Except on FAT and
exFAT under Linux, that includes the file's inode, recorded for the first time,
so nearly every file's mtime moves again, seconds included. On Linux, musefs
records no inode on FAT and exFAT, because they keep no stable inode numbers, so
there the mtime moves only where the revalidate corrects what
the store holds for the file: its picture metadata, an Ogg FLAC's bounds, or
FLAC structural data an older scan never recorded. A restored picture or Ogg
FLAC bound can change the size too. To have rsync without `--checksum`,
Syncthing or a backup tool re-copy the library once rather than twice,
revalidate before the first sync from the new mount. A whole-second comparison
sees only the second change. `--mode structure-only` is unaffected.

**`scan` exits `2` when the store rejects a file** ([#662]). A constraint
violation on one file used to stop the scan with exit `1`. Now that file fails,
everything else is stored, and the scan exits `2` with the file counted under
`rejected` in the `failed N: …` summary. A script that treated exit `1` as "the
store refused something" should check for `2` and read the summary.

**Other `scan` output a script may read:**

- A file whose metadata exceeds a store limit — an oversize tag, picture or
  binary tag — now fails, counted under `oversize` ([#644], [#651]). 1.3.0
  stored such a file without the oversize picture or binary tag, and stopped
  the whole scan on an oversize tag.
- A `--checksum=full` scan that cannot hash a file now fails it, counted under
  `checksum-failed`, instead of storing it with no full-file hash ([#690]).
- Piped progress lines read `processed N/M (P%)`, not `ingested N/M (P%)`, and
  now reach `100%` when files fail ([#655]).
- The per-extension breakdown of `skipped` is logged at `info`, not `warn`, so it
  needs `-v` or `RUST_LOG=info`. The `skipped N` count still prints at any level
  ([#651]).

**Scan flags removed.** Both changes fail loudly rather than quietly doing
something different, so a script or unit that needs updating will tell you:

- **`scan --revalidate` is gone**, with its `MUSEFS_REVALIDATE` variable
  ([#707]). It has been a deprecated alias since v1.2.0. Run
  `musefs revalidate` instead — the alias never pruned, so neither does the
  replacement unless you add `--prune`. The flag is now a usage error (exit `2`).
- **`scan --fast` and `--strict` are replaced by `--match`** ([#709]):
  `--fast` becomes `--match=fast` and `--strict` becomes `--match=strict`. If you
  passed neither, there is nothing to change: `--match=auto` is the default and
  behaves exactly as before. The variables follow: `MUSEFS_FAST=true` becomes
  `MUSEFS_MATCH=fast`, `MUSEFS_STRICT=true` becomes `MUSEFS_MATCH=strict`. The old
  flags are usage errors (exit `2`).

**Retired variables stop `scan`.** An environment variable that no flag reads
any more would otherwise be ignored in silence — turning a revalidate into a full
scan, or quietly weakening how a moved file is confirmed. So `scan` refuses to
start (exit `1`) while `MUSEFS_REVALIDATE`, `MUSEFS_FAST` or `MUSEFS_STRICT` is
set, and names the replacement. That includes a value of `false`: delete the
line from a systemd `EnvironmentFile=` or container environment rather than
switching it off. An empty value is treated as unset. `mount` does not read
these variables and is unaffected, so an environment file shared by the mount
and scan units only matters to the scan.

**`vacuum` refuses a store that is in use.** It always said it did, but it only
noticed a mount that was actively reading, and compacted the store underneath
one sitting idle. It now refuses while any mount or scan has the store open
([#721]). A scheduled `vacuum` that used to run while the library was mounted
will start failing with *the store is in use*; stop the mount around it.

**External writers.** Upgrade the `contrib/` packages together with musefs:
they open only a store at the new schema version, just as musefs 1.3.0 refuses
the new one. The [contrib changelog](integrations/overview.md#contrib-changelog)
has the package side. For anything writing to the store without them, this is
what changed underneath:

- `tracks.backing_path` is a `BLOB` holding the filesystem's bytes, not text
  ([#680]), of at most 64 KiB ([#758]). Bind paths as bytes; a text value is
  refused. The helpers'
  `path_param` and `path_value` do this, and `realpath_key` returns a non-UTF-8
  name as the string `os.fsdecode` gives, not a lossy one.
- A picture's MIME type and dimensions describe one file's embedding, so they
  moved from `art` to `track_art`, which also gains `depth` and `colors`
  ([#716]). `upsert_art` takes only the bytes, and `replace_track_art` takes
  `(art_id, picture_type, description, mime)` or
  `(art_id, picture_type, description, mime, width, height)` rows; any other
  length, the 1.x three-field form included, raises `ValueError`. A link written
  without a MIME type is served with an empty one. `image_dimensions` reads the
  width and height from a PNG or JPEG header, which is how `sync_files` now fills
  them ([#737]).
- `tags` has no primary key any more. A unique index on
  `(track_id, key, ordinal, (value_blob IS NULL))` takes its place, so text and
  binary tags number their ordinals independently, and an upsert naming
  `ON CONFLICT(track_id, key, ordinal)` no longer matches a constraint ([#663]).
- `art` rows cannot be changed once written, and a row filed under a digest must
  hold the bytes that digest names. `upsert_art` raises `ArtDigestMismatch` when
  it does not ([#724]).
- That digest is the lowercase hex SHA-256 of the bytes, and the store refuses
  any other spelling; `fingerprint` and `content_hash` follow the same grammar
  ([#761]). `upsert_art` produces the canonical form.
- A tag or picture link cannot be moved to another track by updating its
  `track_id`; delete it and insert it under the new one ([#717]).
- A track's `id` cannot be changed once assigned. It is the identity the mount's
  refresh keys on, so the store refuses the update ([#762]).
- A `structural_blocks` row cannot be updated in place ([#759]). The table is
  scanner-owned and outside this contract, and the scanner replaces a track's
  blocks by delete-then-insert; the store now refuses the `UPDATE` a writer
  ignoring the contract could make.
- In the helpers, `run_scan` returns a `ScanResult` for a partial scan (exit
  `2`) instead of raising ([#647]), so a caller that relied on the exception
  needs updating. `MAX_TAG_VALUE_LEN` exports the store's byte cap on
  `tags.value`.

The scan-flag changes above need no plugin update: the packages have called the
`revalidate` subcommand since their 1.2.0 and pass neither `--fast` nor
`--strict`.

**Rust crate API.** This only affects code depending on the musefs crates
directly.

- The store model follows the schema ([#674], [#680], [#716]).
  - `Track` and `NewTrack` carry `backing_path` as a `PathBuf` and gain
    `backing_ino`. `Db::get_track_by_path` and `Db::retarget_track` take a
    `&Path`.
  - `Db::track_version_and_path` is replaced by `Db::track_identity`, which
    returns the new `TrackIdentity`.
  - `NewArt` is only the bytes, and `Art` and `ArtMeta` lose the MIME type and
    dimensions.
  - `TrackArt` gains `mime`, `width`, `height`, `depth` and `colors`, and the
    synthesis inputs `ArtInput` and `EmbeddedPicture` gain `depth` and `colors`.
  - `NewTrack`, `TrackArt`, `ArtInput` and `EmbeddedPicture` stay exhaustive, so
    code that builds them must fill the new fields.
  - `Db::refresh_embedded_art` and its `EmbeddedArt` input are new ([#746]), as
    is `Db::count_tracks_awaiting_revalidate` ([#705]).
  - `DbError::FieldTooLarge` gains `unit`, saying whether it measured bytes or
    characters ([#693]).
- `Db::set_track_checksums` and the retarget writer take a `ChecksumWrite`
  instead of an `Option<&str>` ([#689]).
- `Db::open` refuses a store that needs a gated step, with
  `DbError::StoreNeedsMigration`. `PendingMigration` is the API `musefs migrate`
  drives ([#705], [#706]). `DbError::StoreInUse` now names the operation it
  refused, and `DbError::ArtDigestMismatch` is new ([#724]).
- `CoreError::BackingChanged` carries the path as a `PathBuf`. The messages it
  used to carry in its place are now `CoreError::DerivedStateStale` ([#680]).
- `Attr::mtime_secs` is replaced by `mtime: Option<VirtualMtime>`, which is
  `None` only for a virtual directory, and `ResolvedFile::mtime_secs` by
  `mtime: VirtualMtime` ([#696], [#725]).
- `BackingStamp` gains `ino`. Compare a stored stamp with a live one through
  `matches_live`, not `==` ([#674]).
- `Segment::OggAudio` gains `serial: u32`, the stream's serial, which the serve
  path checks every page against. `Segment` stays exhaustive ([#722]).
- `VirtualTree::build`, `build_with` and `build_with_ci` take rendered paths as
  `Arc<str>`, and `Node`'s names and `remove_track`'s result are `Arc<str>`
  ([#617], [#629]).
- `render_prometheus` takes a `&ProcessStats` ([#631]), and `FuseTelemetry`,
  which the caller fills in and which stays exhaustive, gains `dir_listings`,
  `dir_handle_rejections`, `readdirplus_calls` and `pool_over_cap`.
- `ChangelogRead` gains `malformed`, set when `changelog_since` skipped a
  changelog row whose track id was not an integer. The refresh treats that
  window as a gap and rebuilds ([#760]).
- `musefs_cli::run_scan` no longer takes `revalidate`, and takes a
  `musefs_cli::MatchMode` in place of `fast`/`strict`.
- The public enums a caller matches on — the error types, `Format`, the scan and
  mount option enums, and `musefs-cli`'s `Command` and value enums — are
  `#[non_exhaustive]` ([#708]). A `match` on one outside its crate needs a
  wildcard arm; in exchange, a new audio format or error case is no longer a
  breaking change. The changelog lists which enums, and which were deliberately
  left exhaustive.
- Test scaffolding is no longer public ([#710]):
  `musefs_core::scan_directory_full_oracle`, the `*_for_test` methods on `Musefs`
  and `Db`, `musefs_db::seed_store_at_version` ([#751]), and
  `musefs_format::ogg::page_test_support`. No production code called any of
  them.
- The configuration and result structs follow the enums ([#743]):
  `ScanOptions`, `MountConfig`, `FuseConfig`, `musefs-cli`'s argument structs and
  the crates' result types are `#[non_exhaustive]`, so outside their crate they
  can no longer be built with a struct literal, `..Default::default()` included.
  Start from `default()` — new for `MountConfig`, matching a bare `musefs mount`
  — and assign the fields you change. The store-row and synthesis input structs
  (`NewTrack`, `TrackArt`, `ArtInput` and the like) are not marked, so a new
  store column is still a breaking change for code that writes rows, as the new
  fields above are.

[#617]: https://github.com/Sohex/musefs/issues/617
[#629]: https://github.com/Sohex/musefs/issues/629
[#631]: https://github.com/Sohex/musefs/issues/631
[#644]: https://github.com/Sohex/musefs/issues/644
[#647]: https://github.com/Sohex/musefs/issues/647
[#651]: https://github.com/Sohex/musefs/issues/651
[#655]: https://github.com/Sohex/musefs/issues/655
[#662]: https://github.com/Sohex/musefs/issues/662
[#663]: https://github.com/Sohex/musefs/issues/663
[#667]: https://github.com/Sohex/musefs/issues/667
[#668]: https://github.com/Sohex/musefs/issues/668
[#672]: https://github.com/Sohex/musefs/issues/672
[#674]: https://github.com/Sohex/musefs/issues/674
[#675]: https://github.com/Sohex/musefs/issues/675
[#678]: https://github.com/Sohex/musefs/issues/678
[#680]: https://github.com/Sohex/musefs/issues/680
[#682]: https://github.com/Sohex/musefs/issues/682
[#689]: https://github.com/Sohex/musefs/issues/689
[#690]: https://github.com/Sohex/musefs/issues/690
[#691]: https://github.com/Sohex/musefs/issues/691
[#693]: https://github.com/Sohex/musefs/issues/693
[#694]: https://github.com/Sohex/musefs/issues/694
[#695]: https://github.com/Sohex/musefs/issues/695
[#696]: https://github.com/Sohex/musefs/issues/696
[#705]: https://github.com/Sohex/musefs/issues/705
[#706]: https://github.com/Sohex/musefs/issues/706
[#707]: https://github.com/Sohex/musefs/issues/707
[#708]: https://github.com/Sohex/musefs/issues/708
[#709]: https://github.com/Sohex/musefs/issues/709
[#710]: https://github.com/Sohex/musefs/issues/710
[#716]: https://github.com/Sohex/musefs/issues/716
[#717]: https://github.com/Sohex/musefs/issues/717
[#718]: https://github.com/Sohex/musefs/issues/718
[#721]: https://github.com/Sohex/musefs/issues/721
[#722]: https://github.com/Sohex/musefs/issues/722
[#723]: https://github.com/Sohex/musefs/issues/723
[#724]: https://github.com/Sohex/musefs/issues/724
[#725]: https://github.com/Sohex/musefs/issues/725
[#737]: https://github.com/Sohex/musefs/issues/737
[#743]: https://github.com/Sohex/musefs/issues/743
[#746]: https://github.com/Sohex/musefs/issues/746
[#747]: https://github.com/Sohex/musefs/issues/747
[#749]: https://github.com/Sohex/musefs/issues/749
[#750]: https://github.com/Sohex/musefs/issues/750
[#751]: https://github.com/Sohex/musefs/issues/751
[#757]: https://github.com/Sohex/musefs/issues/757
[#758]: https://github.com/Sohex/musefs/issues/758
[#759]: https://github.com/Sohex/musefs/issues/759
[#760]: https://github.com/Sohex/musefs/issues/760
[#761]: https://github.com/Sohex/musefs/issues/761
[#762]: https://github.com/Sohex/musefs/issues/762
[#766]: https://github.com/Sohex/musefs/issues/766

## v1.3.0

A compatibility and maintenance release. The headline change makes musefs
ingest **FLAC files that carry a leading ID3 tag** instead of rejecting them,
and the dependency tree is clear of open RUSTSEC advisories. The on-disk schema
is unchanged (still version 2) and no CLI flag or default moved, so upgrading is
a drop-in — but a re-scan is worth running, see
[Upgrading from v1.2.0](#upgrading-from-v120).

### Highlights

- **FLAC with a leading ID3 tag now scans** ([#602]). Some FLAC files put one or
  more ID3v2 tags in front of the `fLaC` marker — non-standard, but common in
  the wild, often a blank header left behind by a converter. musefs used to
  reject these with `no parseable audio metadata`, counting them `failed`; they
  now parse normally.
- **Their ID3 tags come with them.** Text frames and embedded cover art in the
  ID3 header are ingested as a *fallback*: the FLAC's own `VORBIS_COMMENT` and
  `PICTURE` blocks win, and the ID3 tag only supplies what the FLAC itself does
  not carry. A FLAC whose tags live only in its ID3 header therefore arrives
  tagged rather than blank. The served file is a stock FLAC with no ID3 tag —
  the tag is metadata, and musefs regenerates metadata rather than copying it.
- **No open security advisories.** `musefs-core`'s persistent collections moved
  from the archived `im` crate to the maintained `imbl` fork, clearing two
  unsoundness advisories along with the unmaintained ones, and a
  `crossbeam-epoch` vulnerability was patched. Both lockfiles were refreshed.

See the [Changelog](changelog.md#130---2026-08-19) for the full list.

### Upgrading from v1.2.0

**No schema migration.** The store stays at `user_version` 2, and you can roll
back to v1.2.0 without touching it.

**Re-scan to pick up previously-rejected files.** FLACs with a leading ID3 tag
were never added to the store, so a bare `musefs scan <dir>` ingests them now —
no `--force` needed, and files already tracked are left alone. If a scripted
pipeline was exiting **2** because of these files (`failed Y` with `Y > 0`), it
will now succeed.

**`musefs-core` API.** `VirtualTree::children` returns an opaque
`impl ExactSizeIterator<Item = (&str, u64)>` instead of borrowing the backing
`OrdMap`. This only affects code depending on the `musefs-core` crate directly;
callers that iterated are unaffected apart from the item type, and by-name
lookups have always had `VirtualTree::lookup`. The change takes the
persistent-collection crate out of the public API, so swapping it again cannot
break a downstream consumer.

**External writers.** The `contrib/` packages are unchanged this cycle; no
update is needed alongside musefs.

[#602]: https://github.com/Sohex/musefs/issues/602

## v1.2.0

A behavior-focused release: the headline change makes `scan` **non-destructive
by default**. The on-disk schema is unchanged (still version 2), so there is no
migration — but a default that previously overwrote curated metadata has been
removed, so read [Upgrading from v1.1.0](#upgrading-from-v110) before you update
any automation that calls `scan`.

### Highlights

- **`scan` is now additive.** A bare `musefs scan` ingests only files not already
  in the store and **never overwrites curated tags or art**. Re-running it to
  pick up newly-added files is safe; the old full-reimport-from-disk behavior is
  now opt-in via `scan --force`. This closes a footgun where a routine re-scan
  silently clobbered tags written by Picard / beets / the store.
- **`revalidate` is its own subcommand.** Promoted from the `scan --revalidate`
  flag, it refreshes the structural/serving facts (audio bounds, checksums, FLAC
  structural blocks) of in-store files whose backing bytes changed, while
  **preserving curated tags, art, and binary tags**. Files not yet in the store
  are ignored — that is `scan`'s job.
- **Deletion is opt-in.** `revalidate` no longer prunes by default; pass
  `revalidate --prune` to drop rows whose backing file is gone and
  garbage-collect orphaned art. No bare command deletes or overwrites store data.
- **Data-loss fix.** Revalidating a changed file no longer clobbers curated tags,
  art, or binary tags — previously the structural/checksum backfill routed
  through a full re-seed from disk.

See the [Changelog](changelog.md#120---2026-06-18) for the full list.

### Upgrading from v1.1.0

**No schema migration.** The store stays at `user_version` 2; nothing about the
on-disk format changes, and you can roll back to v1.1.0 without touching it.

**Behavior changes to check:**

- **Bare `scan` no longer re-imports existing tracks.** Automation that runs
  `musefs scan <dir>` expecting it to refresh tags on files already in the store
  now **skips** them (reported as `already present`). To re-seed curated metadata
  from disk, use `scan --force`. The common case — re-running `scan` to pick up
  newly-added files — is unchanged.
- **`scan --revalidate` is deprecated.** It still works but prints a warning and
  forwards to the non-pruning `revalidate` path; it will be removed next release.
  Switch to the `musefs revalidate` subcommand — and note it **no longer prunes**,
  so add `--prune` for the old delete-gone-rows-and-GC behavior.
- **`revalidate` does not prune by default.** Scripted maintenance that relied on
  `scan --revalidate` to remove rows for deleted files must now pass
  `revalidate --prune` explicitly.

**External writers.** The `contrib/` packages are bumped to 1.2.0; the beets and
Picard plugins are updated for the new CLI — the beets plugin's autoscan now
resets the store with `scan --force`, and `beet musefs --revalidate` maps to
`revalidate --prune` (both transparent to you). Update these packages alongside
the binary; older copies invoke the deprecated `scan --revalidate` alias, which
still works for this release.

## v1.1.0

A feature-and-hardening release on top of the v1.0.0 stable line. No CLI flags
or store columns were removed, but the on-disk schema steps to **version 2** and
a few defaults change observable behavior — read [Upgrading from
v1.0.0](#upgrading-from-v100) before you update an existing store.

### Highlights

- **Runtime telemetry.** An opt-in `--expose-metrics` (env
  `MUSEFS_EXPOSE_METRICS`) surfaces a synthetic `.musefs-metrics/` directory at
  the mount root whose `metrics` file renders Prometheus-format counters for
  getattr/read/open activity, backing read-ahead behavior, and (with the
  jemalloc build) allocator stats. Off by default. See
  [Tuning & metrics](guide/tuning.md#metrics).
- **Scan progress indicator.** `scan` and `scan --revalidate` render a live
  progress bar on an interactive terminal and fall back to periodic
  `ingested N/M (P%)` lines when output is redirected. A new `--quiet`/`-q`
  suppresses it.
- **`--skip-on-missing` template flag.** Opt-in (env `MUSEFS_SKIP_ON_MISSING`):
  drops a track from the mount when a top-level template field stays unresolved,
  instead of substituting `--default-fallback`. The motivating case is
  `--template '$!{beets_path}' --skip-on-missing`, hiding tracks beets left
  without a `beets_path` rather than collapsing them into an `Unknown` bucket.
- **`--read-ahead-prefetch` flag.** Opt-in background prefetch threads layered on
  read amplification, default off — benchmarks found amplification alone
  delivers the read-ahead win, so enable this only when profiling a backend where
  a single large read does not self-pipeline.
- **riscv64 release platform.** Prebuilt `riscv64gc-unknown-linux-{gnu,musl}`
  binaries and `linux/riscv64` Docker images now ship with each tagged release.
  Container bases moved to current stable (Debian trixie, Alpine 3.23).
- **`statfs` reply.** The mount now reports a synthetic non-zero capacity with
  ample free space, so `df` no longer shows a 0-byte filesystem and
  capacity-checking importers (Lidarr et al.) no longer balk.
- **Per-extension skip breakdown.** End-of-scan summary breaks the `skipped`
  count down by lowercased extension (e.g. `skipped 42: jpg=20, cue=10, log=8`)
  so a large skip count is diagnosable. Log-only; the counters are unchanged.
- **`musefs vacuum`.** A maintenance command that compacts the SQLite store —
  reclaiming the free pages that prunes, orphan-art GC, and the migration leave
  behind — and reports the space reclaimed. Run it while unmounted. See
  [Maintenance](guide/maintenance.md).

Plus a substantial round of correctness and robustness fixes across the read
fast path (rowid-reuse consistency for art segments), the MP4/QuickTime
metadata walk, ID3 synthesis, and the prune/delete paths — see the
[Changelog](changelog.md#110---2026-06-17) for the full list.

### Upgrading from v1.0.0

**1. Back up your store.** The schema migration below is one-way. While no scan
or external writer is touching the database, copy `musefs.db` (and its `-wal` /
`-shm` sidecars if present). A v1.0.0 binary has no guard against a newer store
and may misread one that has been migrated, so keep the backup if you might roll
back. From v1.1.0 onward a binary instead **refuses** to open a store whose
schema is newer than it understands, with a clear error.

**2. Automatic schema migration (`user_version` 1 → 2).** The first time a
v1.1.0 binary opens the store — for example `musefs scan` — it migrates in a
single transaction. The migration:

- Adds scanner-owned `tracks.fingerprint` and `tracks.content_hash` columns
  (nullable SHA-256 hex, non-unique by design) plus a `fingerprint` index. They
  start `NULL` and are populated on the next scan; external writers do not set
  them.
- Rebuilds the `tags` table so the 256 KiB `value` cap counts bytes rather than
  characters (the v1 `CHECK` was up to ~4× looser for multibyte text). Any row
  that was already over the byte cap is dropped in the rebuild (this only reaches
  genuinely pathological data — a single tag value larger than 256 KiB of bytes,
  which a real library never has, and such rows were already unreadable under the
  byte-counting read guard anyway; in practice no store is affected).

The migration applies automatically the first time a v1.1.0 binary opens the
store, but you should still run `musefs scan --db <store>` once after upgrading:
that is what populates the new `fingerprint` / `content_hash` columns, which the
scanner's content-identity refind logic relies on. Then remount. See
[The SQLite store](architecture/store.md) for the full schema contract.

**3. Behavior changes to check.**

- **`scan` exit code.** `scan`/`scan --revalidate` now exit `2` when any file
  fails to parse or ingest (previously always `0` on a non-fatal run). A clean
  scan still exits `0`; a hard error still exits `1`. Pipelines that key off the
  exit status — e.g. `musefs scan … && musefs mount …` — will now correctly stop
  on a partial-ingest failure; update any script that assumed `0`.
- **`--fallback` keys are case-insensitive.** A per-field `--fallback
  AlbumArtist=…` (or any non-lowercase key) is now matched against the template
  field instead of silently never applying. If you worked around the old bug by
  lowercasing keys, no change is needed; uppercase keys now take effect.
- **`df` on the mount** now shows a synthetic capacity instead of zeros.
- **Extended attributes** (`getxattr`/`setxattr`/…) now return `ENOTSUP`
  explicitly on the read-only mount; the caller-visible result is unchanged, but
  the per-probe `[Not Implemented]` warning is gone.

**4. External writers** (beets, Picard, Lidarr, `python-musefs`) version
independently and need no change for this upgrade: the new `fingerprint` /
`content_hash` columns are scanner-owned and nullable, so the external-writer
contract is unchanged. Update those packages on their own cadence.

## Earlier releases

For v1.0.0 and earlier, see the [Changelog](changelog.md).
