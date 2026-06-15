# Scan counter semantics (#301) + symlink dedup (#302) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `ScanStats` honest — `skipped` = unsupported-extension files (counted at collection), `failed` = supported-extension files that won't probe, `scanned` = distinct tracks — and stop double-counting a file reached via both a real path and a symlink under `--follow-symlinks`.

**Architecture:** All code changes are in `musefs-core/src/scan.rs` plus its two integration test files. #301 moves the `skipped` tally from the probe pipeline (where it actually counted parse failures) to the directory walk (where unsupported extensions are dropped), and reclassifies unparseable supported-extension files as `failed`. #302 adds a file-level `(dev, ino)` visited set beside the existing directory-cycle set, active only under follow, so duplicate canonical targets are dropped before dispatch.

**Tech Stack:** Rust (`musefs-core`), `cargo test`, `cargo clippy --all-targets -D warnings`, `cargo fmt`, `cargo mutants` (anchor-drift guard + in-diff gate). Serena symbolic tools are the primary edit tools for `scan.rs` per the repo's `CLAUDE.md`.

---

## Read this before starting

1. **Read the spec:** [docs/superpowers/specs/2026-06-12-scan-counters-and-symlink-dedup-design.md](../specs/2026-06-12-scan-counters-and-symlink-dedup-design.md).

2. **Line numbers in this plan are navigation hints, not edit coordinates.** Every code edit targets a *named symbol* via Serena (`replace_symbol_body`, `insert_after_symbol`); locate symbols by name, not by the cited line. The numbers were accurate at authoring but the file shifts as you edit it.

3. **The pre-commit hook is strict.** `.githooks/pre-commit` runs `cargo fmt`, `cargo clippy -D warnings`, the **full workspace test suite**, AND — whenever a `musefs-core/src/*.rs` file is staged and `cargo-mutants` is installed — a **mutant-anchor drift guard** (`scripts/check_mutant_anchors.py`). Every commit must satisfy all of them. A red test, a warning, or a drifted anchor rejects the commit.

4. **The mutant-anchor guard is the subtle one.** `.cargo/mutants.toml` excludes specific equivalent/unkillable mutants by exact `file:line:col`, each tagged with a `# guard: op="…" fn="…" rows=N` comment. The guard re-derives the live mutant list and checks each anchor still lands on exactly that operator in that function. **Any edit that shifts a line in `scan.rs` moves these anchors and trips the guard** — so each `scan.rs`-touching commit below re-anchors `.cargo/mutants.toml` in the *same* commit. The affected `scan.rs` anchors are at (authoring-time) lines 396, 403, 414, 419, 781, 872, 874, 882, 884, 994, 998, 1034. If `cargo-mutants` is not installed the local guard is skipped, but **CI enforces it regardless**, so re-anchor either way.

### Re-anchor procedure (referenced by Task 1 and Task 2)

Run after the code edits in a `scan.rs`-touching task, before committing:

```bash
# 1. Regenerate the live mutant list and run the guard.
cargo mutants --no-config --list --json > /tmp/musefs-mutants.json
python3 scripts/check_mutant_anchors.py --mutants-json /tmp/musefs-mutants.json
```

For each reported failure (`[mutants.toml:N] /…scan\.rs:LINE:COL:/ — … line likely shifted …`):
- Read that entry's `# guard: op="OP" fn="FN" rows=R` comment in `.cargo/mutants.toml`.
- Find the mutant's new coordinate in the plain list:

```bash
cargo mutants --no-config --list | grep -F 'in FN' | grep -F 'OP'
```

- Update the `musefs-core/src/scan\.rs:LINE:COL:` coordinate in `.cargo/mutants.toml` to the new `LINE:COL` (keep the rest of the regex, including any `replace + with -` description suffix).
- Re-run the guard. Repeat until it prints `OK: N exclude_re entries validated against M mutants.`

If a guard entry now matches **zero** mutants because the mutated expression was *removed* (not just moved), that exclusion is obsolete — delete the entry and its `# guard:` comment. (None of the planned edits remove an anchored expression — the removed `skipped` atomic is not anchored — but verify.)

---

## File map

- **Modify** `musefs-core/src/scan.rs` — the `ProbeOutcome` enum, the walk (`collect_audio`/`collect_audio_inner`/`descend`), a new `push_file` helper, `run_pipeline`, `scan_directory_with`, `scan_directory_full_oracle`, and three in-module tests.
- **Modify** `musefs-core/tests/scan_counters.rs` — update `oracle_counts_scanned_and_skipped_exactly` to the new contract; add four symlink/skip tests.
- **Modify** `.cargo/mutants.toml` — re-anchor drifted coordinates (Tasks 1 and 2).
- **Modify** `ARCHITECTURE.md`, `CHANGELOG.md` — document the dedup and the operator-visible counter shift.

