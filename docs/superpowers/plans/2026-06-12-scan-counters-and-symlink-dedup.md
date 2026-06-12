# Scan counter semantics (#301) + symlink dedup (#302) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `ScanStats` honest — `skipped` = unsupported-extension files (counted at collection), `failed` = supported-extension files that won't probe, `scanned` = distinct tracks — and stop double-counting a file reached via both a real path and a symlink under `--follow-symlinks`.

**Architecture:** All changes are in `musefs-core/src/scan.rs` plus its two integration test files. #301 moves the `skipped` tally from the probe pipeline (where it actually counted parse failures) to the directory walk (where unsupported extensions are dropped), and reclassifies unparseable supported-extension files as `failed`. #302 adds a file-level `(dev, ino)` visited set beside the existing directory-cycle set, active only under follow, so duplicate canonical targets are dropped before dispatch.

**Tech Stack:** Rust (workspace crate `musefs-core`), `cargo test`, `cargo clippy --all-targets -D warnings`, `cargo fmt`, `cargo mutants` (in-diff gate). Serena symbolic tools are the primary edit tools for `scan.rs` per the repo's `CLAUDE.md`.

---

## Background the worker needs

Read [the design spec](../specs/2026-06-12-scan-counters-and-symlink-dedup-design.md) first. Key facts about the current code (`musefs-core/src/scan.rs`):

- The directory walk is `collect_audio` → `collect_audio_inner` → `descend` (mutually recursive). It filters to supported *extensions* via `is_supported_audio`; non-audio files are silently dropped. A `HashSet<(dev, ino)>` (`dir_key`) guards directory-symlink cycles.
- `probe_file` returns an `enum ProbeOutcome { Probed(..), Unsupported, Raced }`. `Unsupported` is produced when `probe_body` returns `None` — i.e. a **supported extension that would not parse** (collection already filtered out unsupported extensions, so `Unsupported` never means "wrong extension").
- `run_pipeline` runs parallel probe workers feeding one writer thread. Workers map `Unsupported → skipped`, `Err(_)`/canonicalize-failure → `failed`, `Raced → raced`. The writer counts `scanned` once per drained unit.
- `scan_directory_with`, `scan_directory_full_oracle` (a whole-file-probe oracle compared for equivalence against the bounded path), and `revalidate_with` all call `collect_audio`.

**Pre-commit gate:** the hook runs `cargo fmt`, `cargo clippy -D warnings`, and the **full workspace test suite**. Every commit must be green — a commit with red tests is rejected. So each task below ends green.

**A subtlety for #302 testing:** a symlink to an already-visited *directory* is already caught by the existing `dir_key` cycle guard, so "symlinked directory reaching the same file" mostly does not double-count today. The genuinely-buggy case is a **file** reachable via a real path and a symlink (same dir or different dir), because file symlinks bypass the directory guard. Tests below cover both, and a comment notes which path each exercises.

---

## File map

- **Modify** `musefs-core/src/scan.rs` — the enum, the walk (`collect_audio`/`collect_audio_inner`/`descend`), `run_pipeline`, `scan_directory_with`, `scan_directory_full_oracle`, and three in-module tests.
- **Modify** `musefs-core/tests/scan_counters.rs` — update `oracle_counts_scanned_and_skipped_exactly` to the new contract and its mutation kill-anchor; add the #302 dedup tests.
- **Modify** `ARCHITECTURE.md` and `CHANGELOG.md` — document the dedup and the operator-visible counter shift.

No new files. No public API signature changes except `collect_audio` (private) gaining a `u64` return.

---

## Task 1a: Rename `ProbeOutcome::Unsupported` → `Unparseable` (pure rename, no behavior change)

Mechanical rename to isolate the behavior change in Task 1b. After this task the variant still maps to `skipped`; all tests still pass.

**Files:**
- Modify: `musefs-core/src/scan.rs` (enum `ProbeOutcome` ~37-45; `probe_file` ~308-334; `run_pipeline` worker arm ~811; in-module test `oversize_unparseable_file_is_skipped_not_read_whole` ~2289-2310)

- [ ] **Step 1: Rename the enum variant and tighten its doc**

