# Maintenance

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
