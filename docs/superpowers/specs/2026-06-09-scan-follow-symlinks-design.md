# Scan: follow symlinks (opt-in, cycle-guarded)

**Issue:** [#189](https://github.com/Sohex/musefs/issues/189) — Scan silently
skips symlinked files and directories with no log or skipped count.

## Problem

`collect_audio` (`musefs-core/src/scan.rs:76`) classifies directory entries with
`entry.file_type()`, which does **not** follow symlinks. A symlink dirent is
therefore neither `is_dir()` nor `is_file()`, so it falls through both arms:

```rust
let ftype = entry.file_type()?;
if ftype.is_dir() {
    collect_audio(&path, out)?;
} else if ftype.is_file() && is_supported_audio(&path) {
    out.push(path);
}
```

Symlinked audio files and symlinked subdirectories are never recursed into, never
added to the candidate set, and never counted in `ScanStats::skipped` (the skip
tally happens later in the probe phase, which these entries never reach). There
is no log line. For libraries that rely on symlinks — the Lidarr integration
creates symlink destinations by default (`CONTRIBUTING.md:300`), and symlinked
libraries are common on seedbox/NAS setups — this produces a "half my library is
missing" outcome with no diagnostic.

The current "symlinks are not followed" behavior is undocumented. It also makes
the scan incidentally immune to directory-symlink cycles: because nothing is
followed, no cycle guard is needed today.

## Goals

- Symlinked audio files and directories can be scanned.
- The current silent skip is replaced by a diagnostic, even when symlinks are
  not followed.
- Directory-symlink cycles cannot cause infinite recursion once following is
  enabled.
- The default behavior stays cycle-immune and free of new overhead on the
  non-symlink path.

## Non-goals

- Changing the cardinal read-only-passthrough invariant. Following a symlink only
  changes *which* backing file is read; `run_pipeline` already canonicalizes
  every collected path before storing it, so a followed symlink is stored by its
  real target path exactly like any other file.
- Deduplicating individual files reachable via more than one symlink path. The
  directory cycle guard prevents re-walking the same directory; a file reachable
  through two distinct symlinked directories would be probed twice, but
  `ingest_bulk` upserts by canonical path, so the second write is idempotent.
  This edge case is accepted, not engineered against.
- Changing `ScanStats` semantics or the CLI summary line.

## Behavior

A new `--follow-symlinks` flag on the `scan` subcommand controls the behavior.
It is **off by default**.

### Flag off (default)

A symlink dirent is no longer silently dropped. `collect_audio` emits a
`log::warn!` per skipped symlink, naming the path and hinting that
`--follow-symlinks` will scan it. `ScanStats` is untouched — `scanned`,
`skipped`, and `failed` keep their current meanings (`skipped` remains
"unsupported-format files probed"). Nothing is followed, so cycle-immunity is
preserved.

### Flag on

Symlinks are resolved (stat following the link, via `std::fs::metadata`):

- a symlink whose target is a supported audio file is collected;
- a symlink whose target is a directory is recursed into, subject to the cycle
  guard below;
- a target that is neither file nor directory (socket, fifo, …) is ignored;
- a **broken/dangling** symlink (target stat fails) is logged with `log::warn!`
  and skipped. Collection never aborts on a single bad link — the error is **not**
  propagated via `?`, and `ScanStats` is not touched (the collection phase has no
  counters today).

## Cycle guard

When following is on, the collection walk maintains a
`HashSet<(u64, u64)>` of visited `(st_dev, st_ino)` pairs, obtained via
`std::os::unix::fs::MetadataExt` (musefs is Unix-only — it is a FUSE
filesystem). The guard:

- records **every directory the walk descends into — real or symlinked — not
  just symlinked ones**, and
- before descending into a directory, skips it (logged as a cycle) if its
  `(dev, ino)` is already in the set.

Recording only symlinked directories is **insufficient**: a symlink pointing back
to a real *ancestor* would loop forever through ordinary directory recursion (the
walk re-enters the symlink on each pass). Recording every entered directory
breaks both direct symlink loops and ancestor loops.

The set lives only for the duration of one `collect_audio` walk and is only
populated when the flag is on, so the default path gains no per-directory `stat`
overhead beyond the new symlink branch.

## Code shape

- `ScanOptions` (`scan.rs:378`) gains `follow_symlinks: bool`; its `Default` impl
  sets it `false`.
- `collect_audio` (`scan.rs:76`) gains the `follow` flag and a
  `&mut HashSet<(u64, u64)>` visited set, threaded from `scan_directory_with`
  (`scan.rs:596`). `scan_directory_with` seeds the set with the root directory's
  `(dev, ino)` before the first call (when following).
- A `ftype.is_symlink()` arm is added to `collect_audio`. The existing
  `is_dir()` / `is_file()` arms behave exactly as today when the flag is off; the
  directory arm consults the cycle guard when the flag is on.
- CLI (`musefs-cli/src/lib.rs`): parse `--follow-symlinks` into
  `opts.follow_symlinks`. The `scanned … skipped … failed` summary line is
  unchanged.

## Testing

Added to the existing `hardening_tests` module in `scan.rs`, creating links with
`std::os::unix::fs::symlink`:

- **symlinked audio file** — collected when the flag is on; logged and skipped
  (not collected) when off.
- **symlinked directory** — its contents are recursed into and scanned when on.
- **cycle** — a directory symlink pointing to an ancestor terminates rather than
  recursing infinitely.
- **broken symlink, flag on** — logged, skipped, and the scan completes
  successfully (does not abort).
- **default-off regression** — real files are still scanned and symlinks are not
  followed.

## Docs

- `ARCHITECTURE.md` scanning section: document the symlink behavior and the
  `--follow-symlinks` flag (currently undocumented; the only symlink mention,
  `ARCHITECTURE.md:153`, is about the external destination tree).
- `README.md`: add `--follow-symlinks` to the `scan` flags.