In `scan.rs`, replace the `ProbeOutcome` enum body (use Serena `replace_symbol_body` on `ProbeOutcome`):

```rust
/// Outcome of probing one backing file. `Unparseable` is a supported-extension
/// file whose bytes did not parse (counted as a scan `failed`). `Raced` means
/// the file changed under us between the pre- and post-probe `fstat` — the probe
/// may be torn, so nothing is committed for it (#276).
#[derive(Debug)]
enum ProbeOutcome {
    Probed(Probed, BackingStamp),
    Unparseable,
    Raced,
}
```

- [ ] **Step 2: Update `probe_file` doc + construction**

In `probe_file`, change the doc sentence and the `None` arm. The doc line currently reads "Returns `ProbeOutcome::Unsupported` for an unsupported/unparseable file (to be skipped)…"; replace with:

```rust
/// Returns `ProbeOutcome::Unparseable` for a supported-extension file that does
/// not parse (counted as `failed`) and `ProbeOutcome::Raced` if the file
/// changed under us.
```

And the match tail:

```rust
    Ok(match probed {
        Some(p) => ProbeOutcome::Probed(p, s1),
        None => ProbeOutcome::Unparseable,
    })
```

- [ ] **Step 3: Update the `run_pipeline` worker arm (still maps to `skipped` for now)**

In `run_pipeline`, the worker `match probe_file(...)` arm:

```rust
                    Ok(ProbeOutcome::Unparseable) => {
                        skipped.fetch_add(1, Ordering::Relaxed);
                    }
```

- [ ] **Step 4: Update the in-module test's variant reference**

In `oversize_unparseable_file_is_skipped_not_read_whole`, change the `matches!` arm to `ProbeOutcome::Unparseable`:

```rust
        assert!(matches!(
            probe_file(&path, WINDOW).unwrap(),
            ProbeOutcome::Unparseable
        ));
```

- [ ] **Step 5: Build, lint, test**

Run: `cargo test -p musefs-core && cargo clippy -p musefs-core --all-targets`
Expected: PASS, no warnings. (Pure rename — behavior unchanged.)

- [ ] **Step 6: Commit**

```bash
git add musefs-core/src/scan.rs
git commit -m "refactor(scan): rename ProbeOutcome::Unsupported to Unparseable

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 1b: Reclassify counters to the documented contract (#301)

Move `skipped` to the collection phase (unsupported extensions) and map `Unparseable → failed` in the pipeline and the oracle. This task flips behavior, so it updates the value-asserting tests in the same commit to stay green.

**Files:**
- Modify: `musefs-core/src/scan.rs` (`collect_audio`, `collect_audio_inner`, `descend`, `run_pipeline`, `scan_directory_with`, `scan_directory_full_oracle`, in-module test `scan_directory_counts_scanned_and_skipped`)
- Modify: `musefs-core/tests/scan_counters.rs` (`oracle_counts_scanned_and_skipped_exactly` + its kill-anchor)

- [ ] **Step 1: Update the in-module counter test to the new contract (failing test)**

In `scan.rs`'s `hardening_tests` module, replace `scan_directory_counts_scanned_and_skipped` (use Serena `replace_symbol_body` on `hardening_tests/scan_directory_counts_scanned_and_skipped`; **re-include the `#[test]` attribute** — `replace_symbol_body` drops leading attributes otherwise):

```rust
    #[test]
    fn scan_directory_counts_scanned_failed_and_skipped() {
        let dir = tempfile::tempdir().unwrap();
        write_flac(
            &dir.path().join("ok1.flac"),
            &["ARTIST=A", "TITLE=T1"],
            None,
        );
        write_flac(
            &dir.path().join("ok2.flac"),
            &["ARTIST=A", "TITLE=T2"],
            None,
        );
        // Supported extension, unparseable bytes → a scan failure.
        std::fs::write(dir.path().join("bad.flac"), b"garbage").unwrap();
        // Unsupported extension → skipped at collection, never probed.
        std::fs::write(dir.path().join("notes.txt"), b"hello").unwrap();
        let db = musefs_db::Db::open_in_memory().unwrap();
        let stats = crate::scan_directory(&db, dir.path()).unwrap();
        assert_eq!(stats.scanned, 2);
        assert_eq!(stats.failed, 1);
        assert_eq!(stats.skipped, 1);
    }
```

