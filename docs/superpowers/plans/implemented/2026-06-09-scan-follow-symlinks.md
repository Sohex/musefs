# Scan: follow symlinks (opt-in, cycle-guarded) — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an opt-in `--follow-symlinks` flag so the scanner can traverse symlinked audio files and directories (with a cycle guard), and make the default-off path log a diagnostic instead of silently dropping symlinks.

**Architecture:** The fix lives entirely in the collection phase, `collect_audio` (`musefs-core/src/scan.rs`). `collect_audio` is split into a thin entry point that owns a `HashSet<(dev, ino)>` visited set and a recursive inner walker. A new `ftype.is_symlink()` arm logs-and-skips when the flag is off and resolves-and-recurses (cycle-guarded) when on. The flag rides on the existing `ScanOptions` struct, so both `scan_directory_with` and `revalidate_with` honor it; the CLI plumbs a clap flag into `ScanOptions`.

**Tech Stack:** Rust, `std::fs` (`read_dir`/`metadata`/`symlink`), `std::os::unix::fs::MetadataExt` (Unix-only — musefs is a FUSE filesystem), `clap` derive, `log`, `tempfile` (test-only).

**Spec:** `docs/superpowers/specs/2026-06-09-scan-follow-symlinks-design.md`

---

## Critical constraint: every commit must be green

The pre-commit hook runs `cargo fmt`, `cargo clippy --all-targets -D warnings`, and the **full workspace test suite**. A commit with red tests or clippy warnings is rejected. Therefore:

- Within each task, write the test(s) and the implementation, confirm green locally, *then* commit. Never commit a red tree.
- Changing `collect_audio`'s arity breaks every caller at compile time, so Task 2 updates the signature **and** all callers **and** the existing test caller in one commit — that is the smallest compilable unit.
- Run the full check before each commit: `cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test`.

## File structure

| File | Responsibility | Change |
| ---- | -------------- | ------ |
| `musefs-core/src/scan.rs` | `ScanOptions` flag + `Default`; `collect_audio` split + symlink arm + cycle guard; update 3 callers + 1 test caller; new `hardening_tests` cases | Modify |
| `musefs-cli/src/lib.rs` | `Command::Scan` clap flag; `run_scan` param; `run` dispatch; `ScanOptions` literal; parse tests | Modify |
| `musefs-cli/tests/scan.rs` | Update 3 positional `run_scan` callers | Modify |
| `ARCHITECTURE.md` | Document symlink scanning behavior + flag | Modify |
| `README.md` | Document `--follow-symlinks` flag | Modify |

Note: all `ScanOptions { … }` literals in the workspace use `..Default::default()` (verified across `musefs-core/tests/*` and `musefs-cli/src/lib.rs`), so adding a struct field does not break them. The only positional break is `run_scan`'s three callers in `musefs-cli/tests/scan.rs` (Task 3).

---

## Task 1: Add `follow_symlinks` to `ScanOptions`

**Files:**
- Modify: `musefs-core/src/scan.rs` — `ScanOptions` struct (`scan.rs:378-386`) and `impl Default for ScanOptions` (`scan.rs:388-396`)
- Test: `musefs-core/src/scan.rs` — `hardening_tests` module (`scan.rs:1203+`)

- [ ] **Step 1: Write the failing test**

In the `hardening_tests` module (after the existing `collect_audio_skips_unsupported_files` test), add:

```rust
    #[test]
    fn scan_options_default_does_not_follow_symlinks() {
        assert!(!ScanOptions::default().follow_symlinks);
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p musefs-core scan_options_default_does_not_follow_symlinks`
Expected: FAIL — compile error `no field 'follow_symlinks' on type 'ScanOptions'`.

- [ ] **Step 3: Add the field and default**

Edit the struct (use Serena `replace_symbol_body` on `ScanOptions`) to:

