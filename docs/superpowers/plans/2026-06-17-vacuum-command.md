# `musefs vacuum` Command Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a `musefs vacuum --db <store>` subcommand that compacts the SQLite store (reclaims free pages left by deletions) and reports bytes reclaimed.

**Architecture:** A `Db::<ReadWrite>::vacuum()` method in `musefs-db` runs `VACUUM` then `PRAGMA wal_checkpoint(TRUNCATE)` and maps SQLite BUSY/locked to a typed `DbError::StoreInUse`. The CLI adds a thin `Command::Vacuum` + `run_vacuum` that guards an existing store (like `mount --db`), measures the on-disk footprint before/after, and prints a summary. Layering stays `db → cli`.

**Tech Stack:** Rust, rusqlite (bundled SQLite), clap (derive), anyhow (CLI only), indicatif (`HumanBytes`), thiserror, tempfile (dev).

## Global Constraints

- Spec: `docs/superpowers/specs/2026-06-17-vacuum-command-design.md`.
- Work happens on the existing `release/v1.1.0` branch (folds into PR #566). Do **not** create a worktree or new branch.
- The pre-commit hook runs `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, the **full workspace test suite**, and ruff — every commit must be green. Non-docs commits run the cargo gate; docs-only commits (all staged paths under `docs/` or `*.md`) skip it.
- Integer-cast convention: widenings via `From`/`u64::from`; `as` only under a reasoned `#[expect]`. The code in this plan uses no narrowing casts.
- Error convention: `musefs-db` returns `DbError`; the CLI is the only `anyhow` consumer.
- No schema change: this feature adds no table/column/migration, so the Python schema mirror is untouched and needs no regen.
- `with_raw_conn` is `#[cfg(feature = "fuzzing")]` — not usable here; the method uses `self.conn` directly (every other `Db<ReadWrite>` write method does).
- Mutant-anchor pre-commit guard covers only `musefs-core`/`musefs-format` src — editing `musefs-db`/`musefs-cli` shifts no anchors.

---

### Task 1: `musefs-db` — `vacuum()`, `StoreInUse`, `map_vacuum_err`

**Files:**
- Create: `musefs-db/src/maintenance.rs`
- Modify: `musefs-db/src/lib.rs` (add `mod maintenance;` to the module list, lines 1-10)
- Modify: `musefs-db/src/error.rs` (add `StoreInUse` variant after `StoreTooNew`, ~line 24)

**Interfaces:**
- Consumes: `Db<ReadWrite>` with field `conn: Connection` (`lib.rs:93-97`); `Db::open<P: AsRef<Path>>(path) -> Result<Db>`; `Db::open_in_memory() -> Result<Db>`; `DbError` (`error.rs`); `NewArt { mime, width, height, data }`, `NewTrack { .. }` (`models.rs`); `upsert_art(&NewArt) -> Result<i64>`, `gc_orphan_art() -> Result<usize>` (`art.rs`); `pub type Result<T> = std::result::Result<T, DbError>`.
- Produces:
  - `DbError::StoreInUse(rusqlite::Error)` — new enum variant.
  - `Db::<ReadWrite>::vacuum(&self) -> Result<()>` — runs VACUUM + TRUNCATE checkpoint.
  - `fn map_vacuum_err(err: rusqlite::Error) -> DbError` (module-private to `maintenance`) — BUSY/locked → `StoreInUse`, else `Sqlite`.

- [ ] **Step 1: Add the `StoreInUse` error variant**

In `musefs-db/src/error.rs`, insert after the `StoreTooNew { .. }` variant (after line 24, before `FieldTooLarge`):

```rust
    #[error(
        "the store is in use — unmount the filesystem or stop any scan before vacuuming"
    )]
    StoreInUse(#[source] rusqlite::Error),
```

- [ ] **Step 2: Register the module**

In `musefs-db/src/lib.rs`, add `mod maintenance;` between `pub mod limits;` and `mod models;` (keep the existing alphabetical grouping):

```rust
pub mod limits;
mod maintenance;
mod models;
```

- [ ] **Step 3: Write the failing tests (create `maintenance.rs` with tests only)**

Create `musefs-db/src/maintenance.rs` with exactly this content (the `impl`/`fn` come in Step 5; this step is the test module plus stub signatures so it compiles-and-fails meaningfully):

```rust
//! Store maintenance operations: compaction (`VACUUM` + WAL checkpoint).

use crate::{Db, DbError, ReadWrite, Result};

impl Db<ReadWrite> {
    /// Compact the store: reclaim free pages left by deletions, then truncate
    /// the WAL. Runs a full `VACUUM` (rewrites the whole database — transiently
    /// needs free disk roughly equal to the store size) followed by
    /// `PRAGMA wal_checkpoint(TRUNCATE)`. The TRUNCATE checkpoint *after* VACUUM
    /// is what actually shrinks the main `.db` file on disk and zeroes the
    /// `-wal`. A busy/locked store (e.g. a live mount) maps to
    /// [`DbError::StoreInUse`].
    pub fn vacuum(&self) -> Result<()> {
        self.conn.execute_batch("VACUUM").map_err(map_vacuum_err)?;
        self.conn
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .map_err(map_vacuum_err)?;
        Ok(())
    }
}

/// Translate a VACUUM/checkpoint error: a SQLite busy/locked failure means the
/// store is open elsewhere (a mount or scan), surfaced as the actionable
/// [`DbError::StoreInUse`]; everything else flows through the transparent
/// rusqlite variant.
fn map_vacuum_err(err: rusqlite::Error) -> DbError {
    if let rusqlite::Error::SqliteFailure(e, _) = &err {
        if matches!(
            e.code,
            rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
        ) {
            return DbError::StoreInUse(err);
        }
    }
    DbError::Sqlite(err)
}

#[cfg(test)]
mod tests {
    use super::map_vacuum_err;
    use crate::models::NewArt;
    use crate::{Db, DbError};

    #[test]
    fn vacuum_shrinks_file_and_truncates_wal_after_deletion() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.db");
        let db = Db::open(&path).unwrap();

        // Allocate many pages: 16 distinct 256 KiB art blobs (~4 MiB).
        for i in 0..16u8 {
            db.upsert_art(&NewArt {
                mime: "image/png".into(),
                width: None,
                height: None,
                data: vec![i; 256 * 1024],
            })
            .unwrap();
        }
        // None are linked to a track, so they are all orphan: free their pages.
        assert_eq!(db.gc_orphan_art().unwrap(), 16);

        // Settle the WAL so the pre-vacuum main-file size reflects the deletes.
        db.conn
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .unwrap();
        let before = std::fs::metadata(&path).unwrap().len();

        db.vacuum().unwrap();

        let after = std::fs::metadata(&path).unwrap().len();
        assert!(after < before, "expected shrink: {before} -> {after}");

        let freelist: i64 = db
            .conn
            .query_row("PRAGMA freelist_count", [], |r| r.get(0))
            .unwrap();
        assert_eq!(freelist, 0, "vacuum must leave no free pages");

        // The TRUNCATE checkpoint inside vacuum() must drain the WAL: a
        // subsequent checkpoint reports 0 frames in the log (column 1 of
        // `PRAGMA wal_checkpoint` is the WAL frame count). Deterministic, and
        // unlike a `-wal` file-size check it does not depend on WAL internals.
        // Without the in-method checkpoint, VACUUM's frames are still pending
        // here, so this is non-zero and the checkpoint-removal mutant dies.
        let wal_frames: i64 = db
            .conn
            .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |r| r.get(1))
            .unwrap();
        assert_eq!(wal_frames, 0, "vacuum must checkpoint the WAL");
    }

    #[test]
    fn vacuum_on_empty_store_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("t.db")).unwrap();
        db.vacuum().unwrap();
    }

    #[test]
    fn map_vacuum_err_maps_busy_and_locked_to_store_in_use() {
        use rusqlite::{ffi, Error};
        let busy = Error::SqliteFailure(ffi::Error::new(ffi::SQLITE_BUSY), None);
        assert!(matches!(map_vacuum_err(busy), DbError::StoreInUse(_)));
        let locked = Error::SqliteFailure(ffi::Error::new(ffi::SQLITE_LOCKED), None);
        assert!(matches!(map_vacuum_err(locked), DbError::StoreInUse(_)));
    }

    #[test]
    fn map_vacuum_err_passes_through_other_errors() {
        use rusqlite::{ffi, Error};
        let corrupt = Error::SqliteFailure(ffi::Error::new(ffi::SQLITE_CORRUPT), None);
        assert!(matches!(map_vacuum_err(corrupt), DbError::Sqlite(_)));
    }
}
```

**TDD note (recommended red-first):** `vacuum()`, `map_vacuum_err`, and the tests live in one small file and must compile together. To observe a real red, first paste the module with `vacuum`'s body stubbed — `pub fn vacuum(&self) -> Result<()> { Ok(()) }` (drop the two `execute_batch` lines; keep `map_vacuum_err` so the mapping tests still compile) — run Step 4a, and watch `vacuum_shrinks_file_and_truncates_wal_after_deletion` fail (no shrink, `freelist_count != 0`) while the three other tests pass. Then restore the real two-statement body shown above and run Step 4b.

- [ ] **Step 4a: Run with the stub — verify the shrink test fails (red)**

Run: `cargo test -p musefs-db maintenance`
Expected: FAIL — `vacuum_shrinks_file_and_truncates_wal_after_deletion` fails on `after < before` (the stub does not compact); the two `map_vacuum_err` tests and `vacuum_on_empty_store_is_ok` PASS.

- [ ] **Step 4b: Restore the real body — verify all pass (green)**

Run: `cargo test -p musefs-db maintenance`
Expected: PASS — `vacuum_shrinks_file_and_truncates_wal_after_deletion`, `vacuum_on_empty_store_is_ok`, `map_vacuum_err_maps_busy_and_locked_to_store_in_use`, `map_vacuum_err_passes_through_other_errors` all green.

If `vacuum_shrinks_file...` fails on `after < before`: confirm the `gc_orphan_art` returned 16 and the pre-vacuum checkpoint ran (the main file must be bloated before vacuuming).

- [ ] **Step 5: Lint**

Run: `cargo clippy -p musefs-db --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 6: Commit**

```bash
git add musefs-db/src/maintenance.rs musefs-db/src/lib.rs musefs-db/src/error.rs
git commit -m "feat(db): add Db::vacuum() store compaction (VACUUM + WAL truncate)"
```

(The pre-commit hook runs the full workspace suite; expect it green.)

---

### Task 2: `musefs-cli` — `vacuum` subcommand and `run_vacuum`

**Files:**
- Modify: `musefs-cli/src/lib.rs` — import (line 10), `Command` enum (after the `Mount(MountArgs)` arm, ~line 217), new `run_vacuum`/`store_footprint`/`vacuum_summary` (after `run_mount`, before `run`), dispatch arm in `run` (~line 607), tests (in `mod tests`, ~line 612).

**Interfaces:**
- Consumes: `Db::open` and `Db::<ReadWrite>::vacuum()` from Task 1; `musefs_db::{Db, NewArt}`; `indicatif::HumanBytes`; `anyhow::{Context, Result, bail}`; `std::process::ExitCode`.
- Produces: `Command::Vacuum { db: PathBuf }`; `pub fn run_vacuum(db: &Path) -> anyhow::Result<()>`; private `fn store_footprint(db: &Path) -> u64`; private `fn vacuum_summary(path: &Path, before: u64, after: u64) -> String`.

- [ ] **Step 1: Add the `HumanBytes` import**

In `musefs-cli/src/lib.rs` line 10, change:

```rust
use indicatif::HumanDuration;
```
to:
```rust
use indicatif::{HumanBytes, HumanDuration};
```

- [ ] **Step 2: Add the `Vacuum` command variant**

In the `Command` enum, after the `Mount(MountArgs)` arm (the last arm, ~line 217), add:

```rust
    /// Compact the SQLite store, reclaiming free pages left by deletions
    /// (prunes, orphan-art GC, the schema migration). Run while unmounted; this
    /// may also upgrade an older store's schema to the current version.
    Vacuum {
        /// Path to the SQLite database.
        #[arg(long, env = "MUSEFS_DB")]
        db: PathBuf,
    },
```

- [ ] **Step 3: Write the failing tests**

In `musefs-cli/src/lib.rs`, inside `mod tests` (after the existing `read_ahead_budget_flag_maps_to_mount_config` test), add:

```rust
    #[test]
    fn vacuum_command_parses_db_path() {
        use clap::Parser;
        let cli = Cli::try_parse_from(["musefs", "vacuum", "--db", "/tmp/x.db"]).unwrap();
        let Command::Vacuum { db } = cli.command else {
            panic!("expected Vacuum");
        };
        assert_eq!(db, PathBuf::from("/tmp/x.db"));
    }

    #[test]
    fn run_vacuum_errors_on_missing_db() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.db");
        assert!(run_vacuum(&missing).is_err());
    }

    #[test]
    fn run_vacuum_compacts_a_bloated_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lib.db");
        {
            let db = musefs_db::Db::open(&path).unwrap();
            for i in 0..16u8 {
                db.upsert_art(&musefs_db::NewArt {
                    mime: "image/png".into(),
                    width: None,
                    height: None,
                    data: vec![i; 256 * 1024],
                })
                .unwrap();
            }
            assert_eq!(db.gc_orphan_art().unwrap(), 16);
        }
        let before = std::fs::metadata(&path).unwrap().len();
        run_vacuum(&path).unwrap();
        let after = std::fs::metadata(&path).unwrap().len();
        assert!(after < before, "expected shrink: {before} -> {after}");
    }

    #[test]
    fn vacuum_summary_reports_reclaimed_then_compact() {
        let p = Path::new("/x.db");
        assert!(vacuum_summary(p, 1000, 400).contains("reclaimed"));
        // Equal sizes => already compact.
        assert!(vacuum_summary(p, 400, 400).contains("already compact"));
        // VACUUM can grow a tiny file; saturating delta must not go negative.
        assert!(vacuum_summary(p, 400, 410).contains("already compact"));
    }