- [ ] **Step 2: Update the oracle counter test (failing test)**

In `musefs-core/tests/scan_counters.rs`, replace `oracle_counts_scanned_and_skipped_exactly` and the comment block immediately above it (the `// === Full-probe oracle counters …` divider, the doc comment, and the `// kills scan …` anchor) with:

```rust
// === Full-probe oracle counters (scan_directory_full_oracle) ===

/// The full-file-probe oracle's `scanned`/`failed`/`skipped` counters must
/// reflect the corpus exactly: one valid FLAC (`scanned`), one extension-only
/// `.flac` of garbage that `is_supported_audio` collects but `probe_full`
/// rejects (`failed`), and one unsupported-extension file dropped at collection
/// (`skipped`). The `+=`→`-=` mutants underflow-panic from 0 and `+=`→`*=` pin
/// the counter at 0, so all three must be asserted nonzero and exact.
// kills scan L<scanned> `stats.scanned += 1` and L<failed> `stats.failed += 1` `+=`→`-=`/`*=`
#[test]
fn oracle_counts_scanned_failed_and_skipped_exactly() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("good.flac"), flac_minimal(b"AUDIO-OK")).unwrap();
    // Extension-only: collected by `is_supported_audio`, rejected by `probe_full`.
    std::fs::write(dir.path().join("bad.flac"), b"not a flac at all").unwrap();
    // Unsupported extension: dropped at collection → skipped.
    std::fs::write(dir.path().join("notes.txt"), b"not audio").unwrap();

    let db = Db::open_in_memory().unwrap();
    let stats = scan_directory_full_oracle(&db, dir.path()).unwrap();

    assert_eq!(stats.scanned, 1, "exactly the one valid FLAC is scanned");
    assert_eq!(stats.failed, 1, "the garbage .flac is a failure");
    assert_eq!(stats.skipped, 1, "the .txt is skipped at collection");
    assert_eq!(db.list_tracks().unwrap().len(), 1);
}
```

The `L<scanned>`/`L<failed>` placeholders in the kill-anchor are filled in during Step 11 once the oracle's final line numbers are known.

- [ ] **Step 3: Run the two tests to verify they fail**

Run: `cargo test -p musefs-core scan_directory_counts_scanned_failed_and_skipped oracle_counts_scanned_failed_and_skipped_exactly`
Expected: FAIL — old code counts `bad.flac` as `skipped` (so `failed == 0`, `skipped` includes it) and does not count `notes.txt`.

- [ ] **Step 4: Thread a `skipped` counter through the walk — `collect_audio`**

Replace `collect_audio` (Serena `replace_symbol_body`):

```rust
fn collect_audio(
    root: &Path,
    out: &mut Vec<PathBuf>,
    follow_symlinks: bool,
) -> std::io::Result<u64> {
    let mut visited = HashSet::new();
    let mut skipped = 0u64;
    if follow_symlinks {
        // Seed with the root's identity so a symlink pointing back to it is
        // caught as a cycle on the first descent.
        if let Ok(meta) = std::fs::metadata(root) {
            visited.insert(dir_key(&meta));
        }
    }
    collect_audio_inner(root, out, follow_symlinks, &mut visited, &mut skipped)?;
    Ok(skipped)
}
```

- [ ] **Step 5: Count unsupported extensions in `collect_audio_inner`**

Replace `collect_audio_inner` (Serena `replace_symbol_body`):

