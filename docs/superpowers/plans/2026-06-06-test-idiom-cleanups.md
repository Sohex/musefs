# Test-Suite Idiom Cleanups Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement the four cleanups from issue #139 per the approved spec at `docs/superpowers/specs/2026-06-06-test-idiom-cleanups-design.md`: inject scan knobs via `ScanOptions` (deleting the `MUSEFS_SCAN_WINDOW`/`MUSEFS_BATCH_BYTES` env vars), consolidate triplicated FLAC fixture helpers into `fuzz_check::fixtures`, stop leaking the bench `TempDir`, and make the `b64` fuzz target's bounds locally underflow-safe.

**Architecture:** Behavior-preserving refactors; the existing test suite is the safety net. Work happens on the `test-idiom-cleanups` branch (already created, carries the spec). One commit per task. The only new test is a `ScanOptions::default()` assertion that preserves mutation-kill coverage for the `WINDOW`/`BATCH_BYTES` constants.

**Tech Stack:** Rust workspace (cargo, clippy, rustfmt), cargo-mutants in-diff gate, cargo-fuzz (nightly, out-of-workspace `fuzz/` crate).

**Out of scope (do not touch):** `common_corpus_smoke.rs`'s `ENV_LOCK` and `MUSEFS_BENCH_*` handling (those tests deliberately test env parsing of the bench-corpus config surface); the *different-signature* `make_flac(comments, audio)`/`flac_block` local variants in `musefs-cli/tests/scan.rs` and `musefs-fuse/tests/{concurrency,mount,keep_cache}.rs`.

---

### Task 1: Inject scan knobs via `ScanOptions`; delete the env vars

**Files:**
- Modify: `musefs-core/src/scan.rs` (struct ~line 368, `scan_window()` ~173, `batch_bytes_cap()` ~382, `probe_file` ~214, `run_pipeline` ~612, `payload_weight` doc ~399, unit tests ~880–995, `jobs1_and_jobs_n` test ~1766)
- Modify: `musefs-cli/src/lib.rs` (`run_scan`, ~line 108)
- Modify: `musefs-core/tests/probe_equivalence.rs` (~lines 60–64)
- Modify: `musefs-core/tests/scan_counters.rs` (ENV_LOCK ~14, three env-using tests, two `jobs: 4` literals)
- Modify: `musefs-core/tests/pipeline_backpressure.rs` (~lines 51–70)
- Modify: `musefs-core/tests/bench_ingest.rs` (three `ScanOptions` literals: ~28, ~130, ~215)
- Modify: `musefs-core/src/lock.rs` (registry comment ~line 20)
- Modify: `musefs-core/tests/common/corpus.rs` (stale comment ~line 100)
- Modify: `musefs-core/tests/metrics.rs` (stale comment ~line 476)
- Modify: `.cargo/mutants.toml` (three line:col anchors into scan.rs)

- [ ] **Step 1: Extend `ScanOptions` and replace its derived `Default` with a manual impl**

In `musefs-core/src/scan.rs`, replace:

```rust
/// Knobs for a scan. `jobs == 0` means "use available parallelism".
#[derive(Debug, Clone, Default)]
pub struct ScanOptions {
    pub jobs: usize,
}
```

with:

```rust
/// Knobs for a scan. `jobs == 0` means "use available parallelism".
#[derive(Debug, Clone)]
pub struct ScanOptions {
    pub jobs: usize,
    /// Initial probe read window in bytes; widened on `NeedMore`.
    pub window: usize,
    /// In-flight art-byte budget and per-batch byte-flush threshold.
    pub batch_bytes: u64,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            jobs: 0,
            window: WINDOW,
            batch_bytes: BATCH_BYTES,
        }
    }
}
```

(`WINDOW` is `const WINDOW: usize = 1 << 20;` at scan.rs:16, `BATCH_BYTES` is `const BATCH_BYTES: u64 = 64 << 20;` at scan.rs:12 — both already exist above.)

- [ ] **Step 2: Delete the two env-reading functions**

Delete `scan_window()` **including its doc comment** (scan.rs ~lines 172–179):

```rust
/// Effective initial window: `MUSEFS_SCAN_WINDOW` (bytes) if set, else `WINDOW`.
fn scan_window() -> usize {
    std::env::var("MUSEFS_SCAN_WINDOW")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(WINDOW)
}
```

Delete `batch_bytes_cap()` **including its doc comment** (~lines 379–389):