```

- [ ] **Step 4: Run tests to verify they fail**

Run: `cargo test -p musefs-cli vacuum`
Expected: FAIL — compile errors (`run_vacuum`, `vacuum_summary`, `Command::Vacuum` not found) until Steps 2 and 5 land. (Step 2 adds the variant; the parse test still fails to compile without Step 5's functions.)

- [ ] **Step 5: Implement `run_vacuum`, `store_footprint`, `vacuum_summary`**

In `musefs-cli/src/lib.rs`, after `run_mount` and before `pub fn run`, add:

```rust
/// Total on-disk footprint of the store: the main `.db` plus its `-wal`/`-shm`
/// sidecars (a missing sidecar counts as 0). Summing the WAL/SHM keeps the
/// reclaimed figure honest — a TRUNCATE checkpoint reclaims WAL bytes too.
fn store_footprint(db: &Path) -> u64 {
    // `.map_or`, not `.map(..).unwrap_or(..)`: the latter trips
    // `clippy::map_unwrap_or` (pedantic), which the `-D warnings` gate rejects.
    let main = std::fs::metadata(db).map_or(0, |m| m.len());
    let side: u64 = ["-wal", "-shm"]
        .iter()
        .map(|suffix| {
            let mut p = db.as_os_str().to_os_string();
            p.push(suffix);
            std::fs::metadata(p).map_or(0, |m| m.len())
        })
        .sum();
    main + side
}