```rust
/// Knobs for a scan. `jobs == 0` means "use available parallelism".
#[derive(Debug, Clone)]
pub struct ScanOptions {
    pub jobs: usize,
    /// Initial probe read window in bytes; widened on `NeedMore`.
    pub window: usize,
    /// In-flight art-byte budget and per-batch byte-flush threshold.
    pub batch_bytes: u64,
    /// Follow symlinks during collection. Off by default: symlinks are logged
    /// and skipped, which keeps the walk immune to directory-symlink cycles.
    pub follow_symlinks: bool,
}
```

Edit `impl Default for ScanOptions` (use Serena `replace_symbol_body` on `impl Default for ScanOptions`) to:

```rust
impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            jobs: 0,
            window: WINDOW,
            batch_bytes: BATCH_BYTES,
            follow_symlinks: false,
        }
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p musefs-core scan_options_default_does_not_follow_symlinks`
Expected: PASS.

- [ ] **Step 5: Full check + commit**

```bash
cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test
git add musefs-core/src/scan.rs
git commit -m "$(cat <<'EOF'
feat(core): add follow_symlinks to ScanOptions (#189)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Follow symlinks in `collect_audio` (core mechanism)

This task changes `collect_audio`'s signature, so it updates **all four call sites** (3 production + 1 existing test) and adds the new tests in the same commit — that is the smallest compilable unit.

**Files:**
- Modify: `musefs-core/src/scan.rs`
  - `collect_audio` (`scan.rs:76-88`) — split into entry + inner, add `descend` + `dir_key` helpers, add symlink arm
  - `scan_directory_with` call site (`scan.rs:612`)
  - `revalidate_with` call site (`scan.rs:823`)
  - `scan_directory_full_oracle` call site (`scan.rs:781`)
  - existing test caller `hardening_tests::collect_audio_skips_unsupported_files` (`scan.rs:1235`)
- Test: `musefs-core/src/scan.rs` — `hardening_tests` module (new cases)

- [ ] **Step 1: Write the failing tests**

In the `hardening_tests` module, add these five tests:

```rust
    #[test]
    fn collect_audio_follows_symlinked_file_when_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.flac");
        std::fs::write(&real, b"x").unwrap();
        let lib = dir.path().join("lib");
        std::fs::create_dir(&lib).unwrap();
        std::os::unix::fs::symlink(&real, lib.join("link.flac")).unwrap();

        let mut on = Vec::new();
        collect_audio(&lib, &mut on, true).unwrap();
        assert_eq!(on.len(), 1, "symlinked file should be collected when following");

        let mut off = Vec::new();
        collect_audio(&lib, &mut off, false).unwrap();
        assert!(off.is_empty(), "symlinked file should be skipped by default");
    }

    #[test]
    fn collect_audio_follows_symlinked_dir_when_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let real_dir = dir.path().join("music");
        std::fs::create_dir(&real_dir).unwrap();
        std::fs::write(real_dir.join("song.flac"), b"x").unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir(&root).unwrap();
        std::os::unix::fs::symlink(&real_dir, root.join("linkdir")).unwrap();

        let mut on = Vec::new();
        collect_audio(&root, &mut on, true).unwrap();
        assert_eq!(on.len(), 1, "files under a symlinked dir should be collected");

        let mut off = Vec::new();
        collect_audio(&root, &mut off, false).unwrap();
        assert!(off.is_empty(), "symlinked dir should be skipped by default");
    }

    #[test]
    fn collect_audio_terminates_on_symlink_cycle() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        std::fs::create_dir(&a).unwrap();
        std::fs::write(a.join("song.flac"), b"x").unwrap();
        // a/loop -> the root, which contains a, which contains loop, ...
        std::os::unix::fs::symlink(dir.path(), a.join("loop")).unwrap();

        let mut out = Vec::new();
        collect_audio(dir.path(), &mut out, true).unwrap(); // must return, not hang
        assert_eq!(
            out.iter().filter(|p| p.ends_with("song.flac")).count(),
            1,
            "each real file collected at most once despite the cycle"
        );
    }

    #[test]
    fn collect_audio_skips_broken_symlink_when_following() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("real.flac"), b"x").unwrap();
        std::os::unix::fs::symlink(
            dir.path().join("nonexistent"),
            dir.path().join("dangling"),
        )
        .unwrap();

        let mut out = Vec::new();
        let result = collect_audio(dir.path(), &mut out, true);
        assert!(result.is_ok(), "a dangling symlink must not abort collection");
        assert_eq!(out.len(), 1);
        assert!(out[0].ends_with("real.flac"));
    }

    #[test]
    fn collect_audio_does_not_follow_symlinks_by_default() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("real.flac"), b"x").unwrap();
        let other = dir.path().join("other.flac");
        std::fs::write(&other, b"x").unwrap();
        std::os::unix::fs::symlink(&other, dir.path().join("link.flac")).unwrap();

        let mut out = Vec::new();
        collect_audio(dir.path(), &mut out, false).unwrap();
        // Both real files collected; the symlink is skipped (not double-counted).
        assert_eq!(out.len(), 2);
    }