```rust
fn collect_audio_inner(
    root: &Path,
    out: &mut Vec<PathBuf>,
    follow_symlinks: bool,
    visited: &mut HashSet<(u64, u64)>,
    skipped: &mut u64,
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        let ftype = entry.file_type()?;
        if ftype.is_dir() {
            descend(&path, out, follow_symlinks, visited, skipped)?;
        } else if ftype.is_file() {
            if is_supported_audio(&path) {
                out.push(path);
            } else {
                *skipped += 1;
            }
        } else if ftype.is_symlink() {
            if !follow_symlinks {
                log::warn!(
                    "skipping symlink {} (pass --follow-symlinks to scan it)",
                    path.display()
                );
                continue;
            }
            match std::fs::metadata(&path) {
                Ok(meta) if meta.is_dir() => {
                    descend(&path, out, follow_symlinks, visited, skipped)?
                }
                Ok(meta) if meta.is_file() => {
                    if is_supported_audio(&path) {
                        out.push(path);
                    } else {
                        *skipped += 1;
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    log::warn!("skipping broken symlink {}: {e}", path.display());
                }
            }
        }
    }
    Ok(())
}
```

- [ ] **Step 6: Thread `skipped` through `descend`**

Replace `descend` (Serena `replace_symbol_body`):

```rust
fn descend(
    path: &Path,
    out: &mut Vec<PathBuf>,
    follow_symlinks: bool,
    visited: &mut HashSet<(u64, u64)>,
    skipped: &mut u64,
) -> std::io::Result<()> {
    if !follow_symlinks {
        return collect_audio_inner(path, out, follow_symlinks, visited, skipped);
    }
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) => {
            log::warn!("skipping directory {}: {e}", path.display());
            return Ok(());
        }
    };
    if !visited.insert(dir_key(&meta)) {
        log::warn!("skipping symlink cycle at {}", path.display());
        return Ok(());
    }
    collect_audio_inner(path, out, follow_symlinks, visited, skipped)
}
```

- [ ] **Step 7: Map `Unparseable → failed` and drop the `skipped` atomic in `run_pipeline`**

Edit `run_pipeline` (Serena `replace_symbol_body`, or targeted `replace_content` for each spot). Make exactly these four changes:

1. Delete the declaration `let skipped = Arc::new(AtomicU64::new(0));`.
2. Delete the per-worker clone line `let skipped = Arc::clone(&skipped);` (in the `for _ in 0..jobs` setup).
3. Change the worker arm to:

```rust
                    Ok(ProbeOutcome::Unparseable) => {
                        failed.fetch_add(1, Ordering::Relaxed);
                    }
```

4. Change the final return to source `skipped` from nowhere (the caller fills it):

```rust
    Ok(ScanStats {
        scanned,
        skipped: 0,
        failed: failed.load(Ordering::Relaxed),
        raced: raced.load(Ordering::Relaxed),
    })
```

- [ ] **Step 8: Fill `skipped` from collection in `scan_directory_with`**

Replace `scan_directory_with` (Serena `replace_symbol_body`; keep the existing doc comment — re-include it in the body):

```rust
/// Public entry: parallel-probe / single-writer scan of `root`.
///
/// Insert/update a track row for each supported audio file (FLAC, MP3, M4A,
/// Opus, Vorbis, FLAC-in-Ogg) under `root` (with audio bounds and validation
/// stamps), seeding its tags from the file's existing metadata. `root` may be
/// a single audio file (only that file is scanned) or a directory (walked
/// recursively). Files whose extension is not a supported audio format
/// increment `ScanStats::skipped`; supported-extension files with a per-file
/// I/O or parse error increment `ScanStats::failed` and do not abort the scan.
pub fn scan_directory_with(db: &Db, root: &Path, opts: &ScanOptions) -> Result<ScanStats> {
    let mut files = Vec::new();
    let mut skipped = 0u64;
    if root.is_file() {
        if is_supported_audio(root) {
            files.push(root.to_path_buf());
        } else {
            skipped += 1;
        }
    } else {
        skipped += collect_audio(root, &mut files, opts.follow_symlinks)?;
    }
    db.apply_bulk_pragmas_self()?; // scan-scoped tuning on the caller's connection
    let mut stats = run_pipeline(db, files, opts)?;
    stats.skipped = skipped;
    Ok(stats)
}
```

- [ ] **Step 9: Reclassify the oracle (`scan_directory_full_oracle`) in lockstep**

Replace `scan_directory_full_oracle` (Serena `replace_symbol_body`; re-include the `#[doc(hidden)]` attribute and doc comment):