/// One-line summary for a completed vacuum. `after >= before` (VACUUM can grow
/// an already-compact file slightly) reports `(already compact)` rather than a
/// zero or negative delta.
fn vacuum_summary(path: &Path, before: u64, after: u64) -> String {
    let reclaimed = before.saturating_sub(after);
    if reclaimed == 0 {
        format!(
            "vacuumed {}: {} (already compact)",
            path.display(),
            HumanBytes(after)
        )
    } else {
        format!(
            "vacuumed {}: {} → {} (reclaimed {})",
            path.display(),
            HumanBytes(before),
            HumanBytes(after),
            HumanBytes(reclaimed)
        )
    }
}

/// Compact the SQLite store at `db`. Best-effort: a store in use (a live mount
/// or a running scan) surfaces `DbError::StoreInUse`'s actionable message.
pub fn run_vacuum(db: &Path) -> Result<()> {
    if !db.exists() {
        anyhow::bail!("database not found: {} (nothing to vacuum)", db.display());
    }
    let before = store_footprint(db);
    let store = Db::open(db).with_context(|| format!("opening store {}", db.display()))?;
    store.vacuum()?;
    let after = store_footprint(db);
    println!("{}", vacuum_summary(db, before, after));
    Ok(())
}
```

- [ ] **Step 6: Wire the dispatch arm**

In `pub fn run`, after the `Command::Mount(args) => ...` arm (~line 607), add:

```rust
        Command::Vacuum { db } => run_vacuum(&db).map(|()| ExitCode::SUCCESS),
