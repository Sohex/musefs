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
module (matching the one-file-per-concern split of `art.rs`/`tags.rs`/
`tracks.rs`/`bulk.rs`/`structural.rs`; `is_busy` and any future
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
- BUSY mapping: a small pure helper `fn is_busy(err: &rusqlite::Error) -> bool`
  returns true for `rusqlite::Error::SqliteFailure` whose code is
  `DatabaseBusy` or `DatabaseLocked`. When `vacuum()` hits a busy/locked error
  it returns a new typed error variant:

  ```
  DbError::StoreInUse  // #[error("the store is in use — unmount the filesystem
                       //          or stop any scan before vacuuming")]
  ```

  carrying the underlying `rusqlite::Error` as its `#[source]`. All other errors
  continue to flow through the existing transparent rusqlite `DbError` variant.

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
  2. Stat the file size (`before`).
  3. `Db::open(db)` (this runs `migrate()` as every open does — see note below).
  4. `store.vacuum()?` — `DbError::StoreInUse` surfaces its own actionable
     message through anyhow.
  5. Re-stat the file size (`after`).
  6. Print one stdout line:
     `vacuumed <path>: <before> → <after> (reclaimed <delta>)` using a small
     `human_bytes(u64) -> String` helper. If `after >= before`, report
     `(already compact)` instead of a negative/zero delta (use saturating
     subtraction).
- Dispatch: `Command::Vacuum { db } => run_vacuum(&db).map(|()| ExitCode::SUCCESS)`.
  Success exits `0`; any error exits `1` via the existing anyhow→main path.
  No exit-code `2` semantics.

The reported sizes are the main `.db` file (the dominant component and the
user's actual concern); the `-wal`/`-shm` files are not summed into the figure.

**Migrate-on-open note.** Because `Db::open` always migrates, vacuuming a
v1.0.0-schema store also upgrades it to schema v2 as a side effect. This is
benign and consistent with every other write entry point; it will be called out
in one line in the command help and the maintenance docs.

### Docs & changelog

- New `docs/src/guide/maintenance.md`: a short "Maintenance" page documenting
  `musefs vacuum` — what it reclaims, the "run while unmounted" caveat, the
  reclaimed-bytes output, and the migrate-on-open note. Add it to
  `docs/src/SUMMARY.md` under **User Guide**.
- README: add `vacuum` to the command list only if the README enumerates
  subcommands; otherwise leave it to the guide.
- Changelog: add a `### Added` bullet to the already-promoted **[1.1.0]**
  section of both `CHANGELOG.md` (root, curated) and `docs/src/changelog.md`
  (full), and add a bullet to the v1.1.0 highlights in
  `docs/src/release-notes.md`.

## Test plan

Tests are written TDD-first and every commit lands green (the pre-commit hook
runs the full workspace suite).

- **`musefs-db`** (temp-file store — file-size shrink needs a real file, not
  in-memory):
  - Insert tracks/tags/art, delete a chunk, assert `PRAGMA freelist_count > 0`;
    call `vacuum()`; assert `freelist_count == 0` and the on-disk `.db` size
    shrank.
  - `vacuum()` on a fresh/empty store returns `Ok`.
  - `is_busy` mapping is unit-tested directly by constructing a
    `SqliteFailure` with `DatabaseBusy` and asserting it maps to
    `DbError::StoreInUse` (deterministic; avoids racy real-lock contention).
- **`musefs-cli`:**
  - `run_vacuum` on a temp store returns `Ok` (and shrinks / prints a summary).
  - `run_vacuum` on a missing `--db` path errors.
  - Clap parse test: `vacuum --db <path>` parses to `Command::Vacuum`.
- **Mutation gate:** the new `musefs-db`/`musefs-cli` code falls in the CI
  in-diff mutation gate's scope. The delete-then-shrink assertion kills the
  `VACUUM -> ()` / checkpoint mutants; the `is_busy` test kills the predicate
  mutants. Run the local in-diff gate before pushing; add a documented
  `.cargo/mutants.toml` exclude only if a mutant proves equivalent or
  hang/OOM-class.

## Out of scope / future

- `auto_vacuum=INCREMENTAL` + an `incremental_vacuum` variant (lighter, online,
  but needs store-creation-time setup) could be revisited later if online
  compaction is ever wanted.
