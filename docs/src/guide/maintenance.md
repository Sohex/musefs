# Maintenance

## Refreshing the store (`musefs revalidate`)

`musefs revalidate` is the maintenance pass over a library you have already
scanned. It re-probes only the tracks whose backing file changed on disk (by
size / mtime / ctime) and refreshes their structural serving data — audio byte
range, content checksums, and FLAC structural blocks — while **preserving the
curated tags, art, and binary tags in the store**. Unchanged files are skipped,
and files not yet in the store are ignored (ingesting new files is `scan`'s
job — see [Scanning](scanning.md)).

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

It shares `scan`'s probe flags — `--jobs N`, `--follow-symlinks`, `--quiet` /
`-q`, and `--checksum` (which also backfills missing checksums on a changed
row) — and shows the same live progress indicator. The per-target summary reads
`revalidated N: U updated, C unchanged, P pruned, F failed`.

### When to run it

Run `revalidate` after the contents of your backing library change on disk —
files re-encoded, retagged at the source, or deleted — to bring the store's
structural data back in sync without disturbing your curated metadata. To pick
up newly-added files run `scan` (which is additive); to drop rows for deleted
files add `--prune`. After moving files, run `scan` first so they retarget, then
`revalidate --prune` — see [Move re-identification](scanning.md#content-checksums-and-move-re-identification).

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
error: the store is in use — unmount the filesystem or stop any scan before vacuuming
```

Once it has the store, nothing else can attach until it finishes. One case it
cannot see is a process that has opened the store and not yet read from it:
SQLite only registers a connection on its first statement, so that process is
kept out from the moment it tries rather than detected in advance.

### Notes

- **Full rewrite.** Each run rewrites the entire database and transiently needs
  free disk space roughly equal to the store size (it builds a complete copy
  before swapping). Running it again on an already-compact store is safe and
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
error: store schema version 2 needs an explicit upgrade to version 4 before this
musefs build can open it; run `musefs migrate --db <store>`.
```

A command that refuses leaves the store exactly as it found it. While a gated
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
  v4 (musefs 2.0.0) — clears every stored fingerprint; a revalidate recomputes them  [needs this command]
This rewrites the store in place. Once it is done, musefs builds older than this one will no longer open it.
store is 412.7 MiB; the upgrade needs about 825.4 MiB free and has 27.7 GiB.
a snapshot will be written to library.db.v2.bak first.
Upgrade library.db now? [y/N]
```

### Run it while unmounted

The upgrade takes the store for itself and refuses to start if anything else
has it open — a mount, a running scan, another `musefs migrate`:

```text
error: the store is in use — unmount the filesystem or stop any scan before migrating
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
error: refusing to upgrade library.db: 2 row(s) would be rejected. Pass --repair to delete them, or fix them yourself first. The upgrade changes nothing until this is resolved
```

Nothing has happened at this point — no snapshot, no rewrite. Either fix the
rows with whatever wrote them, or pass `--repair` to have `migrate` **delete**
them. It will not do that on its own: dropping a row an external tool chose to
write is exactly the kind of thing that should not happen without being asked.

`--repair` deletes after the snapshot is taken, so the rows are still in the copy
you can go back to — which is why it refuses to run alongside `--no-snapshot`.
The count is what will actually go: a child whose parent does not survive is
reported with it, rather than left to the cascade to take silently.

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

The free-space figure it reports accounts for the snapshot and for SQLite
staging the rewritten pages before committing them. If the filesystem is short,
the command refuses up front rather than failing part-way through.

### Afterwards

An upgrade that rewrites rows leaves the store larger than it was, so `migrate`
offers a vacuum. It also reports how many tracks lost a scanner-derived value
the upgrade retired — the fingerprint, in the 2.0.0 upgrade — and offers to run
a [`revalidate`](#refreshing-the-store-musefs-revalidate) over the directory
your library shares. A plain `scan` does not recompute a stored file's
fingerprint; the revalidate does. Until it runs, those tracks cannot be
re-identified after a move.

The 2.0.0 upgrade leaves more than fingerprints for that revalidate: it records
each file's inode, and restores each file's own picture metadata. The offer
never prunes. If the revalidate counts any file as failed, `migrate` exits `2`
once it is done, as `revalidate` itself would, even though the store is
upgraded ([#750](https://github.com/Sohex/musefs/issues/750)). The
[release notes](../release-notes.md#upgrading-from-v130) list what else the
first revalidate changes, including every synthesized file's modification time.

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
| `--revalidate` / `--revalidate=false` | Revalidate afterwards, or do not. Omit to be asked. A revalidate that counts failures makes `migrate` exit `2`. |
| `--jobs N` | Probe worker threads for that revalidate. |

Running it against a store that is already current reports so and changes
nothing, so it is safe to put in a provisioning script ahead of `mount` — and
because only a major release can ever need it, that script will sit there doing
nothing for every upgrade in between.