```

Also update the existing test `collect_audio_skips_unsupported_files` (`scan.rs:1235`) — change its call from `collect_audio(dir.path(), &mut out).unwrap();` to:

```rust
        collect_audio(dir.path(), &mut out, false).unwrap();
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p musefs-core collect_audio`
Expected: FAIL — compile error: `collect_audio` takes 2 arguments but 3 were supplied (the whole crate fails to compile until Step 3 is complete).

- [ ] **Step 3: Rewrite `collect_audio` and update all call sites**

Replace `collect_audio` (`scan.rs:76-88`) with the entry point plus two helpers. Use Serena `replace_symbol_body` on `collect_audio` for the entry body, then `insert_after_symbol` to add `collect_audio_inner`, `descend`, and `dir_key`. Add `use std::collections::HashSet;` and `use std::os::unix::fs::MetadataExt;` at the top of the file (alongside the existing `use std::collections::HashMap;` at `scan.rs:1`).

```rust
fn collect_audio(root: &Path, out: &mut Vec<PathBuf>, follow_symlinks: bool) -> std::io::Result<()> {
    let mut visited = HashSet::new();
    if follow_symlinks {
        // Seed with the root's identity so a symlink pointing back to it is
        // caught as a cycle on the first descent.
        if let Ok(meta) = std::fs::metadata(root) {
            visited.insert(dir_key(&meta));
        }
    }
    collect_audio_inner(root, out, follow_symlinks, &mut visited)
}

fn collect_audio_inner(
    root: &Path,
    out: &mut Vec<PathBuf>,
    follow_symlinks: bool,
    visited: &mut HashSet<(u64, u64)>,
) -> std::io::Result<()> {
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        let ftype = entry.file_type()?;
        if ftype.is_dir() {
            descend(&path, out, follow_symlinks, visited)?;
        } else if ftype.is_file() {
            if is_supported_audio(&path) {
                out.push(path);
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
                Ok(meta) if meta.is_dir() => descend(&path, out, follow_symlinks, visited)?,
                Ok(meta) if meta.is_file() => {
                    if is_supported_audio(&path) {
                        out.push(path);
                    }
                }
                Ok(_) => {} // target is neither file nor directory — ignore
                Err(e) => {
                    log::warn!("skipping broken symlink {}: {e}", path.display());
                }
            }
        }
    }
    Ok(())
}