```rust
/// In-flight art-byte budget (and per-batch byte-flush threshold). Overridable via
/// `MUSEFS_BATCH_BYTES` so tests can exercise the backpressure path without 64 MiB
/// of fixture art; defaults to `BATCH_BYTES`.
fn batch_bytes_cap() -> u64 {
    std::env::var("MUSEFS_BATCH_BYTES")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(BATCH_BYTES)
}
```

- [ ] **Step 3: Thread `window` into `probe_file`; read `batch_bytes` from opts**

`probe_file` signature (scan.rs ~214) gains the window parameter:

```rust
fn probe_file(path: &Path, file_len: u64, window: usize) -> std::io::Result<Option<Probed>> {
```

Inside it (~line 249), replace:

```rust
    let mut want = (scan_window() as u64).min(file_len) as usize;
```

with:

```rust
    let mut want = (window as u64).min(file_len) as usize;
```

In `run_pipeline` (~line 616), replace:

```rust
    let jobs = effective_jobs(opts.jobs);
    let cap = batch_bytes_cap();
```

with:

```rust
    let jobs = effective_jobs(opts.jobs);
    let window = opts.window;
    let cap = opts.batch_bytes;
```

(`window` is a `Copy` `usize`, so each `move` worker closure captures its own copy; no `Arc` needed.) Then update the single call site inside the worker loop (~line 640):

```rust
            match probe_file(&path, meta.len(), window) {
```

- [ ] **Step 4: Fix the `payload_weight` doc comment**

It references the deleted env var (~line 399). Change:

```rust
/// In-memory byte weight of a `Probed`, used for batch backpressure
/// (`MUSEFS_BATCH_BYTES`). Counts every buffered payload — pictures plus FLAC
```

to:

```rust
/// In-memory byte weight of a `Probed`, used for batch backpressure
/// (`ScanOptions::batch_bytes`). Counts every buffered payload — pictures plus FLAC
```

- [ ] **Step 5: Update the `scan.rs` unit tests**

In `mod scan_unit_tests` (~line 881):

a. Remove `use std::sync::Mutex;` from the module imports (only `ENV_LOCK` used it).

b. Delete the `ENV_LOCK` static and its doc comment:

```rust
    /// Env is process-global: serialize the env-mutating tests so they never
    /// observe each other's `MUSEFS_*` vars.
    static ENV_LOCK: Mutex<()> = Mutex::new(());
```

c. Delete two **non-adjacent** blocks (the `read_tail_128` tests, `write_temp`, and `effective_jobs` test sit between them — leave those untouched):
   - the `// --- scan_window() / WINDOW ... ---` section header, its kills-comments, and the whole `scan_window_default_and_env` test;
   - the `// --- batch_bytes_cap() / BATCH_BYTES ... ---` section header, its kills-comments, and the whole `batch_bytes_cap_default_and_env` test.

d. Where the `scan_window` section header was (before the `read_tail_128` section), add:

```rust
    // --- ScanOptions defaults (WINDOW L16, BATCH_BYTES L12) ---

    // kills the WINDOW `<<`→`>>` and BATCH_BYTES initializer mutants: the
    // right-hand sides are decimal literals, so a mutated const/Default
    // initializer cannot flow to both sides of the assertion.
    #[test]
    fn scan_options_defaults() {
        let d = ScanOptions::default();
        assert_eq!(d.jobs, 0, "jobs default = use available parallelism");
        assert_eq!(d.window, 1_048_576, "window default = 1 MiB");
        assert_eq!(d.batch_bytes, 67_108_864, "batch_bytes default = 64 MiB");
    }
```

**Important:** the decimal-literal right-hand sides are load-bearing — `assert_eq!(d.window, WINDOW)` would let a `1 << 20`→`1 >> 20` const mutant survive the in-diff gate (it flows to both sides).

e. In the `jobs1_and_jobs_n_produce_equivalent_state` test (~line 1766), the field-shorthand literal:

```rust
            scan_directory_with(&db, dir.path(), &ScanOptions { jobs }).unwrap();
```

becomes:

```rust
            scan_directory_with(
                &db,
                dir.path(),
                &ScanOptions {
                    jobs,
                    ..Default::default()
                },
            )
            .unwrap();
```

- [ ] **Step 6: Run the core unit tests**

Run: `cargo test -p musefs-core --lib`
Expected: PASS (including the new `scan_options_defaults`). If anything still references `scan_window`/`batch_bytes_cap`/`ENV_LOCK`, it fails to compile — fix per the steps above.