```rust
/// Test/oracle only: scan using the legacy whole-file probe (`probe_full`). The
/// equivalence property compares this against the bounded `scan_directory`.
#[doc(hidden)]
pub fn scan_directory_full_oracle(db: &Db, root: &Path) -> Result<ScanStats> {
    let mut files = Vec::new();
    let mut skipped = 0u64;
    if root.is_file() {
        if is_supported_audio(root) {
            files.push(root.to_path_buf());
        } else {
            skipped += 1;
        }
    } else {
        skipped += collect_audio(root, &mut files, false)?;
    }
    let mut stats = ScanStats {
        scanned: 0,
        skipped,
        failed: 0,
        raced: 0,
    };
    for path in files {
        let bytes = std::fs::read(&path)?;
        let Some(probed) = probe_full(&path, &bytes) else {
            stats.failed += 1;
            continue;
        };
        let meta = std::fs::metadata(&path)?;
        let abs = std::fs::canonicalize(&path)?;
        ingest(db, &abs.to_string_lossy(), &meta, probed)?;
        stats.scanned += 1;
    }
    Ok(stats)
}
```

Note: `revalidate_with` needs **no edit** — its `collect_audio(...)?;` statement already discards the now-`u64` return, and reclassifying `Unparseable → failed` flows through `run_pipeline` automatically.

- [ ] **Step 10: Build, lint, run the full crate suite**

Run: `cargo test -p musefs-core && cargo clippy -p musefs-core --all-targets`
Expected: PASS, no warnings. The two tests from Steps 1-2 now pass; the tolerant `assert_eq!(stats.skipped + stats.failed, 1)` test (zero-byte `bad.flac`) still passes (now `failed == 1`).

- [ ] **Step 11: Refresh the oracle test's kill-anchor line numbers**

Find the oracle's counter lines and fill the `L<scanned>`/`L<failed>` placeholders left in Step 2:

Run: `grep -n "stats.scanned += 1\|stats.failed += 1" musefs-core/src/scan.rs`
Take the two line numbers inside `scan_directory_full_oracle` and edit the `// kills scan L<…>` comment in `scan_counters.rs` to the real numbers, e.g. `// kills scan L933 \`stats.scanned += 1\` and L928 \`stats.failed += 1\` …`.

- [ ] **Step 12: Commit**

```bash
git add musefs-core/src/scan.rs musefs-core/tests/scan_counters.rs
git commit -m "fix(scan): count unsupported extensions as skipped, malformed as failed (#301)

skipped now tallies unsupported-extension files at collection; a
supported-extension file that will not parse counts as failed, matching the
documented ScanStats contract. The full-probe oracle is reclassified in
lockstep so the bounded-vs-full equivalence property holds.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: Deduplicate canonical backing paths under `--follow-symlinks` (#302)

Add a file-level `(dev, ino)` visited set so a file reached via both a real path and a symlink (or two symlink paths) is collected once. Active only when `follow_symlinks` is true; off-mode behavior (incl. hardlinks) is unchanged.

**Files:**
- Modify: `musefs-core/src/scan.rs` (`collect_audio`, `collect_audio_inner`, `descend`; new helper `push_file`)
- Modify: `musefs-core/tests/scan_counters.rs` (new dedup tests)

- [ ] **Step 1: Write the dedup tests (failing tests)**

Append to `musefs-core/tests/scan_counters.rs`:

```rust
// === Symlink dedup under --follow-symlinks (#302) ===

/// A real file and a symlink to it in the same directory ingest once: file-level
/// (dev, ino) dedup (the directory-cycle guard does not apply to file symlinks).
#[test]
fn follow_symlinks_dedups_file_and_sibling_symlink() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let song = dir.path().join("song.flac");
    std::fs::write(&song, flac_minimal(b"AUDIO-SONG")).unwrap();
    symlink(&song, dir.path().join("link.flac")).unwrap();

    let db = Db::open_in_memory().unwrap();
    let opts = ScanOptions {
        follow_symlinks: true,
        ..Default::default()
    };
    let stats = scan_directory_with(&db, dir.path(), &opts).unwrap();

    assert_eq!(stats.scanned, 1, "real file and its symlink ingest once");
    assert_eq!(stats.skipped, 0);
    assert_eq!(stats.failed, 0);
    assert_eq!(db.list_tracks().unwrap().len(), 1);
}