No new files. No public API signature change except `collect_audio` (private) gaining a `u64` return.

### A subtlety for #302 testing

A symlink to an already-visited *directory* is already caught by the existing `dir_key` cycle guard, so "symlinked directory reaching the same file" mostly does not double-count today. The genuinely-buggy case is a **file** reachable via a real path and a symlink (same dir or a sibling dir), because file symlinks bypass the directory guard. The tests cover both; a comment notes which path each exercises.

---

## Task 1: Reclassify scan counters to the documented contract (#301)

Rename the probe outcome, move `skipped` to the collection phase (unsupported extensions), and map unparseable supported files to `failed` — across `run_pipeline`, both scan entry points, and the oracle. This flips behavior, so it updates the value-asserting tests and re-anchors `.cargo/mutants.toml` in the same commit.

**Files:** `musefs-core/src/scan.rs`, `musefs-core/tests/scan_counters.rs`, `.cargo/mutants.toml`

- [ ] **Step 1: Update the in-module counter test to the new contract (failing test)**

In `scan.rs`'s `hardening_tests` module, replace `scan_directory_counts_scanned_and_skipped` (Serena `replace_symbol_body` on `hardening_tests/scan_directory_counts_scanned_and_skipped`; **re-include the `#[test]` attribute** — `replace_symbol_body` drops leading attributes):

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

In `musefs-core/tests/scan_counters.rs`, replace `oracle_counts_scanned_and_skipped_exactly` and the comment block immediately above it (the `// === Full-probe oracle counters …` divider, the doc comment, and the `// kills scan L…` line). The replacement comment intentionally references the *expressions* (not scan.rs line numbers, which drift):

```rust
// === Full-probe oracle counters (scan_directory_full_oracle) ===

/// The full-file-probe oracle's `scanned`/`failed`/`skipped` counters must
/// reflect the corpus exactly: one valid FLAC (`scanned`), one extension-only
/// `.flac` of garbage that `is_supported_audio` collects but `probe_full`
/// rejects (`failed`), and one unsupported-extension file dropped at collection
/// (`skipped`). Asserting all three nonzero-and-exact kills the `+=`→`-=`
/// (underflow-panic from 0) and `+=`→`*=` (pinned at 0) mutants on the oracle's
/// `stats.scanned += 1` / `stats.failed += 1` and on collection's `*skipped += 1`.
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

- [ ] **Step 3: Run the two tests to verify they fail**

Run: `cargo test -p musefs-core scan_directory_counts_scanned_failed_and_skipped oracle_counts_scanned_failed_and_skipped_exactly`
Expected: FAIL — old code counts `bad.flac` as `skipped` (so `failed == 0`) and does not count `notes.txt`.

- [ ] **Step 4: Rename the `ProbeOutcome` variant and tighten its doc**

Replace the `ProbeOutcome` enum body (Serena `replace_symbol_body` on `ProbeOutcome`). Note `Unsupported` and `Unparseable` are both 11 chars, so the rename alone does not shift columns:

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

- [ ] **Step 5: Update `probe_file` doc + the `None` arm**

In `probe_file`, change the doc sentence that mentions `Unsupported` and the match tail (Serena `replace_symbol_body` on `probe_file`, preserving the rest of its body):

Doc sentence becomes:

```rust
/// Returns `ProbeOutcome::Unparseable` for a supported-extension file that does
/// not parse (counted as `failed`) and `ProbeOutcome::Raced` if the file
/// changed under us.
```

Match tail becomes:

```rust
    Ok(match probed {
        Some(p) => ProbeOutcome::Probed(p, s1),
        None => ProbeOutcome::Unparseable,
    })
```

- [ ] **Step 6: Update the in-module probe test's variant reference**

In `oversize_unparseable_file_is_skipped_not_read_whole` (Serena `replace_symbol_body`, re-include `#[test]`), change the `matches!` arm:

```rust
        assert!(matches!(
            probe_file(&path, WINDOW).unwrap(),
            ProbeOutcome::Unparseable
        ));
```

- [ ] **Step 7: Thread a `skipped` counter through the walk — `collect_audio`**

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

- [ ] **Step 8: Count unsupported extensions in `collect_audio_inner`**

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

- [ ] **Step 9: Thread `skipped` through `descend`**

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

- [ ] **Step 10: Map `Unparseable → failed` and drop the `skipped` atomic in `run_pipeline`**

Edit `run_pipeline`. Make exactly these four changes (targeted `replace_content` per spot, or a full `replace_symbol_body`):

1. Delete the declaration line `let skipped = Arc::new(AtomicU64::new(0));`.
2. Delete the per-worker clone line `let skipped = Arc::clone(&skipped);` (in the `for _ in 0..jobs` worker setup).
3. Change the worker match arm to:

