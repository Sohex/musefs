# Maintenance

## Refreshing the store (`musefs revalidate`)

`musefs revalidate` is the maintenance pass over a library you have already
scanned. It re-probes the tracks whose backing file changed on disk (by
size / mtime / ctime / recorded inode) and refreshes their structural serving
data — audio byte range, content checksums, and FLAC structural blocks — while
**preserving the curated tags, art, and binary tags in the store**. It also
re-probes an unchanged file whose row lacks something a probe records: the
checksum the `--checksum` tier asks for, a FLAC file's structural blocks, or an
inode on a filesystem that keeps inode numbers — which, after
[`musefs migrate`](#upgrading-the-store-musefs-migrate), is every row. Other
unchanged files are skipped, and files not yet in the store are ignored
(ingesting new files is `scan`'s job — see [Scanning](scanning.md)).

The one thing about art it does refresh is what a file declares about its own
pictures. A link to an image the file embeds, under the same picture type and
description, takes back the file's MIME type, dimensions, bit depth and colour
count; a link an external writer made to other bytes, or re-described, is left
as it is. That is how a revalidate restores the per-file picture metadata the
schema v4 migration could not carry over
([#746](https://github.com/Sohex/musefs/issues/746)).

```bash
musefs revalidate /path/to/music --db library.db          # refresh changed rows
musefs revalidate /path/to/music --db library.db --prune  # also delete gone or refused tracks
```

By default `revalidate` never deletes anything. Pass `--prune` to delete tracks
whose backing file is gone from disk (scoped to the revalidated root) and
garbage-collect any art left unreferenced. It also deletes a track whose file is
still there but in a form this version refuses to serve — a chained Ogg stored
by 1.3.0, which no scan can refresh and which otherwise fails every revalidate
([#747](https://github.com/Sohex/musefs/issues/747)). Only that refusal counts:
a file that fails to parse, or cannot be read, keeps its row. Without
`--prune`, a revalidate that meets one says so. Pruning is opt-in because it removes
a track's curated metadata along with its row, so a transient mount blip or an
unplugged drive can't silently drop your edits.

The scope is the track's stored path, which is always fully resolved. With
`--follow-symlinks`, a track reached through a symlink is stored under the path
it resolves to, so `revalidate --prune /music --follow-symlinks` never prunes a
track whose file resolved to `/nas/...`, even once that file is gone. Prune such
tracks by running `revalidate --prune` over the directory their files resolve
into.

It shares `scan`'s probe flags — `--jobs N`, `--follow-symlinks`, `--quiet` /
`-q`, and `--checksum` (a row missing a checksum that tier computes is
re-probed to fill it, whether or not its file changed) — and shows the same
live progress indicator. The per-target summary reads
`revalidated <target>: U updated, C unchanged, P pruned, F failed in <duration>`.

### When to run it

Run `revalidate` after the contents of your backing library change on disk —
files re-encoded, retagged at the source, or deleted — to bring the store's
structural data back in sync without disturbing your curated metadata. To pick
up newly-added files run `scan` (which is additive); to drop rows for deleted
files add `--prune`. After moving files, run `scan` first so they retarget, then
`revalidate --prune` — see [Move re-identification](scanning.md#content-checksums-and-move-re-identification).

Run it too after replacing the storage under the backing path — a swapped drive,
or a network or FUSE mount replaced by another — even when the files look the
same. musefs recognises a backing file by its path plus its size, modification
time, change time and inode number, not by the device it is on. Copying a
library onto new storage gives every file a new change time, so every file reads
as changed: opens fail until a revalidate re-probes them. A file that agrees on
all four fields cannot be told apart and is served as the original. On
filesystems with real timestamps that takes a coincidence nothing ordinary
produces, but on FAT and exFAT under Linux, where only the size and a coarse
modification time are compared, a copy that preserved its timestamps can agree — one more
reason [they are not recommended](installation.md) for the backing library.

## Compacting the store (`musefs vacuum`)

The SQLite store only grows as you use it: deleting tracks (beets/Lidarr
prunes), garbage-collecting orphaned art, and the schema migration all leave
free pages behind that are not automatically reclaimed. Because embedded art is
stored inline (up to ~16 MiB per image), a library that has churned art can
carry significant dead space.

`musefs vacuum` compacts the store and reports how much it reclaimed:

```bash
musefs vacuum --db library.db        # or: MUSEFS_DB=library.db musefs vacuum
```

```text
vacuumed library.db: 412.7 MiB → 318.2 MiB (reclaimed 94.5 MiB)
```

It runs SQLite's `VACUUM` followed by a WAL checkpoint, rewriting the database
into a compact form.

### Run it while unmounted

`VACUUM` rewrites the whole file, so it takes the store for itself first, the
same way [`musefs migrate`](#upgrading-the-store-musefs-migrate) does. Anything
else with the store open — a mount, including one sitting idle between reads, or
a scan — makes it refuse before anything is rewritten:

```text
musefs: the store is in use — unmount the filesystem or stop any scan before vacuuming: <SQLite error>
```

Once it has the store, nothing else can attach until it finishes. One case it
cannot see is a process that has opened the store and not yet read from it:
SQLite only registers a connection on its first statement, so that process is
kept out from the moment it tries rather than detected in advance.

### Notes

- **Full rewrite.** Each run rewrites the entire database and transiently needs
  free disk space of about the store's size twice over: SQLite builds the
  compacted copy in its temporary directory (the first of `SQLITE_TMPDIR`,
  `TMPDIR`, `/var/tmp`, `/usr/tmp` and `/tmp` it can write to), then writes it
  back through the write-ahead log beside the store. Running it again on an already-compact store is safe and
  reports `(already compact)`.
- **May upgrade the schema.** Like every musefs command that opens the store,
  `vacuum` applies any pending *transparent* migration before compacting. A
  store needing a **gated** one is refused instead, naming
  [`musefs migrate`](#upgrading-the-store-musefs-migrate).

## Upgrading the store (`musefs migrate`)

**You need this only when crossing a major version.** Upgrading 2.0 to 2.1, or
2.1.3 to 2.1.4, never asks for it; going from 1.x to 2.x may. That is a promise,
not a habit: a migration invasive enough to be gated is only ever introduced by
a major release, and musefs will not build if one is added anywhere else.

Most schema changes are applied the moment any musefs command opens the store,
and you never hear about them. Some are not: a change that rewrites data,
transiently needs the store's size again in free disk, or ends compatibility
with older musefs builds is more than anyone running `mount` can reasonably
expect. Those are **gated**, and a major release is the only place they appear.
Every command that opens a store for ordinary work refuses one that needs it:

```text
musefs: opening database at library.db: store schema version 2 needs an explicit
upgrade to version 4 before this musefs build can open it; run `musefs migrate
--db <store>`. The upgrade rewrites the store in place and older musefs builds
will no longer open it, which is why it is not applied automatically
```

A command that refuses leaves the store's data and schema version exactly as it
found them, and the previous release can still open it. (Closing the store can
still fold pending write-ahead-log pages into the database file, so its bytes
may differ.) While a gated
step is pending no step is applied, not even an automatic one, so the previous
release still opens the store until `musefs migrate` has run
([#749](https://github.com/Sohex/musefs/issues/749)).

`musefs migrate` is where that upgrade happens, deliberately:

```bash
musefs migrate --db library.db
```

It reports what it is about to do, checks that every row survives the new
schema, takes a snapshot, upgrades the store, and then offers to clean up after
itself:

```text
store library.db is at schema version 2; this build needs 4.
  v3 (musefs 2.0.0) — widens the tags.value and track_art.description caps
  v4 (musefs 2.0.0) — clears every stored fingerprint and content hash; a revalidate recomputes them  [needs this command]
This rewrites the store in place. Once it is done, musefs builds older than this one will no longer open it.
store is 412.7 MiB; the upgrade needs, on each filesystem it writes to:
  /srv/musefs (the store, the snapshot): about 1.21 GiB free, has 27.7 GiB
  /var/tmp (SQLite's temporary files): about 412.7 MiB free, has 9.3 GiB
a snapshot will be written to library.db.v2.bak first.
Upgrade library.db now? [y/N]
```

### Run it while unmounted

The upgrade takes the store for itself and refuses to start if anything else
has it open — a mount, a running scan, another `musefs migrate`:

```text
musefs: the store is in use — unmount the filesystem or stop any scan before migrating: <SQLite error>
```

### Rows the new schema refuses

Before anything is copied or written, `migrate` checks whether every row in the
store is valid under the schema it is about to become. A row can fail that for
one of two reasons: it was written before the constraint that now refuses it, or
it was written by a tool with the constraints turned off.

If any are found, the command reports them per table and stops:

```text
2 rows in this store are not valid under the new schema:
  tags: 1 row(s)
  art: 1 row(s)
They were written before the constraint that now refuses them, or by a writer with the constraints turned off.
musefs: refusing to upgrade library.db: 2 row(s) would be rejected. Pass --repair to delete them, or fix them yourself first. The upgrade changes nothing until this is resolved
```

Nothing has happened at this point — no snapshot, no rewrite. Either fix the
rows with whatever wrote them, or pass `--repair` to have `migrate` **delete**
them. It will not do that on its own: dropping a row an external tool chose to
write is exactly the kind of thing that should not happen without being asked.

`--repair` deletes after the snapshot is taken, so the rows are still in the copy
you can go back to — which is why it refuses to run alongside `--no-snapshot`.
The deletes are part of the upgrade itself, in the same transaction: if the
upgrade then fails, on a full disk for instance, they are undone with it and the
store is left exactly as it was. The count is what will actually go: a child
whose parent does not survive is reported with it, rather than left to the
cascade to take silently.

**Art filed under a digest that is not lowercase hex.** From 2.0.0 an `art` row's
`sha256` must be 64 lowercase hexadecimal characters
([#761](https://github.com/Sohex/musefs/issues/761)). musefs and the `contrib`
plugins have always written that form, so a row that fails this was written by
another tool. `--repair` **deletes such a row**. Where a correctly filed row
already holds the same bytes, each picture link to it moves onto that row, so
the track keeps the picture; `migrate` reports those links as relinked rather
than refused. Where none does, the links are deleted with the row and each track
that used it loses that picture, which then survives only in the snapshot.
`migrate` does not lowercase the digest for you, because a correctly filed row
for the same image may already exist, and two rows cannot share one.

Fixing the row by hand first is possible but not a one-statement edit. Art rows
cannot be changed once written, before 2.0.0 as well, so you insert a correctly
filed row for the image — or find the one that already holds those bytes —
point each `track_art` row at it, and then delete the old row. A digest that is
not hex at all has to be recomputed from the image outside SQLite, which has no
SHA-256 function. And because the store you are fixing still keeps the MIME type
and dimensions on the `art` row, a link you move onto an existing row takes that
row's values; a `revalidate` restores them for pictures the file embeds itself.

**One path stored twice.** Before 2.0.0 a track's path was text, and a tool that
bound it as bytes could add a second row for a file musefs already had: SQLite
never compares the two spellings equal, so nothing refused it. 2.0.0 stores
every path as bytes, which makes the two one path, and only one row can keep it.
`migrate` reports each such pair with both track ids. `--repair` keeps the row
that carries tags or picture links and deletes the other; when neither carries
any, nothing is lost either way, and it keeps the older one. When both do, it
cannot know which you want, so it refuses and deletes nothing: delete the row
you do not want yourself (deleting a track takes its tags and links with it),
then run `migrate` again.

The check also catches a row that is fine in itself but points at a parent that
is not there — the kind an external tool can leave behind with foreign keys
turned off. Such a row passes every constraint in its own table and fails only
when the upgrade puts it back.

The check reports what the migration would actually do, because it asks the
migration's own schema rather than a second description of it: the target
tables are built exactly as the upgrade builds them, and every row is offered to
them. That is one pass over the store. A second pass, over what the first
accepted, finds the rows whose parent did not survive it, which is how a child
of a refused track is reported with it.

### The snapshot

Before touching anything, `migrate` writes a compacted, consistent copy of the
store to `<db>.v<version>.bak` using SQLite's `VACUUM INTO`. It takes about as
long as a vacuum — under two seconds on a reference-shaped library — and it is
what makes an otherwise one-way upgrade reversible: if anything goes wrong,
that file is your store exactly as it was. Put it somewhere else with
`--snapshot PATH`, or skip it with `--no-snapshot`. `migrate` refuses to
overwrite an existing snapshot.

The copy is written under a temporary name beside the snapshot's —
`library.db.v2.bak.partial-<numbers>` — synced to disk, and only then renamed
into place, so a file under the snapshot's own name is always complete. A run
that is killed or crashes while copying leaves the temporary file instead. It
can never be a usable snapshot, so the next `migrate` removes it, says so, and
takes the snapshot again.

### Disk space

Before asking anything, `migrate` works out the free space the upgrade needs on
each filesystem it writes to, and refuses up front if one is short, rather than
failing part-way through. Filesystems are told apart by device, so a
`--snapshot` in another directory on the store's disk counts against the same
space as the store.

The run writes in phases, and each holds copies of the store at its peak. In
multiples of the store's size on disk (with its `-wal` and `-shm`):

| Phase | Beside the store | The snapshot's filesystem | SQLite's temporary directory |
| ----- | ---------------- | ------------------------- | ---------------------------- |
| Checking the rows | — | — | 1×, deleted afterwards |
| The snapshot | — | 1×, kept | — |
| The upgrade | 2× | 1× | — |
| A vacuum afterwards | 2× | 1× | 1× |

Each filesystem needs its largest phase, adding up whatever that phase puts on
it. With everything on one filesystem that is three times the store, or four
with a vacuum; `--no-snapshot` takes one copy away and `--vacuum=false` another.
A vacuum offer you have not answered counts when `migrate` runs on a terminal,
since you may accept it, and not otherwise. The upgrade's two copies are the
store growing by a copy of its tables and the rollback journal holding the
original of every page it overwrites; if the run is interrupted, the next open
of the store rolls the journal back, leaving the store as it was.

SQLite's temporary directory is the first of `SQLITE_TMPDIR`, `TMPDIR`,
`/var/tmp`, `/usr/tmp` and `/tmp` that it can write to. Where that is a
RAM-backed `/tmp`, checking the rows and vacuuming each hold a copy of the store
in memory; point `SQLITE_TMPDIR` at a disk with room to avoid that. The space
for checking the rows is checked on its own, before the check runs.

### Afterwards

An upgrade that rewrites rows leaves the store larger than it was, so `migrate`
offers a vacuum. It also reports how many tracks lost a scanner-derived value
the upgrade retired — the fingerprint, in the 2.0.0 upgrade — and offers to run
a [`revalidate`](#refreshing-the-store-musefs-revalidate) over the directory
your library shares. A plain `scan` does not recompute a stored file's
fingerprint; the revalidate does. Until it runs, those tracks cannot be
re-identified after a move.

The 2.0.0 upgrade leaves more than fingerprints for that revalidate: it records
each file's inode, except on FAT and exFAT under Linux, which keep none, and
restores each file's own picture metadata. The offer
never prunes. If the revalidate counts any file as failed, `migrate` exits `2`
once it is done, as `revalidate` itself would, even though the store is
upgraded ([#750](https://github.com/Sohex/musefs/issues/750)). The
[release notes](../release-notes.md#upgrading-from-v130) list what else the
first revalidate changes, including every synthesized file's modification time.

The offer covers the deepest directory every stored track shares. Stored paths
are already resolved, symlinks included, so walking that directory reaches every
track without following any symlink. The one library it cannot cover is one
whose tracks resolve into unrelated trees, such as symlinks into two separate
disks: they share no directory below `/`, so `migrate` prints the command
instead of offering it, and you run `musefs revalidate` over each tree.

The report is printed once, but the gap it describes lasts. So until every
track has been re-probed, `mount`, `scan` and `revalidate` each print a warning
with the number still waiting
([#705](https://github.com/Sohex/musefs/issues/705)). A track counts while it
has neither a fingerprint nor a recorded inode, which is how the upgrade leaves
every row. A `--checksum none` scan on FAT or exFAT under Linux records
neither, so its
tracks count too until a revalidate at the default `--checksum` tier
fingerprints them. A file the revalidate cannot re-probe, such as a chained Ogg
([#747](https://github.com/Sohex/musefs/issues/747)), stays counted until
`revalidate --prune` removes it.

### Flags, for scripts

There is no terminal in a pipeline, so `migrate` never blocks waiting on one.
The confirmation has to come from `--yes`, or the command refuses and says so.
The two offers decline themselves unless you ask for them:

| Flag | Effect |
| ---- | ------ |
| `--yes` / `-y` | Upgrade without asking. Required off a terminal. |
| `--repair` | Delete rows the new schema refuses. Nothing is deleted without it; refuses alongside `--no-snapshot`. |
| `--snapshot PATH` | Write the snapshot here instead of beside the store. |
| `--no-snapshot` | Take no snapshot. The upgrade is then not reversible. |
| `--vacuum` / `--vacuum=false` | Compact afterwards, or do not. Omit to be asked. |
| `--revalidate` / `--revalidate=false` | Revalidate afterwards, or do not. Omit to be asked. A revalidate that counts failures makes `migrate` exit `2`; one that cannot run at all, because the tracks share no directory below `/`, fails the command once the store is upgraded. |
| `--jobs N` | Probe worker threads for that revalidate. |

`--db`, `--yes` and `--jobs` also read `MUSEFS_DB`, `MUSEFS_YES` and
`MUSEFS_JOBS`, so a `MUSEFS_YES` set in an environment file, such as a systemd
`EnvironmentFile`, confirms the upgrade without asking. `--snapshot`,
`--repair`, `--no-snapshot`, `--vacuum` and `--revalidate` have no environment
form.

Running it against a store that is already current reports so and changes
nothing, so it is safe to put in a provisioning script ahead of `mount` — and
because only a major release can ever need it, that script will sit there doing
nothing for every upgrade in between.