/// The same file reached through two directory paths — one real, one a file
/// symlink in a sibling directory — ingests once. Exercises cross-directory
/// file-level dedup.
#[test]
fn follow_symlinks_dedups_file_across_directories() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a");
    let b = dir.path().join("b");
    std::fs::create_dir(&a).unwrap();
    std::fs::create_dir(&b).unwrap();
    let song = a.join("song.flac");
    std::fs::write(&song, flac_minimal(b"AUDIO-SONG")).unwrap();
    symlink(&song, b.join("alias.flac")).unwrap();

    let db = Db::open_in_memory().unwrap();
    let opts = ScanOptions {
        follow_symlinks: true,
        ..Default::default()
    };
    let stats = scan_directory_with(&db, dir.path(), &opts).unwrap();

    assert_eq!(stats.scanned, 1);
    assert_eq!(db.list_tracks().unwrap().len(), 1);
}

/// A symlinked directory pointing at an already-walked directory reaches the
/// same file by two paths but ingests once. (This case is handled by the
/// existing directory-cycle guard; the test locks in the combined behavior.)
#[test]
fn follow_symlinks_dedups_via_symlinked_directory() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("real");
    std::fs::create_dir(&real).unwrap();
    std::fs::write(real.join("song.flac"), flac_minimal(b"AUDIO-SONG")).unwrap();
    symlink(&real, dir.path().join("mirror")).unwrap();

    let db = Db::open_in_memory().unwrap();
    let opts = ScanOptions {
        follow_symlinks: true,
        ..Default::default()
    };
    let stats = scan_directory_with(&db, dir.path(), &opts).unwrap();

    assert_eq!(stats.scanned, 1);
    assert_eq!(db.list_tracks().unwrap().len(), 1);
}
```

- [ ] **Step 2: Run the dedup tests to verify they fail**

Run: `cargo test -p musefs-core follow_symlinks_dedups`
Expected: `follow_symlinks_dedups_file_and_sibling_symlink` and `follow_symlinks_dedups_file_across_directories` FAIL with `scanned == 2` / `list_tracks().len() == 2`. (`follow_symlinks_dedups_via_symlinked_directory` already passes via the directory guard — that is fine.)

- [ ] **Step 3: Add the `push_file` dedup helper**

Insert a new private function after `dir_key` (Serena `insert_after_symbol` on `dir_key`):

```rust
/// Collect one supported-extension file into `out`, deduplicating by target
/// identity when following symlinks so a real file and a symlink to it (or a
/// file reached via two symlink paths) are ingested once. `known_meta` is the
/// already-resolved target metadata when the caller has it (the symlink arm),
/// avoiding a second `stat`. Dedup is best-effort: if the target cannot be
/// `stat`ed we push it and let the probe pipeline count it rather than dropping
/// it silently.
fn push_file(
    path: &Path,
    out: &mut Vec<PathBuf>,
    follow_symlinks: bool,
    files_visited: &mut HashSet<(u64, u64)>,
    known_meta: Option<&std::fs::Metadata>,
) {
    if !follow_symlinks {
        out.push(path.to_path_buf());
        return;
    }
    let key = match known_meta {
        Some(m) => Some(dir_key(m)),
        None => std::fs::metadata(path).ok().map(|m| dir_key(&m)),
    };
    match key {
        Some(k) if !files_visited.insert(k) => {
            log::debug!("skipping duplicate backing target {}", path.display());
        }
        _ => out.push(path.to_path_buf()),
    }
}
```

- [ ] **Step 4: Thread `files_visited` and route file pushes through `push_file` in `collect_audio_inner`**

Replace `collect_audio_inner` (Serena `replace_symbol_body`):

```rust
fn collect_audio_inner(
    root: &Path,
    out: &mut Vec<PathBuf>,
    follow_symlinks: bool,
    visited: &mut HashSet<(u64, u64)>,
    files_visited: &mut HashSet<(u64, u64)>,
    skipped: &mut u64,
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        let ftype = entry.file_type()?;
        if ftype.is_dir() {
            descend(&path, out, follow_symlinks, visited, files_visited, skipped)?;
        } else if ftype.is_file() {
            if is_supported_audio(&path) {
                push_file(&path, out, follow_symlinks, files_visited, None);
            } else {
                *skipped += 1;
            }
        } else if ftype.is_symlink() {
            if !follow_symlinks {
                log::warn!(
                    "skipping symlink {} (pass --follow-symlinks to scan it)",
                    path.display()
                );
                continue;
            }
            match std::fs::metadata(&path) {
                Ok(meta) if meta.is_dir() => {
                    descend(&path, out, follow_symlinks, visited, files_visited, skipped)?
                }
                Ok(meta) if meta.is_file() => {
                    if is_supported_audio(&path) {
                        push_file(&path, out, follow_symlinks, files_visited, Some(&meta));
                    } else {
                        *skipped += 1;
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    log::warn!("skipping broken symlink {}: {e}", path.display());
                }
            }
        }
    }
    Ok(())
}
```

- [ ] **Step 5: Thread `files_visited` through `descend`**

Replace `descend` (Serena `replace_symbol_body`):

```rust
fn descend(
    path: &Path,
    out: &mut Vec<PathBuf>,
    follow_symlinks: bool,
    visited: &mut HashSet<(u64, u64)>,
    files_visited: &mut HashSet<(u64, u64)>,
    skipped: &mut u64,
) -> std::io::Result<()> {
    if !follow_symlinks {
        return collect_audio_inner(path, out, follow_symlinks, visited, files_visited, skipped);
    }
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) => {
            log::warn!("skipping directory {}: {e}", path.display());
            return Ok(());
        }
    };
    if !visited.insert(dir_key(&meta)) {
        log::warn!("skipping symlink cycle at {}", path.display());
        return Ok(());
    }
    collect_audio_inner(path, out, follow_symlinks, visited, files_visited, skipped)
}
```

- [ ] **Step 6: Create the `files_visited` set in `collect_audio`**

Replace `collect_audio` (Serena `replace_symbol_body`):

```rust
fn collect_audio(
    root: &Path,
    out: &mut Vec<PathBuf>,
    follow_symlinks: bool,
) -> std::io::Result<u64> {
    let mut visited = HashSet::new();
    let mut files_visited = HashSet::new();
    let mut skipped = 0u64;
    if follow_symlinks {
        // Seed with the root's identity so a symlink pointing back to it is
        // caught as a cycle on the first descent.
        if let Ok(meta) = std::fs::metadata(root) {
            visited.insert(dir_key(&meta));
        }
    }
    collect_audio_inner(
        root,
        out,
        follow_symlinks,
        &mut visited,
        &mut files_visited,
        &mut skipped,
    )?;
    Ok(skipped)
}
```

- [ ] **Step 7: Build, lint, run the full crate suite**

Run: `cargo test -p musefs-core && cargo clippy -p musefs-core --all-targets`
Expected: PASS, no warnings. All three #302 tests now pass.

- [ ] **Step 8: Commit**

```bash
git add musefs-core/src/scan.rs musefs-core/tests/scan_counters.rs
git commit -m "fix(scan): dedup canonical backing targets under --follow-symlinks (#302)