/// Recurse into a directory `path`. When following symlinks, guard against
/// cycles by recording every entered directory's `(dev, ino)`; otherwise recurse
/// directly (the walk is cycle-immune because nothing is followed).
fn descend(
    path: &Path,
    out: &mut Vec<PathBuf>,
    follow_symlinks: bool,
    visited: &mut HashSet<(u64, u64)>,
) -> std::io::Result<()> {
    if !follow_symlinks {
        return collect_audio_inner(path, out, follow_symlinks, visited);
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
    collect_audio_inner(path, out, follow_symlinks, visited)
}

/// `(st_dev, st_ino)` identity of a path, used to break symlink cycles.
fn dir_key(meta: &std::fs::Metadata) -> (u64, u64) {
    (meta.dev(), meta.ino())
}
```

Now update the three production call sites:

- `scan_directory_with` (`scan.rs:612`): change `collect_audio(root, &mut files)?;` to
  ```rust
        collect_audio(root, &mut files, opts.follow_symlinks)?;
  ```
- `revalidate_with` (`scan.rs:823`): change `collect_audio(root, &mut files)?;` to
  ```rust
        collect_audio(root, &mut files, opts.follow_symlinks)?;
  ```
- `scan_directory_full_oracle` (`scan.rs:781`): change `collect_audio(root, &mut files)?;` to
  ```rust
        collect_audio(root, &mut files, false)?;
  ```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p musefs-core collect_audio`
Expected: PASS — all six `collect_audio_*` tests green.

- [ ] **Step 5: Full check + commit**

```bash
cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test
git add musefs-core/src/scan.rs
git commit -m "$(cat <<'EOF'
feat(core): follow symlinks during scan collection (#189)

collect_audio now logs-and-skips symlinks by default (no longer silent)
and, with follow_symlinks on, resolves them with a (dev, ino) cycle
guard. Both scan and revalidate honor the flag via ScanOptions.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: CLI `--follow-symlinks` flag

**Files:**
- Modify: `musefs-cli/src/lib.rs`
  - `Command::Scan` variant (`lib.rs:93-111`)
  - `run_scan` signature + `ScanOptions` literal (`lib.rs:122-134`)
  - `run` dispatch (`lib.rs:203-209`)
- Modify: `musefs-cli/tests/scan.rs` — three positional `run_scan` callers (`scan.rs:57`, `:84`, `:114`)
- Test: `musefs-cli/src/lib.rs` — `tests` module (`lib.rs:214+`)

- [ ] **Step 1: Write the failing tests**

In the `tests` module of `musefs-cli/src/lib.rs` (alongside `scan_command_parses_jobs_flag`), add:

```rust
    #[test]
    fn scan_command_parses_follow_symlinks_flag() {
        use clap::Parser;
        let cli = Cli::try_parse_from([
            "musefs",
            "scan",
            "/m",
            "--db",
            "/tmp/x.db",
            "--follow-symlinks",
        ])
        .unwrap();
        match cli.command {
            Command::Scan { follow_symlinks, .. } => assert!(follow_symlinks),
            _ => panic!("expected scan command"),
        }
    }

    #[test]
    fn scan_command_follow_symlinks_defaults_off() {
        use clap::Parser;
        let cli =
            Cli::try_parse_from(["musefs", "scan", "/m", "--db", "/tmp/x.db"]).unwrap();
        match cli.command {
            Command::Scan { follow_symlinks, .. } => assert!(!follow_symlinks),
            _ => panic!("expected scan command"),
        }
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p musefs-cli scan_command_parses_follow_symlinks_flag scan_command_follow_symlinks_defaults_off`
Expected: FAIL — compile error: no field `follow_symlinks` on the `Scan` variant.

- [ ] **Step 3: Add the flag and plumb it through**

Add the field to the `Command::Scan` variant (after the `quiet` field at `lib.rs:110`, before the closing `}`):

```rust
        /// Follow symlinks while walking directories. Off by default: symlinked
        /// files and directories are logged and skipped.
        #[arg(long)]
        follow_symlinks: bool,
```

Change the `run_scan` signature (`lib.rs:122-128`) to insert `follow_symlinks` before `quiet`:

```rust
pub fn run_scan(
    db_path: &Path,
    targets: &[PathBuf],
    revalidate: bool,
    jobs: usize,
    follow_symlinks: bool,
    quiet: bool,
) -> Result<()> {
```

Change the `ScanOptions` literal (`lib.rs:131-134`) to:

```rust
    let opts = musefs_core::ScanOptions {
        jobs,
        follow_symlinks,
        ..Default::default()
    };
```

Change the `run` dispatch (`lib.rs:203-209`) to:

```rust
        Command::Scan {
            targets,
            db,
            revalidate,
            jobs,
            quiet,
            follow_symlinks,
        } => run_scan(&db, &targets, revalidate, jobs, follow_symlinks, quiet),
```

Update the three `run_scan` callers in `musefs-cli/tests/scan.rs` to pass `false` for the new `follow_symlinks` parameter (it sits between `jobs` and `quiet`):

- `scan.rs:57`:
  ```rust
      run_scan(&db_path, &[backing.path().to_path_buf()], false, 0, false, false).unwrap();
  ```
- `scan.rs:84-93` (the multi-path call) — change the trailing args so the call ends:
  ```rust
          false,
          0,
          false,
          false,
      )
  ```
- `scan.rs:114-120` (the missing-path call):
  ```rust
      let result = run_scan(
          &db_path,
          &[backing.path().to_path_buf(), missing],
          false,
          0,
          false,
          false,
      );
  ```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p musefs-cli`
Expected: PASS — both new parse tests green and the existing `tests/scan.rs` integration tests still pass.

- [ ] **Step 5: Full check + commit**

```bash
cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test
git add musefs-cli/src/lib.rs musefs-cli/tests/scan.rs
git commit -m "$(cat <<'EOF'
feat(cli): add --follow-symlinks to scan (#189)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: Documentation

No tests — documentation only. Verify the wording matches the implemented behavior, then commit.

**Files:**
- Modify: `ARCHITECTURE.md` — Scanning section (`ARCHITECTURE.md:232-247`)
- Modify: `README.md` — scan usage prose (`README.md:50-54`)

- [ ] **Step 1: Update ARCHITECTURE.md**

In the `## Scanning` section, insert a new paragraph after the `scan_directory` paragraph (after `ARCHITECTURE.md:240`, before the `revalidate` paragraph):

```markdown
Symlinks are **not followed by default**: a symlinked file or directory is
logged (`RUST_LOG=info`/`warn`) and skipped, which keeps the walk immune to
directory-symlink cycles. Passing `--follow-symlinks` resolves them — symlinked
audio files and directories are scanned — guarded by a visited `(dev, ino)` set
so symlink cycles terminate. Broken symlinks are logged and skipped without
aborting the scan. The `root` argument is always followed regardless of the
flag; only links encountered during recursion are gated.
```

- [ ] **Step 2: Update README.md**

In the scan prose, change the `--jobs N` sentence (`README.md:51-52`) so it reads:

```markdown
files or directories, and `--jobs N` controls probe parallelism.
`--follow-symlinks` walks symlinked files and directories (off by default, so
symlinks are logged and skipped). `--quiet`
```

(The sentence continues with the existing `(\`-q\`) suppresses the per-target summary …` text — leave that intact.)

- [ ] **Step 3: Verify docs build/render**

Run: `grep -n "follow-symlinks" ARCHITECTURE.md README.md`
Expected: matches in both files; read the surrounding prose to confirm it flows.

- [ ] **Step 4: Full check + commit**

```bash
cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test
git add ARCHITECTURE.md README.md
git commit -m "$(cat <<'EOF'
docs: document --follow-symlinks scan behavior (#189)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Final verification

- [ ] Run the full workspace suite once more: `cargo test`
- [ ] Confirm clippy is clean: `cargo clippy --all-targets -- -D warnings`
- [ ] Confirm formatting: `cargo fmt --all --check`
- [ ] Manual smoke (optional): create a temp dir with a symlinked audio file, run `cargo run -p musefs-cli -- scan <dir> --db /tmp/t.db` (symlink skipped, warning logged) then `... --follow-symlinks` (symlink scanned).
- [ ] The `fuzz/` crate is outside the workspace but only format-layer signatures break it; this change touches `musefs-core` collection only, so no `cargo +nightly fuzz build` is needed.