```rust
                    Ok(ProbeOutcome::Unparseable) => {
                        failed.fetch_add(1, Ordering::Relaxed);
                    }
```

4. Change the final return so `skipped` is sourced by the caller, not the pipeline:

```rust
    Ok(ScanStats {
        scanned,
        skipped: 0,
        failed: failed.load(Ordering::Relaxed),
        raced: raced.load(Ordering::Relaxed),
    })
```

- [ ] **Step 11: Fill `skipped` from collection in `scan_directory_with`**

Replace `scan_directory_with` (Serena `replace_symbol_body`; keep its doc comment — re-include it):

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

- [ ] **Step 12: Reclassify the oracle (`scan_directory_full_oracle`) in lockstep**

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

`revalidate_with` needs **no edit** — its `collect_audio(...)?;` statement already discards the now-`u64` return (no `#[must_use]`, so no clippy warning under `-D warnings`), and reclassifying `Unparseable → failed` flows through `run_pipeline` automatically.

- [ ] **Step 13: Add a test for the symlink-target `skipped` site (failing test)**

There are now **two** `*skipped += 1` sites in `collect_audio_inner` (the regular-file arm and the symlink-to-file arm). The in-module test covers the regular arm; add a test in `musefs-core/tests/scan_counters.rs` covering the symlink arm so its mutant is killable:

```rust
/// Under follow, a symlink whose target has an unsupported extension counts as
/// skipped, symmetric with a regular unsupported file. Covers the symlink-arm
/// `*skipped += 1` site.
#[test]
fn follow_symlinks_counts_unsupported_symlink_target_as_skipped() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let txt = dir.path().join("notes.txt");
    std::fs::write(&txt, b"hi").unwrap();
    symlink(&txt, dir.path().join("link.txt")).unwrap();
    std::fs::write(dir.path().join("song.flac"), flac_minimal(b"AUDIO")).unwrap();

    let db = Db::open_in_memory().unwrap();
    let opts = ScanOptions {
        follow_symlinks: true,
        ..Default::default()
    };
    let stats = scan_directory_with(&db, dir.path(), &opts).unwrap();

    assert_eq!(stats.scanned, 1);
    // notes.txt (regular) + link.txt (symlink target) → both skipped.
    assert_eq!(stats.skipped, 2);
}
```

- [ ] **Step 14: Build, lint, run the full crate suite**

Run: `cargo test -p musefs-core && cargo clippy -p musefs-core --all-targets`
Expected: PASS, no warnings. The three new/updated tests pass; the tolerant `assert_eq!(stats.skipped + stats.failed, 1)` test (zero-byte `bad.flac`) still passes (now `failed == 1`); the single-file `skipped == 0` tests and the existing `collect_audio_*` walk tests (scan.rs, ~1455-1588) stay green — the `Result<u64>` change is transparent to their `.unwrap()`.

- [ ] **Step 15: Re-anchor `.cargo/mutants.toml`**

Follow the **Re-anchor procedure** above. Every `scan.rs` anchor shifted (collection grew above the probe-internal anchors at 396/403/414/419; `run_pipeline` lost 2 lines under its 781/872/874/882/884 anchors; the oracle/`scan_directory_with` growth pushed the `revalidate_with` anchors 994/998/1034 down). The removed `skipped` atomic is **not** anchored, so no exclusion becomes obsolete. Re-run the guard until `OK`.

- [ ] **Step 16: Commit**

```bash
git add musefs-core/src/scan.rs musefs-core/tests/scan_counters.rs .cargo/mutants.toml
git commit -m "fix(scan): count unsupported extensions as skipped, malformed as failed (#301)

skipped now tallies unsupported-extension files at collection; a
supported-extension file that will not parse counts as failed, matching the
documented ScanStats contract. ProbeOutcome::Unsupported is renamed Unparseable
and the full-probe oracle is reclassified in lockstep so the bounded-vs-full
equivalence property holds. Mutant anchors re-coordinated for the line shifts.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: Deduplicate canonical backing targets under `--follow-symlinks` (#302)

Add a file-level `(dev, ino)` visited set so a file reached via both a real path and a symlink (or two symlink paths) is collected once. Active only when `follow_symlinks` is true; off-mode behavior (including hardlinks) is unchanged.

**Files:** `musefs-core/src/scan.rs`, `musefs-core/tests/scan_counters.rs`, `.cargo/mutants.toml`

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
/// same file by two paths but ingests once. (Handled by the existing
/// directory-cycle guard; this locks in the combined behavior.)
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

- [ ] **Step 4: Route file pushes through `push_file` and thread `files_visited` in `collect_audio_inner`**

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
Expected: PASS, no warnings. All four #302-area tests pass.

- [ ] **Step 8: Re-anchor `.cargo/mutants.toml`**

Follow the **Re-anchor procedure**. `push_file` inserted after `dir_key` shifts every anchor below it; re-coordinate them. The new `push_file`/dedup code is covered by the dedup tests (a mutant that disables dedup makes `scanned == 2`, killed), so no new exclusions are expected — but if the final in-diff gate (Task 4) surfaces a survivor here, add a documented exclusion then.

- [ ] **Step 9: Commit**

```bash
git add musefs-core/src/scan.rs musefs-core/tests/scan_counters.rs .cargo/mutants.toml
git commit -m "fix(scan): dedup canonical backing targets under --follow-symlinks (#302)

