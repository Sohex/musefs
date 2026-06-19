# Maintenance

## Refreshing the store (`musefs revalidate`)

`musefs revalidate` is the maintenance pass over a library you have already
scanned. It re-probes only the tracks whose backing file changed on disk (by
size / mtime / ctime) and refreshes their structural serving data — audio byte
range, content checksums, and FLAC structural blocks — while **preserving the
curated tags, art, and binary tags in the store**. Unchanged files are skipped,
and files not yet in the store are ignored (ingesting new files is `scan`'s
job — see [Scanning](scanning.md)).

```bash
musefs revalidate /path/to/music --db library.db          # refresh changed rows
musefs revalidate /path/to/music --db library.db --prune  # also delete gone tracks
```

By default `revalidate` never deletes anything. Pass `--prune` to delete tracks
whose backing file is gone from disk (scoped to the revalidated root) and
garbage-collect any art left unreferenced. Pruning is opt-in because it removes
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

`VACUUM` needs a write lock on the store and rewrites the whole file. Run it when
nothing else is using the database — no mount, no scan. If the store is in use,
the command fails with an actionable error rather than fighting for the lock:

```text
error: the store is in use — unmount the filesystem or stop any scan before vacuuming
```

### Notes

- **Full rewrite.** Each run rewrites the entire database and transiently needs
  free disk space roughly equal to the store size (it builds a complete copy
  before swapping). Running it again on an already-compact store is safe and
  reports `(already compact)`.
- **May upgrade the schema.** Like every musefs command that opens the store for
  writing, `vacuum` migrates an older store to the current schema version before
  compacting.