```

- [ ] **Step 7: Run tests to verify they pass**

Run: `cargo test -p musefs-cli vacuum`
Expected: PASS — `vacuum_command_parses_db_path`, `run_vacuum_errors_on_missing_db`, `run_vacuum_compacts_a_bloated_store`, `vacuum_summary_reports_reclaimed_then_compact`.

- [ ] **Step 8: Lint**

Run: `cargo clippy -p musefs-cli --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 9: Manual smoke (optional but recommended)**

```bash
cargo run -q -p musefs -- vacuum --db /nonexistent.db   # expect: error: database not found ...
```
Expected: the not-found error, exit non-zero.

- [ ] **Step 10: Commit**

```bash
git add musefs-cli/src/lib.rs
git commit -m "feat(cli): add 'musefs vacuum' subcommand"
```

---

### Task 3: Docs & changelog

**Files:**
- Create: `docs/src/guide/maintenance.md`
- Modify: `docs/src/SUMMARY.md` (User Guide section)
- Modify: `CHANGELOG.md` (root, the `## [1.1.0] - 2026-06-17` `### Added` list)
- Modify: `docs/src/changelog.md` (full, the `## [1.1.0] - 2026-06-17` `### Added` list)
- Modify: `docs/src/release-notes.md` (v1.1.0 Highlights list)