A file-level (dev, ino) visited set, active only when following symlinks,
collapses a real file and a symlink to it (or a file reached via two paths)
into one collected candidate, so scanned counts distinct tracks. Off-mode
behavior is unchanged.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: Documentation + release note

**Files:**
- Modify: `ARCHITECTURE.md` (`--follow-symlinks` paragraph in the scanning section, ~344-352)
- Modify: `CHANGELOG.md` (`[Unreleased]` section)

- [ ] **Step 1: Document the dedup in ARCHITECTURE.md**

In the scanning section, the `--follow-symlinks` sentence currently ends "…symlinked audio files and directories are scanned — guarded by a visited `(dev, ino)` set so symlink cycles terminate." Extend it (Edit tool — markdown):

```markdown
flag. Passing `--follow-symlinks` resolves them — symlinked audio files and
directories are scanned — guarded by a visited `(dev, ino)` set so symlink
cycles terminate, and by a second file-level `(dev, ino)` set so a file reached
via both a real path and a symlink is ingested once rather than upserting its
canonical track row twice.
```

- [ ] **Step 2: Add a CHANGELOG entry**

Under `## [Unreleased]`, add a `### Fixed` section (after the existing `### Added`):

```markdown
### Fixed

- **Scan counters now match their documented contract:** `musefs scan` reports
  unsupported-extension files (e.g. `.txt`, `.jpg`) as `skipped` and
  supported-extension files that fail to parse (e.g. a corrupt `.flac`) as
  `failed`. Previously malformed files were miscounted as `skipped` and
  unsupported files were not counted at all (#301).
- **Symlink scans no longer double-count:** with `--follow-symlinks`, a file
  reached via both its real path and a symlink is ingested and counted once
  instead of inflating `scanned` (#302).
```