A file-level (dev, ino) visited set, active only when following symlinks,
collapses a real file and a symlink to it (or a file reached via two paths)
into one collected candidate, so scanned counts distinct tracks. Off-mode
behavior is unchanged. Mutant anchors re-coordinated for the line shifts.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: Documentation + release note

**Files:** `ARCHITECTURE.md`, `CHANGELOG.md` (no `scan.rs` → no anchor guard)

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

Under `## [Unreleased]`, add a `### Fixed` section after the existing `### Added`:

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

## Task 4: Full verification + in-diff mutation gate

- [ ] **Step 1: Workspace fmt + clippy + tests**

Run: `cargo fmt --all --check && cargo clippy --all-targets --workspace -- -D warnings && cargo test --workspace`
Expected: all PASS. This includes `musefs-core/tests/probe_equivalence.rs`, whose corpus (`common::corpus::generate`) is all valid audio of each format — no unsupported extensions or garbage — so it compares DB rows only and is unaffected by the reclassification; confirm it stays green. (Per repo memory: the `rtk` hook may summarize `cargo test` to `cargo test: N passed`; trust the exit code, not a grep for "test result".)

- [ ] **Step 2: metrics-feature tests (CI's `check` job runs these; local `--workspace` skips them)**

Run: `cargo test -p musefs-core --features metrics`
Expected: PASS. This change touches `collect_audio`/`run_pipeline` but not the getattr/read stat counters; confirm rather than assume.

- [ ] **Step 3: Confirm anchor guard is green on the final tree**

Run: `cargo mutants --no-config --list --json > /tmp/musefs-mutants.json && python3 scripts/check_mutant_anchors.py --mutants-json /tmp/musefs-mutants.json`
Expected: `OK: N exclude_re entries validated against M mutants.` If any anchor still drifts, finish re-anchoring (the Tasks 1/2 re-anchor steps were incomplete).

- [ ] **Step 4: Run the in-diff mutation gate over the branch diff**

Per repo memory (copy-mode OOMs on worktree targets — use in-place, serial):

Run: `cargo mutants --in-place --in-diff <(git diff main...HEAD -- musefs-core/src/scan.rs) -j2`
Expected: no surviving/missed mutants in the diff. Sanity-check the diff is non-empty before trusting a pass (an empty diff is a silent false pass). New killable sites to expect covered: oracle `stats.scanned += 1` / `stats.failed += 1` (oracle test), collection `*skipped += 1` ×2 (the `scan_directory_counts_*` and `follow_symlinks_counts_unsupported_symlink_target_*` tests), and the `push_file` dedup branch (the `follow_symlinks_dedups_*` tests). If a survivor appears, add a test that kills it (preferred) or a documented `exclude_re` with a `# guard:` tag in `.cargo/mutants.toml` — then re-run the anchor guard and amend the relevant commit.

---

## Self-review notes (for the executor)

- **`replace_symbol_body` drops leading attributes/doc comments** — every replaced symbol re-includes its `#[test]` / `#[doc(hidden)]` / doc comment in the body. Do not strip them.
- **Signatures change twice** (`collect_audio` return type and `*_inner`/`descend` params in Task 1; a second param added in Task 2). Mutually-recursive callers must all update together — the crate compiles only at each task's "build + test" step, not between individual symbol edits.
- **Each `scan.rs` commit re-anchors `.cargo/mutants.toml`.** The pre-commit anchor guard (when cargo-mutants is installed) will otherwise reject the commit; CI rejects it regardless.
- **`revalidate_with` is intentionally untouched** — verify with `cargo test -p musefs-core revalidate` after Task 1 that its tests stay green.
- **The `// kills scan L…` comments in `scan_counters.rs` are descriptive only** — they are not the mutation gate (`.cargo/mutants.toml` is) and were already drifted in the baseline. The Task 1 rewrite replaces the oracle one with an expression-referencing comment so it stops chasing line numbers; do not reintroduce hard-coded scan.rs line numbers in test comments.
- **Type/name consistency:** the variant is `ProbeOutcome::Unparseable` everywhere; the helper is `push_file`; the two visited sets are `visited` (directories) and `files_visited` (files); `collect_audio` returns `std::io::Result<u64>`.