**Interfaces:** none (documentation only).

- [ ] **Step 1: Create the maintenance page**

Create `docs/src/guide/maintenance.md`:

```markdown
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
```

- [ ] **Step 2: Add the page to the table of contents**

In `docs/src/SUMMARY.md`, under the `# User Guide` section, add a `Maintenance` entry after `Tuning & metrics`:

```markdown
- [Tuning & metrics](guide/tuning.md)
- [Maintenance](guide/maintenance.md)
- [Ownership, permissions & config](guide/configuration.md)
```

- [ ] **Step 3: Add the changelog entry (root `CHANGELOG.md`)**

In `CHANGELOG.md`, under `## [1.1.0] - 2026-06-17` → `### Added`, append this bullet at the end of the Added list (before the `### Changed` heading):

```markdown
- **`musefs vacuum` command:** compact the SQLite store, reclaiming free pages
  left by prunes, orphan-art GC, and the schema migration. Runs `VACUUM` + a WAL
  checkpoint and reports the space reclaimed; run it while unmounted (#566).
```

- [ ] **Step 4: Add the changelog entry (full `docs/src/changelog.md`)**

In `docs/src/changelog.md`, under `## [1.1.0] - 2026-06-17` → `### Added`, append the same bullet at the end of the Added list (before the `### Fixed` heading):

```markdown
- **`musefs vacuum` command:** compact the SQLite store, reclaiming free pages
  left by prunes, orphan-art GC, and the schema migration. Runs `VACUUM` + a WAL
  checkpoint and reports the space reclaimed; run it while unmounted (#566).
```

- [ ] **Step 5: Add a release-notes highlight**

In `docs/src/release-notes.md`, in the **v1.1.0 → Highlights** bullet list, add as the last bullet (after the "Per-extension skip breakdown" item, before the "Plus a substantial round…" paragraph):

```markdown
- **`musefs vacuum`.** A maintenance command that compacts the SQLite store —
  reclaiming the free pages that prunes, orphan-art GC, and the migration leave
  behind — and reports the space reclaimed. Run it while unmounted. See
  [Maintenance](guide/maintenance.md).
```

- [ ] **Step 6: Build the docs to check links**

Run: `cd docs && mdbook build >/dev/null && echo OK`
Expected: `OK` (no broken-link errors; the new `guide/maintenance.md` resolves).

- [ ] **Step 7: Commit (docs-only — cargo gate skipped)**