- [ ] **Step 3: Commit**

```bash
git add ARCHITECTURE.md CHANGELOG.md
git commit -m "docs: scan counter semantics and symlink dedup (#301, #302)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: Full verification + mutation gate

Confirm the whole workspace is green and the mutation kill-anchors did not silently rot.

- [ ] **Step 1: Workspace fmt + clippy + tests**

Run: `cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace`
Expected: all PASS. This includes `musefs-core/tests/probe_equivalence.rs`, whose corpus (`common::corpus::generate`) is all valid audio of each format — no unsupported extensions or garbage — so it compares DB rows only and is unaffected by the counter reclassification; confirm it stays green. (Note per repo memory: the `rtk` hook may summarize `cargo test` output to `cargo test: N passed`; trust the exit code, not a grep for "test result".)

- [ ] **Step 2: metrics-feature tests (CI's `check` job runs these; local `--workspace` skips them)**

Run: `cargo test -p musefs-core --features metrics`
Expected: PASS. This scan change touches `collect_audio`/`run_pipeline` but not the getattr/read stat counters, so these should be unaffected — confirm rather than assume.

- [ ] **Step 3: Audit every `// kills scan L…` anchor for line drift**

Run: `grep -n "kills scan L" musefs-core/tests/scan_counters.rs`
For each anchor, open the cited expression in `musefs-core/src/scan.rs` and confirm the line number still matches; fix any that drifted from the Task 1b/Task 2 edits. The anchors reference lines in `probe_file`, `run_pipeline`, and `scan_directory_full_oracle` — all of which moved.

- [ ] **Step 4: Run the in-diff mutation gate**

Per repo convention (memory: copy-mode OOMs on worktree targets; use in-place, serial):

Run: `cargo mutants --in-place -p musefs-core` (or the project's in-diff invocation `cargo mutants --in-diff <diff>` if scoped to the branch diff)
Expected: no surviving or unviable mutants introduced by the diff. If a counter-arithmetic mutant survives, the corresponding assertion or kill-anchor needs tightening. Sanity-check that the diff fed to the gate is non-empty (an empty diff is a silent false pass).

- [ ] **Step 5 (if any anchor changed): commit the anchor fixes**

```bash
git add musefs-core/tests/scan_counters.rs
git commit -m "test(scan): refresh mutation kill-anchors after counter changes

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Self-review notes (for the executor)

- **`replace_symbol_body` drops leading attributes/doc comments** — every replaced symbol in this plan re-includes its `#[test]` / `#[doc(hidden)]` / doc comment in the body. Do not strip them.
- **Signatures change across Task 1b and Task 2** (`collect_audio` return type; `collect_audio_inner`/`descend` gain params). The code will not compile until *all* mutually-recursive callers in a task are updated — that is why each task builds only at its final "build + test" step, not between individual symbol edits.
- **`revalidate_with` is intentionally untouched** — verify by `cargo test -p musefs-core revalidate` after Task 1b that its tests stay green.
- **Type/name consistency check:** the variant is `ProbeOutcome::Unparseable` everywhere; the helper is `push_file`; the two visited sets are `visited` (directories) and `files_visited` (files); `collect_audio` returns `std::io::Result<u64>`.
