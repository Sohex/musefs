# FAQ

**Does musefs ever write to my audio files?**
No. The mount is read-only and the scanner only reads. The served files are
assembled on the fly: generated metadata plus positioned reads of your
originals. Nothing is ever copied or rewritten.

**Where do my edited tags live?**
In the SQLite store (`--db`). Edit it with the
[beets](../integrations/beets.md) or [Picard](../integrations/picard.md)
plugins, the [Lidarr](../integrations/lidarr.md) integration, or with plain
SQL — the schema is a documented, stable contract
(see [ARCHITECTURE.md](../architecture/store.md#the-sqlite-store)).

**Do edits show up without remounting?**
Yes. The mount polls the database (debounced) and picks up external commits
automatically, with stable inodes across refreshes — even files held open
keep working.

**Can I write through the mount?**
No — and it's not planned. Out-of-band editing against the store *is* the
design: it's what guarantees your originals can never be corrupted.

**Is it fast enough for a big library on a NAS?**
That's the design target: synthesized headers are cached, blocking reads run
on a worker pool so a slow disk never stalls the filesystem, and read-ahead,
cache TTLs, and poll intervals are all [tunable](tuning.md#tuning). In
`structure-only` mode on kernel 6.9+, reads can bypass the daemon entirely
via FUSE passthrough (needs `CAP_SYS_ADMIN`).

**A file in the mount won't open / reads error — why?**
The most common cause is a backing file that changed since its last scan
(musefs refuses to serve a file whose size or mtime drifted, rather than
splice at stale offsets). Run `musefs scan --revalidate` to re-probe it.