```bash
git add docs/src/guide/maintenance.md docs/src/SUMMARY.md CHANGELOG.md docs/src/changelog.md docs/src/release-notes.md
git commit -m "docs: document the musefs vacuum command"
```

---

### Task 4: Full verification & fold into PR #566

**Files:** none (verification only).

**Interfaces:** none.

- [ ] **Step 1: Full workspace test suite**

Run: `cargo test --workspace`
Expected: all green (this mirrors what the pre-commit hook already ran on each commit).

- [ ] **Step 2: Metrics-feature tests (CI `check` job covers this; local default skips it)**

Run: `cargo test -p musefs-core --features metrics`
Expected: green. (This feature touches no getattr/read counters, so it should pass untouched — run it to be sure.)

- [ ] **Step 3: Workspace lint**

Run: `cargo clippy --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 4: Format check**

Run: `cargo fmt --all --check`
Expected: clean (exit 0).

- [ ] **Step 5: Local in-diff mutation gate**

Run the in-diff mutation gate against the new code (serial, in-place, `/tmp` TMPDIR), per the repo's documented local procedure:

```bash
cargo mutants --in-place --in-diff <(git diff origin/main...HEAD -- musefs-db musefs-cli) -- --offline
```

Diff-base note: PR #566's base is `main`, and the release-prep commits on this
branch touched no `musefs-db`/`musefs-cli` **src** (only `Cargo.toml` versions,
which `cargo mutants` ignores), so `origin/main...HEAD` restricted to those two
crates yields exactly the three vacuum src files. If you instead want only the
vacuum commits, diff against the commit before Task 1 (e.g. `git diff
<task1-parent>...HEAD -- musefs-db musefs-cli`). Run `git fetch origin` first so
`origin/main` is current.

Expected: no surviving mutants in `maintenance.rs` / the new CLI functions. The `after < before` shrink assertion kills the `VACUUM`-removal mutant, and the `PRAGMA wal_checkpoint(PASSIVE)` frame-count assertion (`wal_frames == 0`) kills the checkpoint-removal mutant; the `map_vacuum_err` tests kill the predicate mutants; `vacuum_summary_reports_reclaimed_then_compact` kills the `saturating_sub`/comparison mutants.

If a mutant survives and is genuinely equivalent or hang/OOM-class, add a documented `exclude_re`/anchor to `.cargo/mutants.toml` in the same commit (per the repo convention) — do **not** weaken a test to pass.

- [ ] **Step 6: Push the branch (updates PR #566)**

```bash
git push origin release/v1.1.0
```
Expected: the three new commits land on the open PR; CI (`ci-ok` + `coverage-ok`) re-runs.

- [ ] **Step 7: Confirm the PR reflects the new command**

Verify on PR #566 that the diff now includes `musefs vacuum`, and that the PR body's feature summary mentions it (edit the PR body to add a `vacuum` line if it helps reviewers — optional).

---

## Self-Review

- **Spec coverage:** scope (VACUUM + checkpoint) → Task 1; best-effort BUSY handling → Task 1 (`map_vacuum_err`/`StoreInUse`); CLI `--db` + existing-store guard + reclaimed report → Task 2; total-footprint measurement → Task 2 (`store_footprint`); migrate-on-open + inherited failure modes → Task 2 (`run_vacuum` opens via `Db::open`) and Task 3 docs; dedicated maintenance page → Task 3; no-schema-change note → Global Constraints + Task 4 (no schema-py regen); test plan (shrink, compact branch, BUSY mapping, parse, missing-db) → Tasks 1-2; mutation gate → Task 4. All spec sections map to a task.
- **Placeholder scan:** no TBD/TODO; every code step shows complete code; every command shows expected output.
- **Type consistency:** `vacuum(&self) -> Result<()>`, `map_vacuum_err(rusqlite::Error) -> DbError`, `DbError::StoreInUse(rusqlite::Error)`, `run_vacuum(&Path) -> anyhow::Result<()>`, `store_footprint(&Path) -> u64`, `vacuum_summary(&Path, u64, u64) -> String`, `Command::Vacuum { db: PathBuf }` — used identically across tasks. `NewArt { mime, width, height, data }` and `gc_orphan_art() -> Result<usize>` match `models.rs`/`art.rs`.
```
