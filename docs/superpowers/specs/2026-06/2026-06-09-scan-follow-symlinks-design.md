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

## The root argument is always followed

The flag governs symlinks **encountered during recursion**, not the `root`
argument itself. Both `scan_directory_with` and `revalidate_with` dispatch on
`root.is_file()` (which follows the link) before calling `collect_audio`, and
`collect_audio`'s `read_dir(root)` likewise follows a symlinked root directory.
So a symlinked-file root is already scanned and a symlinked-directory root is
already walked **today, regardless of the flag** — and that does not change. Only
links found *inside* the tree are gated. When following, the cycle guard is
seeded with the followed root's `(dev, ino)`.

## Code shape

### `collect_audio` signature and the cycle-guard ownership

`collect_audio` is recursive and has **three production callers**
(`find_referencing_symbols`): `scan_directory_with` (`scan.rs:596`),
`revalidate_with` (`scan.rs:802`), and `scan_directory_full_oracle`
(`scan.rs:771`, a `#[doc(hidden)]` test oracle) — plus one unit-test caller,
`hardening_tests::collect_audio_skips_unsupported_files` (`scan.rs:1229`).

To keep the public signature minimal and the visited-set bookkeeping
encapsulated, split into two functions:

- `collect_audio(root, out, follow_symlinks: bool)` — the entry point all callers
  use. It creates the `HashSet<(u64, u64)>`, seeds it with `root`'s `(dev, ino)`
  when `follow_symlinks` is true, then delegates to the recursive inner function.
- a private recursive `collect_audio_inner(root, out, follow_symlinks, visited)`
  carrying the set.

This means each caller's call site changes only by passing one `bool`, not two
new arguments.

### Per-caller changes (all must be updated — the blocker)

- `scan_directory_with` (`scan.rs:596`): pass `opts.follow_symlinks`.
- `revalidate_with` (`scan.rs:802`): pass `opts.follow_symlinks`. **This is
  required, not optional.** `revalidate_with` walks the same tree via
  `collect_audio` and then prunes tracks under the root whose backing file is
  gone. If scan follows symlinks but revalidate does not, symlinked tracks are
  scanned once and then never re-probed on subsequent revalidations — a silent
  scan/revalidate divergence. It already takes `&ScanOptions`, so threading the
  flag through is the whole fix.
- `scan_directory_full_oracle` (`scan.rs:771`): pass `false`. It is the legacy
  test oracle, takes no `ScanOptions`, and does not need symlink support; it only
  needs to satisfy the new signature.
- `hardening_tests::collect_audio_skips_unsupported_files` (`scan.rs:1229`):
  update the existing call to pass `false`.

### `ScanOptions` and the symlink arm

- `ScanOptions` (`scan.rs:378`) gains `follow_symlinks: bool`; its `Default` impl
  (`scan.rs:387`) sets it `false`. Grep for `ScanOptions {` literals before
  building — the CLI uses `..Default::default()` so it is unaffected, but any full
  literal construction in tests must add the field.
- A `ftype.is_symlink()` arm is added to `collect_audio_inner`. The existing
  `is_dir()` / `is_file()` arms behave exactly as today when the flag is off; the
  directory descent consults the cycle guard when the flag is on.

### CLI plumbing (`musefs-cli/src/lib.rs`)

- The `Command::Scan` variant (`lib.rs:93`) gains `follow_symlinks: bool` with
  `#[arg(long)]` (default false), mirroring the existing `jobs` / `quiet` flags.
- `run_scan` (`lib.rs:122`) gains a `follow_symlinks: bool` parameter and builds
  `ScanOptions { jobs, follow_symlinks, ..Default::default() }` (`lib.rs:131`).
  Because `opts` is shared by both the scan and the revalidate branches, the
  revalidate path honors the flag with no further change.
- `run` (`lib.rs:201`) threads the parsed field into the `run_scan` call.
- The `scanned … skipped … failed` summary line is unchanged.

## Testing

Added to the existing `hardening_tests` module in `scan.rs`, creating links with
`std::os::unix::fs::symlink`. The project has **no log-capture harness** (`log`
is the only logging dep, no `testing_logger`/`logtest`), so tests assert on the
**observable collection result** (the contents of the `out` vec, or `ScanStats`
after a full scan) — not on log emission. The `log::warn!` lines are a diagnostic
side effect, verified by inspection, not by assertion. Tests drive
`collect_audio(root, &mut out, follow_symlinks)` directly where possible.

- **symlinked audio file** — present in `out` when the flag is on; **absent** from
  `out` when off (the observable form of "logged and skipped").
- **symlinked directory** — files beneath it appear in `out` when on; absent when
  off.
- **cycle** — a directory symlink pointing to an ancestor: the call **returns**
  (terminates) rather than recursing infinitely, and each real file appears in
  `out` at most once. (A test that hangs on failure is acceptable here — the
  assertion is "it completes.")
- **broken symlink, flag on** — `collect_audio` returns `Ok(())` (does not abort
  via `?`) and the valid sibling files are still collected.
- **default-off regression** — real files are still collected and symlinks are
  not followed (guards against a behavior change on the default path).

## Docs

- `ARCHITECTURE.md` scanning section: document the symlink behavior and the
  `--follow-symlinks` flag (currently undocumented; the only symlink mention,
  `ARCHITECTURE.md:153`, is about the external destination tree).
- `README.md`: add `--follow-symlinks` to the `scan` flags.
