# Architecture overview

This is the technical reference for musefs internals: how a virtual file is
assembled, how the workspace is layered, what the SQLite store guarantees, and
how external edits become visible without a remount. For usage, see the
[README](../../../README.md); for the development workflow, see
[CONTRIBUTING](../contributing/setup.md); for per-format behavior, see the format docs
under `docs/`.

## Design overview

musefs is a read-only passthrough FUSE filesystem with one cardinal invariant:
**original audio bytes are never copied or modified.** A served file is not a
transcoded or rewritten copy — it is assembled on the fly by splicing a
freshly generated metadata region in front of positioned reads of the
untouched backing file. The SQLite store is the source of truth for tags, art,
and each file's audio byte range; the backing directory is the source of truth
for the audio itself.

## Crate layout

A strict layered Cargo workspace; dependencies point one way only:

```
musefs-db   ─┐                 SQLite store: schema/migrations, tracks/tags/art access
musefs-format┘← (db)           format byte-surgery: metadata synthesis + RegionLayout
        ↑
musefs-core ← (db, format)     orchestration: virtual tree, resolution, scanning, refresh
        ↑
musefs-fuse ← (core)           thin FUSE adapter (fuser)
        ↑
musefs-cli  ← (core, fuse, db) clap commands library (scan/mount logic)
musefs      ← (cli)            thin binary entrypoint; published as `musefs`
```

`musefs-core` is the integration layer — cross-cutting logic belongs there.
`musefs-fuse`, `musefs-cli`, and the `musefs` binary crate are deliberately
thin; the FUSE adapter's job is translating kernel requests into core calls
(and dispatching blocking reads onto a worker pool with per-thread reusable
buffers, so a slow backing read never stalls the FUSE dispatch thread).

The workspace also carries `musefs-latencyfs`, a dev/bench-only crate
(`publish = false`): a latency-injecting passthrough FUSE filesystem used by
the [BENCHMARKS.md](../benchmarks.md) harness to simulate slow backing stores. It
is not part of the shipping dependency graph (core uses it only as a
dev-dependency).