- [ ] **Step 7: Update the production literal in `musefs-cli`**

In `musefs-cli/src/lib.rs` `run_scan` (~line 108), replace:

```rust
    let opts = musefs_core::ScanOptions { jobs };
```

with:

```rust
    let opts = musefs_core::ScanOptions {
        jobs,
        ..Default::default()
    };
```

Behavior is unchanged: the CLI never set the env vars, so it always got `WINDOW`/`BATCH_BYTES` — exactly what `Default` now supplies.

- [ ] **Step 8: Update `probe_equivalence.rs` (no-options API → `scan_directory_with`)**

Replace (~lines 60–64):

```rust
        // Bounded scan with a 64-byte window → widen path fires on every file.
        std::env::set_var("MUSEFS_SCAN_WINDOW", "64");
        let bounded_db = Db::open_in_memory().unwrap();
        musefs_core::scan_directory(&bounded_db, dir.path()).unwrap();
        std::env::remove_var("MUSEFS_SCAN_WINDOW");
```

with:

```rust
        // Bounded scan with a 64-byte window → widen path fires on every file.
        let bounded_db = Db::open_in_memory().unwrap();
        musefs_core::scan_directory_with(
            &bounded_db,
            dir.path(),
            &musefs_core::ScanOptions {
                window: 64,
                ..Default::default()
            },
        )
        .unwrap();
```

