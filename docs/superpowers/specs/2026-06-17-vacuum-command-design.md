# Design: `musefs vacuum` command

Date: 2026-06-17
Status: approved (brainstorming)
Target release: v1.1.0 (folded into the open release-prep PR #566)

## Problem

The SQLite store does no compaction today — there is no `VACUUM`, no
`auto_vacuum`, and no checkpoint-truncate anywhere in `musefs-db`,
`musefs-core`, or `musefs-cli`. The store therefore only grows: every deletion
path leaves free pages that are never reclaimed. The deletion paths are real and
several:

- beets/Lidarr prunes of moved-away or deleted tracks,
- `gc_orphan_art` reclaiming unreferenced art rows,
- the v1.1.0 `MIGRATION_V2` upgrade dropping any over-cap tag rows.

Because art blobs are stored inline (up to ~16 MiB each), a library that has
churned art can carry substantial dead space with no way to recover it. A
`musefs vacuum` maintenance command gives operators an explicit way to compact
the store.

## Goals

- Add a `musefs vacuum --db <store>` subcommand that compacts the SQLite store
  and reports how much space was reclaimed.
- Keep the SQLite work in the `musefs-db` layer; keep the CLI thin.

## Non-goals (YAGNI)

- No `ANALYZE`, no `integrity_check`, no orphan-art GC or prune folded in — the
  command is a vacuum, not a general "maintenance" umbrella. (GC/prune already
  live in `scan --revalidate`.)
- No `auto_vacuum` mode change (it must be set at store-creation time and would
  require a full vacuum to switch anyway).
- No new locking machinery to detect a live mount, and no `-q`/`--quiet` flag.
- Only one argument: `--db` (env `MUSEFS_DB`).

## Decisions (from brainstorming)

1. **Scope:** `VACUUM` + `PRAGMA wal_checkpoint(TRUNCATE)`, reporting bytes
   reclaimed (before → after). Minimal and true to the name.
2. **In-use handling:** best-effort. Rely on SQLite's existing 5 s
   `busy_timeout`; on a busy/locked error, fail with an actionable message
   telling the user to unmount/stop scanning. Documented as an offline
   maintenance command. No active pre-flight lock guard.
3. **Structure:** Approach A — a `Db::<ReadWrite>::vacuum()` method in
   `musefs-db`; the CLI wires it, stats the file, and prints the summary.
4. **Docs:** a dedicated `docs/src/guide/maintenance.md` page (not folded into
   `scanning.md`).

## Design

### DB layer (`musefs-db`)

New method on `impl Db<ReadWrite>`, in a new `musefs-db/src/maintenance.rs`
module (declared with `mod maintenance;` in `lib.rs`, matching the
one-file-per-concern split of `art.rs`/`tags.rs`/`tracks.rs`/`bulk.rs`/
`structural.rs`; the `map_vacuum_err` helper and any future
`incremental_vacuum` live here too):

```rust
pub fn vacuum(&self) -> Result<()>
```

Behavior:

- Run `self.conn.execute_batch("VACUUM")`, then
  `self.conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")`. Order matters: in
  WAL mode the VACUUM rewrite lands in the `-wal`, so the TRUNCATE checkpoint
  *after* VACUUM is what actually shrinks the main `.db` file on disk and zeroes
  the `-wal`.
- BUSY mapping: error translation is factored into a small free function
  `fn map_vacuum_err(err: rusqlite::Error) -> DbError`, so the branch is
  unit-testable from constructed errors with no real lock contention. It maps a
  `rusqlite::Error::SqliteFailure` whose code is `DatabaseBusy` or
  `DatabaseLocked` to a new typed variant `DbError::StoreInUse`, and defers
  every other error to the existing transparent `#[from]` rusqlite variant.
  `vacuum()` applies it via `.map_err(map_vacuum_err)` on each statement. The
  new variant is added to the `DbError` enum in `musefs-db/src/error.rs`:

  ```
  DbError::StoreInUse  // #[error("the store is in use — unmount the filesystem
                       //          or stop any scan before vacuuming")]
  ```

  carrying the underlying `rusqlite::Error` as its `#[source]`.

The method uses `self.conn` directly (the existing `Db` field that every other
`Db<ReadWrite>` write method already uses); `with_raw_conn` is fuzzing-gated and
is not used here.

### CLI layer (`musefs-cli`)

- New `Command` variant:

  ```rust
  /// Compact the SQLite store, reclaiming free pages left by deletions
  /// (prunes, orphan-art GC, the schema migration). Run while unmounted.
  Vacuum {
      #[arg(long, env = "MUSEFS_DB")]
      db: PathBuf,
  },
  ```

- New `run_vacuum(db: &Path) -> anyhow::Result<()>`:
  1. `if !db.exists()` → `bail!` with a not-found message (mirrors the
     `mount --db` existing-store guard); vacuuming a nonexistent store is
     meaningless.
  2. Measure the store's on-disk footprint (`before`) — the sum of the `.db`,
     `-wal`, and `-shm` file sizes (a missing sidecar counts as 0). Summing the
     WAL/SHM makes the figure honest: a TRUNCATE checkpoint reclaims WAL bytes
     too, and the main `.db` alone can understate real usage when a large WAL is
     pending.
  3. `Db::open(db)` (this runs `migrate()` + `validate_identity()` as every open
     does — see note below).
  4. `store.vacuum()?` — `DbError::StoreInUse` surfaces its own actionable
     message through anyhow.
  5. Re-measure the footprint (`after`) — same three-file sum, now post-vacuum
     and post-TRUNCATE (so `-wal` is ~0).
  6. Print one stdout line via `indicatif::HumanBytes` (already a dependency;
     binary KiB/MiB units, matching the CLI's existing `HumanDuration` usage and
     the codebase's MiB/KiB vocabulary):
     `vacuumed <path>: <before> → <after> (reclaimed <delta>)`. Compute `delta`
     with saturating subtraction; if `after >= before`, print `(already
     compact)` instead of a zero/negative delta. Using `HumanBytes` removes the
     need for a hand-rolled formatter and its boundary tests.
- Dispatch: `Command::Vacuum { db } => run_vacuum(&db).map(|()| ExitCode::SUCCESS)`.
  Success exits `0`; any error exits `1` via the existing anyhow→main path.
  Vacuum has no "partial" outcome (it either compacts or errors), so `0`/`1` is
  the complete set — unlike `scan`, which adds exit `2` for partial ingest.

**Open-time behavior & inherited failure modes.** `run_vacuum` opens through
`Db::open`, so it inherits all of `configure()`'s behavior
(`musefs-db/src/lib.rs`):

- It always migrates, so vacuuming a v1.0.0-schema store also upgrades it to
  schema v2 — which itself can drop over-cap tag rows. Benign and consistent
  with every other write entry point, but it means `vacuum` is not a pure
  read-shrink. The command help and the maintenance page state explicitly that
  vacuum may upgrade the schema.
- It runs `validate_identity()`, so pointing `--db` at a non-musefs or tampered
  file fails at *open* with `DbError::SchemaMismatch` (not a vacuum-specific
  message), and a store written by a newer build fails with
  `DbError::StoreTooNew`. Both surface acceptably through anyhow; the plan
  should not special-case them.

### Docs & changelog

- New `docs/src/guide/maintenance.md`: a short "Maintenance" page documenting
  `musefs vacuum` — what it reclaims, the "run while unmounted" caveat, the
  reclaimed-bytes output, the migrate-may-upgrade-schema note, and a warning
  that VACUUM does a full rewrite each run and transiently needs free disk space
  roughly equal to the store size (it builds a complete copy before swapping).
  Add it to `docs/src/SUMMARY.md` under **User Guide**.
- README: no change. Its CLI block is a minimal scan→mount getting-started
  example, not a command reference, so `vacuum` is documented in the maintenance
  guide and changelog rather than added there.
- Changelog: add a `### Added` bullet to the already-promoted **[1.1.0]**
  section of both `CHANGELOG.md` (root, curated) and `docs/src/changelog.md`
  (full), and add a bullet to the v1.1.0 highlights in
  `docs/src/release-notes.md`.

## Test plan

Tests are written TDD-first and every commit lands green (the pre-commit hook
runs the full workspace suite).

- **`musefs-db`** (temp-file store — file-size shrink needs a real on-disk file,
  not in-memory):
  - *Shrink, robustly.* Insert enough rows to span many pages — several art
    blobs (art bytes are large) guarantees multi-page allocation — then delete
    them and checkpoint. Capture a settled pre-vacuum `.db` size, call
    `vacuum()`, and assert two robust post-conditions: `PRAGMA freelist_count == 0`
    and the on-disk `.db` strictly smaller than the settled pre-vacuum size. Do
    **not** gate on a `freelist_count > 0` precondition (not guaranteed for small
    inserts; it can make the test flaky, and the pre-commit hook runs the full
    suite, so a flaky test blocks every commit).
  - *Compact branch.* `vacuum()` on a fresh/empty (already-compact) store returns
    `Ok`; the CLI test below asserts this prints `(already compact)`, killing the
    `after >= before` comparison mutant.
  - *BUSY mapping.* `map_vacuum_err` is unit-tested directly: a constructed
    `SqliteFailure { DatabaseBusy }` (and `DatabaseLocked`) maps to
    `DbError::StoreInUse`; a non-busy `SqliteFailure` falls through to the
    transparent variant. Deterministic and fast — this exercises the exact branch
    `vacuum()` uses, with no racy or 5-second real-lock contention test.
- **`musefs-cli`:**
  - `run_vacuum` on a temp store with deleted rows returns `Ok`, shrinks the
    file, and prints the reclaimed summary.
  - `run_vacuum` on an already-compact store prints `(already compact)`.
  - `run_vacuum` on a missing `--db` path errors (the `db.exists()` guard).
  - Clap parse test: `vacuum --db <path>` parses to `Command::Vacuum`.
- **No schema change.** This feature adds no table/column/migration (VACUUM and a
  checkpoint pragma touch no DDL), so the Python schema mirror
  (`contrib/python-musefs/.../schema.py`) is untouched and the gating
  `schema_py_fixture_is_fresh` test needs no regeneration. Confirm `MIGRATIONS`
  and all `CREATE` statements are unchanged.
- **Mutation gate.** The new `musefs-db`/`musefs-cli` code is in the CI in-diff
  gate's scope. The delete-then-shrink assertion kills the `VACUUM -> ()` /
  checkpoint-removal mutants; the `map_vacuum_err` tests kill the predicate
  mutants; the `(already compact)` test kills the comparison mutant. Adopting
  `indicatif::HumanBytes` avoids hand-rolled arithmetic and its mutation surface.
  Run the local in-diff gate before pushing; add a documented
  `.cargo/mutants.toml` exclude only if a mutant proves equivalent or
  hang/OOM-class. (The mutant-anchor pre-commit guard covers only
  `musefs-core`/`musefs-format` src, so editing `musefs-db`/`musefs-cli` shifts
  no existing anchors.)

## Out of scope / future

- `auto_vacuum=INCREMENTAL` + an `incremental_vacuum` variant (lighter, online,
  but needs store-creation-time setup) could be revisited later if online
  compaction is ever wanted.