(Keep using fully-qualified `musefs_core::` paths — the file's other scan calls are written that way.)

- [ ] **Step 9: Update `scan_counters.rs`**

a. Delete the lock (~lines 13–14):

```rust
/// Serialize env-mutating tests: `MUSEFS_*` vars are process-global.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
```

b. `widen_then_fallback_matches_oracle_under_tiny_window`: delete `let _g = ENV_LOCK.lock().unwrap();` and replace:

```rust
    std::env::set_var("MUSEFS_SCAN_WINDOW", "64");
    let bounded_db = Db::open_in_memory().unwrap();
    let stats = scan_directory(&bounded_db, dir.path()).unwrap();
    std::env::remove_var("MUSEFS_SCAN_WINDOW");
```

with:

```rust
    let bounded_db = Db::open_in_memory().unwrap();
    let stats = scan_directory_with(
        &bounded_db,
        dir.path(),
        &ScanOptions {
            window: 64,
            ..Default::default()
        },
    )
    .unwrap();
```

c. `widen_preserves_art_bytes_vs_oracle`: delete `let _g = ENV_LOCK.lock().unwrap();` and replace:

```rust
    // Tiny window forces a multi-step widen to reach the 4 KiB picture body.
    std::env::set_var("MUSEFS_SCAN_WINDOW", "16");
    let db = Db::open_in_memory().unwrap();
    scan_directory(&db, dir.path()).unwrap();
    std::env::remove_var("MUSEFS_SCAN_WINDOW");
```

with:

```rust
    // Tiny window forces a multi-step widen to reach the 4 KiB picture body.
    let db = Db::open_in_memory().unwrap();
    scan_directory_with(
        &db,
        dir.path(),
        &ScanOptions {
            window: 16,
            ..Default::default()
        },
    )
    .unwrap();
```

d. `scans_more_than_batch_files_persists_all_once`: both occurrences of

```rust
    let stats = scan_directory_with(&db, dir.path(), &ScanOptions { jobs: 4 }).unwrap();
```

(the second is `let stats2 = ...`) become:

```rust
    let stats = scan_directory_with(
        &db,
        dir.path(),
        &ScanOptions {
            jobs: 4,
            ..Default::default()
        },
    )
    .unwrap();
```

e. `byte_threshold_flush_persists_all_art`: delete `let _g = ENV_LOCK.lock().unwrap();`; in its doc comment change ``/// Byte-threshold flushing: with a tiny `MUSEFS_BATCH_BYTES` and art-bearing`` to ``/// Byte-threshold flushing: with a tiny `batch_bytes` and art-bearing``; and replace:

```rust
    // Cap below a couple files' cumulative art so the byte branch flushes often.
    std::env::set_var("MUSEFS_BATCH_BYTES", "100");
    let db = Db::open_in_memory().unwrap();
    let stats = scan_directory_with(&db, dir.path(), &ScanOptions { jobs: 4 }).unwrap();
    std::env::remove_var("MUSEFS_BATCH_BYTES");
```

with:

```rust
    // Cap below a couple files' cumulative art so the byte branch flushes often.
    let db = Db::open_in_memory().unwrap();
    let stats = scan_directory_with(
        &db,
        dir.path(),
        &ScanOptions {
            jobs: 4,
            batch_bytes: 100,
            ..Default::default()
        },
    )
    .unwrap();
```

(`scan_directory` stays imported — `revalidate_unchanged_count_matches_file_count` and others still call it.)

- [ ] **Step 10: Update `pipeline_backpressure.rs`**

Replace (~lines 48–58, keeping the explanatory comment):

```rust
    // Cap the in-flight budget below two files' cumulative art (6 bytes each), so a
    // second concurrent `acquire` blocks while the writer's batch sits below the
    // flush threshold — the exact pre-fix deadlock window.
    std::env::set_var("MUSEFS_BATCH_BYTES", "8");

    let root = dir.path().to_path_buf();
    let handle = std::thread::spawn(move || {
        let db = Db::open_in_memory().unwrap();
        scan_directory_with(&db, &root, &ScanOptions { jobs: 4 }).unwrap();
        db.list_tracks().unwrap().len()
    });
```

with:

```rust
    // Cap the in-flight budget below two files' cumulative art (6 bytes each), so a
    // second concurrent `acquire` blocks while the writer's batch sits below the
    // flush threshold — the exact pre-fix deadlock window.
    let root = dir.path().to_path_buf();
    let handle = std::thread::spawn(move || {
        let db = Db::open_in_memory().unwrap();
        scan_directory_with(
            &db,
            &root,
            &ScanOptions {
                jobs: 4,
                batch_bytes: 8,
                ..Default::default()
            },
        )
        .unwrap();
        db.list_tracks().unwrap().len()
    });
```

And delete the trailing `std::env::remove_var("MUSEFS_BATCH_BYTES");` (~line 70, after the watchdog loop).

- [ ] **Step 11: Update `bench_ingest.rs` literals**

a. ~line 28: `let opts = ScanOptions { jobs };` →

```rust
    let opts = ScanOptions {
        jobs,
        ..Default::default()
    };
```

b. ~lines 130–135, inside the `scan_directory_with(` call:

```rust
        &ScanOptions {
            jobs: std::env::var("MUSEFS_BENCH_JOBS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0),
        },
```

→ add the rest-spread:

```rust
        &ScanOptions {
            jobs: std::env::var("MUSEFS_BENCH_JOBS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0),
            ..Default::default()
        },
```

c. ~line 215: `&ScanOptions { jobs: 0 }` → `&ScanOptions::default()` (`jobs: 0` is the default).

- [ ] **Step 12: Scrub stale comments**

a. `musefs-core/src/lock.rs` (~line 20):

```rust
//! poison), db_pool.rs (#94), scan.rs ENV_LOCK / work-queue (test/scan-internal,
//! not on the FUSE serving path).
```

→

```rust
//! poison), db_pool.rs (#94), scan.rs work-queue (scan-internal, not on the
//! FUSE serving path).
```

b. `musefs-core/tests/common/corpus.rs` (~line 100): in `CorpusParams::single`'s doc, change ``/// time). The tiny `MUSEFS_SCAN_WINDOW=64` still forces the widen path on`` to ``/// time). The tiny `ScanOptions::window` (64) still forces the widen path on``.

c. `musefs-core/tests/metrics.rs` (~line 476):

```rust
    // Corpus tracks are far below the default 1 MiB scan window (this test must
    // not set MUSEFS_SCAN_WINDOW): one prefix read + the tail. read_tail_128
```

→

```rust
    // Corpus tracks are far below the default 1 MiB scan window (this test must
    // keep the default ScanOptions::window): one prefix read + the tail. read_tail_128
```

- [ ] **Step 13: Re-anchor ALL TWELVE `mutants.toml` `scan.rs` line:col excludes**

`.cargo/mutants.toml` pins **twelve** equivalent-mutant excludes by `scan.rs` line:col, and every one sits below the first deletion (~line 172), so all twelve shift. Re-anchoring only some leaves the rest silently stale: the in-diff gate stays green (those lines aren't in the diff) but the later full mutation leg reports them MISSED.

After `cargo fmt --all`, recompute each anchor's new line by grepping its source content. Columns are unchanged (only whole lines above were added/removed; indentation is untouched):

| Old anchor | Content to grep for | Notes |
|---|---|---|
| `263:31` | `.max(want + 1)` | unique; widen progress in `probe_file` |
| `270:30` | `(prefix.len() as u64) < file_len` | unique; post-loop fallback guard |
| `624:46` | `sync_channel::<Unit>(jobs * 2)` | unique; channel capacity in `run_pipeline` |
| `715:29` | `batch_bytes += unit.weight;` | **first** of 2 matches (try_recv branch) |
| `717:32`, `717:47`, `717:62` | `if batch.len() >= BATCH_FILES \|\| batch_bytes >= cap {` | **first** of 2 matches; three cols on one line |
| `725:37` | `batch_bytes += unit.weight;` | **second** of 2 matches (recv branch, deeper indent) |
| `727:40`, `727:55`, `727:70` | `if batch.len() >= BATCH_FILES \|\| batch_bytes >= cap {` | **second** of 2 matches; three cols on one line |
| `831:25` | `skip_failed += 1;` | **first** of 2 matches (`revalidate_with` stat failure) |
| `835:25` | `skip_failed += 1;` | **second** of 2 matches (canonicalize failure) |
| `871:29` | `failed: scan.failed + skip_failed,` | unique; entry reads `:871:29: replace \+ with -` — keep that suffix |

```bash
grep -nF '.max(want + 1)' musefs-core/src/scan.rs
grep -nF '(prefix.len() as u64) < file_len' musefs-core/src/scan.rs
grep -nF 'sync_channel::<Unit>(jobs * 2)' musefs-core/src/scan.rs
grep -nF 'batch_bytes += unit.weight;' musefs-core/src/scan.rs            # 2 matches: first→715-group, second→725-group
grep -nF 'batch.len() >= BATCH_FILES || batch_bytes >= cap' musefs-core/src/scan.rs  # 2 matches: first→717-group, second→727-group
grep -nF 'skip_failed += 1;' musefs-core/src/scan.rs                      # 2 matches: first→831, second→835
grep -nF 'failed: scan.failed + skip_failed,' musefs-core/src/scan.rs
```

Edit each of the twelve `'musefs-core/src/scan\.rs:<line>:<col>:'` entries in `.cargo/mutants.toml`, substituting the new line numbers and keeping every column (and the `871` entry's ` replace \+ with -` suffix) exactly as is.

Cross-check: if Step 15's gate (or the full mutation leg) reports a MISSED mutant at one of these sites, copy the exact `file:line:col:` from the report into the corresponding entry and re-run.

- [ ] **Step 14: Run the workspace tests**

Run: `cargo test --workspace`
Expected: PASS — and the formerly env-serialized tests now run in parallel.

Run: `cargo fmt --all && cargo clippy --all-targets`
Expected: clean (clippy compiles `bench_ingest.rs` and the benches).

- [ ] **Step 15: Run the in-diff mutation gate**

```bash
git diff "$(git merge-base main HEAD)...HEAD" -- '*.rs' > mutants.diff
grep -q '^@@ ' mutants.diff
cargo mutants --in-diff mutants.diff -j2 --exclude 'musefs-latencyfs/**' --output /tmp/mutants-out/in-diff
```

Expected: exit 0, no MISSED mutants. If a MISSED mutant appears at one of the three re-anchored sites, the line:col in Step 13 is off — copy the exact `file:line:col:` from the report into the corresponding `mutants.toml` entry and re-run.

- [ ] **Step 16: Commit**

```bash
git add musefs-core/src/scan.rs musefs-core/src/lock.rs musefs-cli/src/lib.rs \
  musefs-core/tests/probe_equivalence.rs musefs-core/tests/scan_counters.rs \
  musefs-core/tests/pipeline_backpressure.rs musefs-core/tests/bench_ingest.rs \
  musefs-core/tests/common/corpus.rs musefs-core/tests/metrics.rs .cargo/mutants.toml
git commit -m "$(cat <<'EOF'
Inject scan window/batch-bytes via ScanOptions; drop MUSEFS_* env knobs (#139)

The env vars existed only for tests; tests now inject through the
existing ScanOptions, so the ENV_LOCK serialization (and two unguarded
env mutations) disappear.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 2: Consolidate FLAC fixture helpers into `fuzz_check::fixtures`

**Files:**
- Modify: `musefs-format/Cargo.toml` (self-dev-dependency)
- Modify: `musefs-format/src/fuzz_check.rs` (`fixtures` module, ~lines 57–102)
- Modify: `musefs-format/tests/common/mod.rs` (drop 4 fns, add re-export)
- Modify: `musefs-core/tests/common/mod.rs` (drop 4 fns, add re-export)
- Modify: `CLAUDE.md` (proptest command note)

- [ ] **Step 1: Add the self-dev-dependency**

In `musefs-format/Cargo.toml`, add to `[dev-dependencies]` (path-only, no version — stripped on publish):

```toml
# Self-dependency: turns the `fuzzing` feature on for this crate's own test
# builds, so tests/ can use fuzz_check and the proptests run without an
# explicit --features flag.
musefs-format = { path = ".", features = ["fuzzing"] }
```

- [ ] **Step 2: Rewrite the `fixtures` FLAC helpers in `fuzz_check.rs`**

In `musefs-format/src/fuzz_check.rs`, inside `pub mod fixtures`, replace the three private helpers and `flac()` — i.e. everything from `fn flac_block(` through the end of `pub fn flac(...) { ... }` (currently ~lines 58–102), keeping `bx` and everything after it untouched — with the public, better-commented versions plus the new `make_flac`:

```rust
    /// Build a FLAC metadata block (4-byte header + body) independently of production code.
    pub fn flac_block(block_type: u8, body: &[u8], is_last: bool) -> Vec<u8> {
        let mut out = Vec::new();
        let first = (if is_last { 0x80 } else { 0 }) | (block_type & 0x7F);
        out.push(first);
        let len = body.len();
        out.push(((len >> 16) & 0xFF) as u8);
        out.push(((len >> 8) & 0xFF) as u8);
        out.push((len & 0xFF) as u8);
        out.extend_from_slice(body);
        out
    }

    /// A structurally valid STREAMINFO body: 44100 Hz, 2 channels, 16-bit, unknown frame/sample counts.
    pub fn streaminfo_body() -> Vec<u8> {
        let mut b = vec![
            0x10, 0x00, // min block size = 4096
            0x10, 0x00, // max block size = 4096
            0x00, 0x00, 0x00, // min frame size = 0 (unknown)
            0x00, 0x00, 0x00, // max frame size = 0 (unknown)
            0x0A, 0xC4, 0x42, 0xF0, // sample_rate=44100, channels=2, bps=16, top of total samples
            0x00, 0x00, 0x00, 0x00, // remaining total-samples bits = 0
        ];
        b.extend_from_slice(&[0u8; 16]); // MD5 signature = 0
        b
    }

    /// Minimal VORBIS_COMMENT body with the given already-formatted `KEY=value` comments.
    pub fn vorbis_comment_body(vendor: &str, comments: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
        out.extend_from_slice(vendor.as_bytes());
        out.extend_from_slice(&(comments.len() as u32).to_le_bytes());
        for c in comments {
            out.extend_from_slice(&(c.len() as u32).to_le_bytes());
            out.extend_from_slice(c.as_bytes());
        }
        out
    }

    /// Assemble a full FLAC byte stream: marker + blocks (last-flag auto-set on the final block) + audio.
    pub fn make_flac(blocks: &[(u8, Vec<u8>)], audio: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"fLaC");
        for (i, (bt, body)) in blocks.iter().enumerate() {
            let is_last = i == blocks.len() - 1;
            out.extend_from_slice(&flac_block(*bt, body, is_last));
        }
        out.extend_from_slice(audio);
        out
    }

    /// FLAC = `fLaC` + STREAMINFO + VORBIS_COMMENT + `audio`.
    pub fn flac(audio: &[u8]) -> Vec<u8> {
        make_flac(
            &[
                (0, streaminfo_body()),
                (4, vorbis_comment_body("orig", &["TITLE=Orig"])),
            ],
            audio,
        )
    }
```

(The original `flac()` built STREAMINFO with `is_last=false` and VORBIS_COMMENT with `is_last=true`; `make_flac` auto-sets the last flag on the final block — byte-identical output. The in-file `flac_fixture_parses` unit test confirms.)

- [ ] **Step 3: Replace the four helpers in `musefs-format/tests/common/mod.rs` with a re-export**

Delete `flac_block`, `streaminfo_body`, `vorbis_comment_body`, `make_flac` (lines 7–57) and add after the existing `use musefs_format::{RegionLayout, Segment};`:

```rust
pub use musefs_format::fuzz_check::fixtures::{
    flac_block, make_flac, streaminfo_body, vorbis_comment_body,
};
```

`resolve_layout` stays unchanged.

- [ ] **Step 4: Replace the four helpers in `musefs-core/tests/common/mod.rs` with the same re-export**

Delete the four functions (lines 8–48, between `pub mod report;` and `write_flac`) and add in their place:

```rust
pub use musefs_format::fuzz_check::fixtures::{
    flac_block, make_flac, streaminfo_body, vorbis_comment_body,
};
```

The in-module callers (`write_flac`, `write_oggflac_with_art`) resolve through the `pub use` unchanged, as do `common::*` users in dependent test files and the `#[path]`-included benches (musefs-core's dev-dep on musefs-format already enables `fuzzing`).

- [ ] **Step 5: Run both crates' tests**

Run: `cargo test -p musefs-format`
Expected: PASS, **and** the proptest targets (`proptest_flac`, `proptest_mp3`, `proptest_mp4`, `proptest_ogg`, `proptest_wav`) now appear in the run without `--features fuzzing` (they are `#![cfg(feature = "fuzzing")]`; the self-dev-dep turns the feature on).

Run: `cargo test -p musefs-core`
Expected: PASS.

- [ ] **Step 6: Update CLAUDE.md**

Replace:

```
# Property tests (proptest): byte-identical invariant + tag round-trip. The
# format-layer proptests need the `fuzzing` feature; `cargo test --workspace`
# also runs them via feature unification.
cargo test -p musefs-format --features fuzzing
```

with:

```
# Property tests (proptest): byte-identical invariant + tag round-trip. The
# format-layer proptests are gated on the `fuzzing` feature, which
# musefs-format's self-dev-dependency enables for all of its test builds.
cargo test -p musefs-format
```

- [ ] **Step 7: Build the out-of-workspace fuzz targets**

The fuzz crate consumes `fuzz_check::fixtures` and is not compiled by workspace builds:

Run: `cargo +nightly fuzz build`
Expected: all targets build (the `fixtures::flac()` signature is unchanged; this catches any accidental breakage).

- [ ] **Step 8: Format, then commit**

Run: `cargo fmt --all && cargo fmt --all --check`
Expected: exit 0 (the fmt gate is CI-enforced; never commit unformatted code).

```bash
git add musefs-format/Cargo.toml musefs-format/src/fuzz_check.rs \
  musefs-format/tests/common/mod.rs musefs-core/tests/common/mod.rs \
  CLAUDE.md Cargo.lock
git commit -m "$(cat <<'EOF'
Consolidate FLAC fixture helpers into fuzz_check::fixtures (#139)

Three copies (format tests/common, core tests/common, fuzz_check's
private set) become one. The self-dev-dependency turns the fuzzing
feature on for musefs-format's own test builds, so the proptests now
run under a plain `cargo test -p musefs-format`.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

(Include `Cargo.lock` only if the self-dev-dep changed it.)

---

### Task 3: Bench fixture returns its `TempDir` instead of leaking it

**Files:**
- Modify: `musefs-core/benches/read_throughput.rs` (`fixture` ~line 38, callers ~lines 74 and ~102)

- [ ] **Step 1: Change `fixture()` to return the `TempDir`**

Signature:

```rust
fn fixture(
    format: Format,
    bytes_per_track: usize,
    tracks: usize,
) -> (Arc<Musefs>, Vec<u64>, tempfile::TempDir) {
```

And replace the tail of the function:

```rust
    // Keep the tempdir alive for the duration of the bench by leaking it. Each
    // fixture call leaks one; a full run accumulates at most ALL_FORMATS.len()+1
    // (sequential per-format + concurrent), all reclaimed when the process exits.
    std::mem::forget(dir);
    (fs, inodes)
}
```

with:

```rust
    (fs, inodes, dir)
}
```

(This matches the sibling `cold_fixture()`, which already returns its `TempDir`.)

- [ ] **Step 2: Update the two callers**

In `bench_sequential_read`:

```rust
        let (fs, inodes) = fixture(fmt, 4 * 1024 * 1024, 1);
```

→

```rust
        let (fs, inodes, _dir) = fixture(fmt, 4 * 1024 * 1024, 1);
```

In `bench_concurrent_read_and_walk`:

```rust
    let (fs, inodes) = fixture(Format::Flac, 1024 * 1024, m.max(2));
```

→

```rust
    let (fs, inodes, _dir) = fixture(Format::Flac, 1024 * 1024, m.max(2));
```

**Must be `_dir`, not `_`** — a bare `_` pattern drops the `TempDir` immediately and the bench would read from a deleted directory.

- [ ] **Step 3: Compile-check the bench**

Run: `cargo clippy -p musefs-core --benches`
Expected: clean (this compiles `read_throughput.rs`; plain `cargo test` does not build `harness = false` benches).

Run: `cargo fmt --all && cargo fmt --all --check`
Expected: exit 0.

- [ ] **Step 4: Commit**

```bash
git add musefs-core/benches/read_throughput.rs
git commit -m "$(cat <<'EOF'
Stop leaking the bench fixture TempDir (#139)

Return it from fixture() and let callers hold it, matching
cold_fixture().

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 4: Locally-evident bounds in the `b64` fuzz target

**Files:**
- Modify: `fuzz/fuzz_targets/b64.rs` (lines 20–32)

- [ ] **Step 1: Rewrite the guard and bounds**

Replace:

```rust
    let total = b64_len(img.len() as u64);
    if total == 0 {
        return;
    }
    let full = encode_b64_slice(&img, 0, total as usize);
    let out_off = match u.int_in_range(0..=total - 1) {
        Ok(v) => v,
        Err(_) => return,
    };
    let take = match u.int_in_range(1..=total - out_off) {
        Ok(v) => v,
        Err(_) => return,
    };
```

with:

```rust
    let total = b64_len(img.len() as u64);
    let full = encode_b64_slice(&img, 0, total as usize);
    // img is non-empty so total >= 4; checked_sub keeps each bound
    // underflow-safe on its own, independent of statement order.
    let Some(max_off) = total.checked_sub(1) else {
        return;
    };
    let out_off = match u.int_in_range(0..=max_off) {
        Ok(v) => v,
        Err(_) => return,
    };
    let Some(max_take) = total.checked_sub(out_off) else {
        return;
    };
    let take = match u.int_in_range(1..=max_take) {
        Ok(v) => v,
        Err(_) => return,
    };
```

(The old `total == 0` guard was dead — `img` is rejected when empty at lines 17–19, so `total >= 4` always; the first `checked_sub` subsumes it.)

- [ ] **Step 2: Build the fuzz target**

Run: `cargo +nightly fuzz build b64`
Expected: builds clean.

- [ ] **Step 3: Smoke-run the target briefly**

Run: `cargo +nightly fuzz run b64 -- -max_total_time=30`
Expected: no crashes in 30 seconds (the windowed-encode property still holds).

Run: `cd fuzz && cargo fmt --check; cd ..`
Expected: exit 0 (the fuzz crate is outside the workspace, so `cargo fmt --all` from the root does not cover it; format it in place with `cd fuzz && cargo fmt` if the check fails).

- [ ] **Step 4: Commit**

```bash
git add fuzz/fuzz_targets/b64.rs
git commit -m "$(cat <<'EOF'
Make b64 fuzz bounds locally underflow-safe (#139)

checked_sub instead of bare subtractions whose non-underflow depended
on a guard and a prior range several statements away; the dead
total==0 guard goes away.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 5: Full verification sweep

**Files:** none (verification only)

- [ ] **Step 1: Workspace gates**

```bash
cargo test --workspace
cargo clippy --all-targets
cargo fmt --all --check
```

Expected: all exit 0 (check the exit status of each directly — the fmt gate is CI-enforced).

- [ ] **Step 2: In-diff mutation gate over the full branch**

```bash
git diff "$(git merge-base main HEAD)...HEAD" -- '*.rs' > mutants.diff
grep -q '^@@ ' mutants.diff
cargo mutants --in-diff mutants.diff -j2 --exclude 'musefs-latencyfs/**' --output /tmp/mutants-out/in-diff
```

Expected: exit 0, no MISSED. (Do not set TMPDIR; the `grep -q '^@@ '` sanity check guards against an empty diff silently passing. `mutants.diff` is untracked scratch — do not commit it.)

- [ ] **Step 3: Fuzz-crate build (full)**

```bash
cargo +nightly fuzz build
```

Expected: all targets build — guards the `fuzz_check::fixtures` consumers the workspace build doesn't compile.

- [ ] **Step 4: Confirm no env-var stragglers**

```bash
grep -rn "MUSEFS_SCAN_WINDOW\|MUSEFS_BATCH_BYTES" --include='*.rs' .
```

Expected: no matches anywhere in the repo's Rust sources.
