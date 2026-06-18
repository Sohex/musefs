# Non-destructive `scan` / `revalidate` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make bare `musefs scan` additive (never overwrite or delete curated DB data); confine tag-overwrite to `scan --force` and track-deletion to `revalidate --prune`; make `revalidate` refresh only structural/serving facts while preserving curated metadata.

**Architecture:** Two data layers — **A** (structural: audio bounds, stamp, checksums, FLAC structural blocks) and **B** (curated: text tags, art, binary tags). A new `refresh_structural_into` writes Layer A only. `run_pipeline` gains a `WritePolicy` so `revalidate` uses the structural-only write while `scan` uses the full write. Bare `scan` pre-filters out paths already in the DB (so existing rows are never touched); `--force` skips that pre-filter. `revalidate` is promoted to its own CLI subcommand; `scan --revalidate` survives one release as a warned, non-pruning alias. The Python `run_scan` wrapper (vendored twice) learns to emit the new verbs.

**Tech Stack:** Rust (workspace crates `musefs-db` → `musefs-format` → `musefs-core` → `musefs-cli` → `musefs` binary), SQLite (rusqlite), clap; Python contrib (beets plugin, Picard plugin, shared `musefs_common`), pytest.

## Global Constraints

- **Audio-byte invariant:** original audio bytes are never copied or modified. None of this work touches the read/serve path.
- **Workspace is strictly layered** (db → format → core → cli → binary). Cross-cutting logic lives in `musefs-core`; CLI stays thin.
- **`[lints]` denies `unsafe_code`** (even in tests). Use safe wrappers only.
- **Pre-commit hook runs the full workspace test suite** (fmt, clippy `-D warnings`, all tests, shellcheck/yamllint/ruff). Every commit must be green. Docs-only commits skip the cargo gate.
- **Mutant anchors:** editing `musefs-core/src/scan.rs` shifts lines and trips `check_mutant_anchors.py` in pre-commit. Re-anchor `.cargo/mutants.toml` (via each entry's `# guard:` tag) **in the same commit** as the scan.rs change, or the commit is rejected.
- **metrics feature is not in the default test run.** After scan-path edits, run `cargo test -p musefs-core --features metrics` before considering core done (CI's `check` job runs it).
- **`fuzz/` is out of the workspace.** No format-layer signatures change here, so a fuzz build is not expected to break — but if any `Probed`/format API shifts, run `cargo +nightly fuzz build`.
- **Schema is unchanged** by this plan (no `musefs-db` schema edit → no Python schema mirror regen).
- **Released version is v1.1.0.** Bare-`scan` semantics change is **breaking** → prominent changelog entry; the `scan --revalidate` alias is deprecated for removal next release.
- Commit trailer: `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`.

## Test Fixture Conventions (verified against the real helpers — use these exact forms)

The `musefs-core` integration tests share `tests/common/mod.rs`. The example
test code in this plan uses these real signatures — do **not** invent variants:

- **Write a FLAC fixture:** `common::write_flac(path: &Path, comments: &[&str], audio: &[u8]) -> (u64, u64)`. Comments are `KEY=VALUE` strings. Example: `let (off, len) = write_flac(&path, &["TITLE=A"], &[0xAA; 30]);`. Add `write_flac` to the test file's `use common::{…}` line (e.g. `musefs-core/tests/scan.rs` currently imports `make_flac, streaminfo_body, vorbis_comment_body` — append `write_flac`). To mutate a file so its stamp changes, `write_flac` again with different bytes/comments.
- **Open an in-memory DB:** `musefs_db::Db::open_in_memory()`.
- **List tracks:** `db.list_tracks() -> Vec<Track>`. There is no `db.tracks()`.
- **Read a track's tags:** `db.get_tags(track_id) -> Vec<Tag>`; fields `Tag.key`, `Tag.value`. There is no `tags_for_track` (only `tags_for_tracks(&[i64]) -> HashMap<…>`, which is not what these tests want).
- **Read structural blocks:** `db.get_structural_blocks(track_id) -> Vec<StructuralBlock>`; fields `.kind`, `.ordinal`, `.body`.
- **Track audio bounds:** `track.bounds.audio_offset()` / `track.bounds.audio_length()` (NOT `track.audio_offset`); `track.backing_size` IS a direct field.
- **Edit tags in the DB (simulate an external writer):** `db.replace_tags(id, &[musefs_db::Tag::new("title", "Curated", 0)])` — confirm `Db::replace_tags` is `pub` and reachable from the test crate; if it is crate-private, use the `TrackSink`/`BulkWriter` path the sibling tests already use to seed tags. `Tag::new(key, value, ordinal)` is correct.
- **Clear structural blocks (simulate a V1 row):** `db.set_structural_blocks(id, &[])` — same `pub`-reachability caveat; if not public, simulate the V1 row the way `incremental_refresh.rs` already does (grep it for the existing V1 seam).

The in-crate `scan_unit_tests.rs` tests build `Probed { … }` and `Unit { … }` directly (module-private fields are visible there) and call `ingest_into(&db, …)` / `ingest_unit(&db, …)` against `impl TrackSink for &Db` — Task 1's unit test follows that existing pattern.

---

## File Structure

**Core (`musefs-core/src/scan.rs`)** — all the new write/policy/pre-filter logic:
- `WritePolicy` enum (new, private)
- `refresh_structural_into` (new, private) — Layer-A-only write
- `ingest_unit` (modify) — take `WritePolicy`
- `run_pipeline` (modify) — take + thread `WritePolicy`
- `ScanOptions` (modify) — add `force`, `prune`
- `ScanStats` (modify) — add `already_present`
- `scan_directory_with` (modify) — additive pre-filter when `!force`
- `revalidate_with` (modify) — structural-only policy, existing-only scope, prune behind `opts.prune`
- `scan_unit_tests` / `hardening_tests` modules (modify) — unit tests
- `musefs-core/src/lib.rs` — no re-export changes (all new items private; structs already re-exported)

**Core integration tests:**
- `musefs-core/tests/incremental_refresh.rs` (modify/extend) — the revalidate behavior tests live here today
- `musefs-core/tests/scan.rs` (modify/extend) — additive-scan + force tests
- `musefs-core/tests/scan_counters.rs` (modify) — `already_present` counter

**CLI (`musefs-cli/src/lib.rs`)**:
- `Command::Scan` (modify) — add `--force`
- `Command::Revalidate` (new variant) — `--prune`
- `run_scan` (modify) — add `force`; deprecated-`--revalidate` warns + delegates
- `run_revalidate` (new)
- `run` dispatch (modify)
- CLI parse tests (modify/extend)

**CLI integration tests:** `musefs-cli/tests/scan.rs`, `musefs-cli/tests/cli.rs`, `musefs/tests/cli_process.rs` (extend).

**Contrib Python:**
- `contrib/python-musefs/src/musefs_common/scan.py` (modify) — `run_scan` gains `force`, `prune`; emits new verbs
- `contrib/picard/musefs/_common/scan.py` (re-vendor — byte-identical copy)
- `contrib/beets/beetsplug/musefs.py` (modify) — `_run_scan` passes `force`/`prune`; callers updated
- Python tests under `contrib/*/tests/` (extend)

**Docs:** `docs/src/architecture/tree-scanning.md`, `docs/src/architecture/store.md`, `README.md`, `docs/src/changelog.md`, `CLAUDE.md`, wrapper docstrings, contrib integration docs.

---

## Task 1: Layer-A-only write path + `WritePolicy`

**Files:**
- Modify: `musefs-core/src/scan.rs` (add `WritePolicy`, `refresh_structural_into`; change `ingest_unit` signature)
- Test: `musefs-core/src/scan.rs` `mod scan_unit_tests` (in-crate, can build `Probed`)

**Interfaces:**
- Produces: `enum WritePolicy { Full, StructuralOnly }` (private, `Copy`); `fn refresh_structural_into(w: impl TrackSink, abs_path: &str, stamp: BackingStamp, probed: Probed, fingerprint: Option<&str>, content_hash: Option<&str>) -> Result<()>`; `fn ingest_unit(w: impl TrackSink, unit: Unit, strictness: MatchStrictness, policy: WritePolicy) -> Result<()>`.
- Consumes: existing `TrackSink` (`upsert_track`, `set_track_checksums`, `set_structural_blocks`), `Probed`, `BackingStamp`, `NewTrack`.

- [ ] **Step 1: Write the failing unit test**

In `mod scan_unit_tests` add (uses the in-crate `impl TrackSink for &Db` and crate-private `Probed`):

```rust
#[test]
fn refresh_structural_into_preserves_tags_and_art() {
    use musefs_db::Db;
    let db = Db::open_in_memory().unwrap();
    // Seed a track with a tag + structural block via the full path.
    let stamp = BackingStamp { size: 10, mtime_ns: 1, ctime_ns: 1 };
    let seeded = Probed {
        format: Format::Flac,
        audio_offset: 4,
        audio_length: 6,
        tags: vec![("title".into(), "Original".into())],
        pictures: vec![],
        binary_tags: vec![],
        structural_blocks: vec![("STREAMINFO".into(), vec![1, 2, 3])],
    };
    ingest_into(&db, "/m/a.flac", stamp, seeded, None, None).unwrap();
    let id = db.list_tracks().unwrap()[0].id;

    // Refresh structurally with DIFFERENT tags + bounds; tags must NOT change.
    let changed = Probed {
        format: Format::Flac,
        audio_offset: 8,
        audio_length: 12,
        tags: vec![("title".into(), "CLOBBERED".into())],
        pictures: vec![],
        binary_tags: vec![],
        structural_blocks: vec![("STREAMINFO".into(), vec![9, 9, 9])],
    };
    let stamp2 = BackingStamp { size: 20, mtime_ns: 2, ctime_ns: 2 };
    refresh_structural_into(&db, "/m/a.flac", stamp2, changed, None, None).unwrap();

    let track = &db.list_tracks().unwrap()[0];
    assert_eq!(track.id, id, "same row upserted, not replaced");
    assert_eq!(track.bounds.audio_offset(), 8, "Layer A bounds refreshed");
    assert_eq!(track.backing_size, 20, "Layer A stamp refreshed");
    let tags = db.get_tags(id).unwrap();
    assert_eq!(tags.len(), 1);
    assert_eq!(tags[0].value, "Original", "Layer B tag preserved");
    let blocks = db.get_structural_blocks(id).unwrap();
    assert_eq!(blocks[0].body, vec![9, 9, 9], "Layer A structural block refreshed");
}
```

> NOTE: confirm the exact read-back helper names against `musefs-db` (`get_tags`, `get_structural_blocks`, `Db::open_in_memory`). If a name differs, use the one the sibling tests in this module already use — grep `mod scan_unit_tests` for the existing read-back calls and match them.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p musefs-core refresh_structural_into_preserves -- --nocapture`
Expected: FAIL — `cannot find function refresh_structural_into`.

- [ ] **Step 3: Add `WritePolicy` and `refresh_structural_into`**

Insert directly above `ingest_into` in `musefs-core/src/scan.rs`:

```rust
/// Whether the pipeline writer overwrites curated metadata (Layer B) or only
/// refreshes structural/serving facts (Layer A).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WritePolicy {
    /// Full upsert: track row + checksums + tags + binary tags + structural
    /// blocks + art. Re-seeds Layer B from the file (scan / scan --force).
    Full,
    /// Layer A only: track row + checksums + structural blocks. Leaves tags,
    /// art, and binary tags untouched (revalidate of a changed file).
    StructuralOnly,
}

/// Refresh Layer A (structural/serving facts) for an already-probed file
/// without touching Layer B (tags / art / binary tags). Upserts the track row
/// by `abs_path` (same row id), rewrites checksums and structural blocks, and
/// leaves the curated tag/art/binary-tag rows in place. Takes `probed` by value
/// for symmetry with `ingest_into`; only its structural fields are read.
fn refresh_structural_into(
    mut w: impl TrackSink,
    abs_path: &str,
    stamp: BackingStamp,
    probed: Probed,
    fingerprint: Option<&str>,
    content_hash: Option<&str>,
) -> Result<()> {
    let track_id = w.upsert_track(&NewTrack {
        backing_path: abs_path.to_string(),
        format: probed.format,
        audio_offset: probed.audio_offset,
        audio_length: probed.audio_length,
        backing_size: stamp.size,
        backing_mtime_ns: stamp.mtime_ns,
        backing_ctime_ns: stamp.ctime_ns,
    })?;
    w.set_track_checksums(track_id, fingerprint, content_hash)?;

    let mut sb_ordinals: HashMap<String, u64> = HashMap::new();
    let structural_blocks: Vec<musefs_db::StructuralBlock> = probed
        .structural_blocks
        .into_iter()
        .map(|(kind, body)| {
            let ord = sb_ordinals.entry(kind.clone()).or_insert(0);
            let sb = musefs_db::StructuralBlock { kind, ordinal: *ord, body };
            *ord += 1;
            sb
        })
        .collect();
    w.set_structural_blocks(track_id, &structural_blocks)?;
    Ok(())
}
```

- [ ] **Step 4: Thread `WritePolicy` into `ingest_unit`**

Change `ingest_unit`'s signature and add the early structural-only branch at the top of the body (immediately after the doc comment / `fn` line):

```rust
fn ingest_unit(
    mut w: impl TrackSink,
    unit: Unit,
    strictness: MatchStrictness,
    policy: WritePolicy,
) -> Result<()> {
    if policy == WritePolicy::StructuralOnly {
        // Revalidate of a known, changed file: refresh Layer A, preserve Layer B.
        // Revalidate only ever feeds known paths, so the retarget branch below is
        // unreachable here and intentionally skipped.
        return refresh_structural_into(
            w,
            &unit.abs_path,
            unit.stamp,
            unit.probed,
            unit.fingerprint.as_deref(),
            unit.content_hash.as_deref(),
        );
    }
    // ... existing body unchanged (the `track_exists_at` upsert + retarget logic) ...
}
```

> The existing body below is unchanged. `strictness` stays used by the `Full` path.

- [ ] **Step 5: Update ALL `ingest_unit` call sites (or the crate won't compile)**

`ingest_unit` has four callers — miss one and the Task 1 commit fails the pre-commit full-suite build. Grep first: `grep -rn "ingest_unit(" musefs-core/src`. The sites:

- `musefs-core/src/scan.rs` (in `run_pipeline`): `ingest_unit(&mut bw, unit, strictness)` → add `, WritePolicy::Full`:

```rust
ingest_unit(&mut bw, unit, strictness, WritePolicy::Full)?;
```

- `musefs-core/src/scan/scan_unit_tests.rs` — three calls (currently `ingest_unit(&db, unit, MatchStrictness::Auto).unwrap();`). Add `, WritePolicy::Full` to each:

```rust
ingest_unit(&db, unit, MatchStrictness::Auto, WritePolicy::Full).unwrap();
```

> `WritePolicy` is private to `scan.rs`; `scan_unit_tests.rs` is a submodule (`mod scan_unit_tests;`) and reaches it via `use super::*` already present in that test module. Confirm the import resolves; if the module doesn't glob-import `super`, qualify as `super::WritePolicy::Full`.

- [ ] **Step 6: Run the test to verify it passes**

Run: `cargo test -p musefs-core refresh_structural_into_preserves`
Expected: PASS.

- [ ] **Step 7: Re-anchor mutants + run the crate suite**

Inserting code in `scan.rs` shifts the `file:line:col` anchors `.cargo/mutants.toml` pins for `run_pipeline`/`revalidate_with` (lines ~163–176). These are op/fn-unique line:col anchors — `--fix` can only re-anchor covering-set clusters, so expect it to **no-op them**; hand-anchoring is the normal path here, not a fallback:

1. `python3 scripts/check_mutant_anchors.py` — run the **check** (no `--fix`) to see which anchors drifted.
2. For each drifted entry, update its `file:line:col` to the new location, using its `# guard: op=… fn=…` tag to find the new site (grep the function, locate the operator/line).
3. Re-verify: `python3 scripts/check_mutant_anchors.py` passes, then `cargo test -p musefs-core` and `cargo clippy -p musefs-core --all-targets`.

Expected: anchors check clean, tests pass, no warnings.

- [ ] **Step 8: Commit**

```bash
git add musefs-core/src/scan.rs .cargo/mutants.toml
git commit -m "$(cat <<'EOF'
feat(core): Layer-A-only structural refresh write path

Add refresh_structural_into (track row + checksums + structural blocks,
leaving tags/art/binary-tags untouched) and a WritePolicy enum. ingest_unit
takes the policy; StructuralOnly short-circuits to the structural refresh.
No behavior change yet — every caller passes Full.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Thread `WritePolicy` through `run_pipeline`

**Files:**
- Modify: `musefs-core/src/scan.rs` (`run_pipeline` signature + both call sites)

**Interfaces:**
- Produces: `fn run_pipeline(db: &Db, files: Vec<PathBuf>, opts: &ScanOptions, policy: WritePolicy) -> Result<ScanStats>`.
- Consumes: `WritePolicy` (Task 1).

- [ ] **Step 1: Change `run_pipeline` to take `policy` and use it**

Update the signature:

```rust
fn run_pipeline(
    db: &Db,
    files: Vec<PathBuf>,
    opts: &ScanOptions,
    policy: WritePolicy,
) -> Result<ScanStats> {
```

In the `flush` closure replace the call added in Task 1 Step 5 with the parameter:

```rust
ingest_unit(&mut bw, unit, strictness, policy)?;
```

- [ ] **Step 2: Update both production callers — `Full` for now (no behavior change)**

In `scan_directory_with`, change `run_pipeline(db, files, opts)?` to:

```rust
let mut stats = run_pipeline(db, files, opts, WritePolicy::Full)?;
```

In `revalidate_with`, change `run_pipeline(db, changed, opts)?` to **`Full` as well** for this task — revalidate keeps its current (tag-replacing) behavior until Task 5, which flips it to `StructuralOnly` together with the test rewrite in one green commit:

```rust
let scan = run_pipeline(db, changed, opts, WritePolicy::Full)?;
```

> `scan_directory_full_oracle` uses the direct `ingest` path, not `run_pipeline` — no change. This task is a **pure refactor: zero behavior change**, so the whole suite stays green.

- [ ] **Step 3: Build + run the suite**

Run: `cargo test -p musefs-core`
Expected: PASS (no behavior changed — both callers still use `Full`).

- [ ] **Step 4: Commit**

```bash
git add musefs-core/src/scan.rs
git commit -m "$(cat <<'EOF'
refactor(core): thread WritePolicy through run_pipeline (no behavior change)

Both callers pass Full for now; revalidate flips to StructuralOnly in the
task that rewrites its tests, keeping every commit green.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: `ScanOptions.force` / `.prune` + `ScanStats.already_present`

**Files:**
- Modify: `musefs-core/src/scan.rs` (`ScanOptions`, its `Default`, `ScanStats`, every `ScanStats { .. }` literal)

**Interfaces:**
- Produces: `ScanOptions { …, force: bool, prune: bool }`; `ScanStats { scanned, skipped, already_present, failed, raced }`.

- [ ] **Step 1: Add the fields**

In `ScanOptions` add:

```rust
    /// Scan only: re-ingest files already in the DB, overwriting curated tags
    /// (Layer B). Off by default — bare `scan` is additive.
    pub force: bool,
    /// Revalidate only: delete tracks whose backing file is gone (+ orphan-art
    /// GC). Off by default.
    pub prune: bool,
```

In `impl Default for ScanOptions` add `force: false,` and `prune: false,`.

In `ScanStats` add `pub already_present: u64,` and derive `Default`:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ScanStats {
    pub scanned: u64,
    pub skipped: u64,
    /// Files skipped because a track already exists at that path (bare `scan`).
    pub already_present: u64,
    pub failed: u64,
    pub raced: u64,
}
```

- [ ] **Step 2: Fix the `ScanStats` literals**

`run_pipeline` builds `ScanStats { scanned, skipped: 0, failed, raced }` → add `already_present: 0,`. `scan_directory_full_oracle` builds `ScanStats { scanned: 0, skipped, failed: 0, raced: 0 }` → add `already_present: 0,`.

- [ ] **Step 3: Build + fix downstream literals**

Run: `cargo build -p musefs-core 2>&1 | head -40`
Expected: compile errors only where `ScanStats` is constructed/destructured exhaustively. Add `already_present: 0` (or `..` in pattern matches) at each. Grep to find them: `grep -rn "ScanStats {" musefs-core musefs-cli`.

- [ ] **Step 4: Run the suite**

Run: `cargo test -p musefs-core scan_counters` then `cargo test -p musefs-core`
Expected: existing counter tests pass (new field defaults to 0).

- [ ] **Step 5: Commit**

```bash
git add musefs-core/src/scan.rs
git commit -m "$(cat <<'EOF'
feat(core): add ScanOptions force/prune and ScanStats.already_present

Plumbing only; defaults preserve current behavior.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: Additive `scan` (pre-filter known paths unless `--force`)

**Files:**
- Modify: `musefs-core/src/scan.rs` (`scan_directory_with`)
- Test: `musefs-core/tests/scan.rs`

**Interfaces:**
- Consumes: `ScanOptions.force`, `ScanStats.already_present`, `db.list_tracks()`.

- [ ] **Step 1: Write failing integration tests**

Append to `musefs-core/tests/scan.rs` (match the file's existing helpers for writing a FLAC fixture + opening a `Db`; grep the top of the file for the fixture helper name and reuse it):

```rust
#[test]
fn bare_rescan_is_additive_and_preserves_db_edits() {
    let dir = tempfile::tempdir().unwrap();
    let db = musefs_db::Db::open_in_memory().unwrap();
    write_flac(&dir.path().join("a.flac"), &["TITLE=A-on-disk"], &[0xAA; 30]);

    musefs_core::scan_directory(&db, dir.path()).unwrap();
    let id = db.list_tracks().unwrap()[0].id;
    // Simulate an external tag edit in the DB.
    db.replace_tags(id, &[musefs_db::Tag::new("title", "Curated", 0)]).unwrap();

    // Add a NEW file, then bare scan again.
    write_flac(&dir.path().join("b.flac"), &["TITLE=B"], &[0xBB; 40]);
    let stats = musefs_core::scan_directory(&db, dir.path()).unwrap();

    assert_eq!(stats.scanned, 1, "only the new file ingested");
    assert_eq!(stats.already_present, 1, "the existing file was skipped");
    let a = db.list_tracks().unwrap().into_iter().find(|t| t.backing_path.ends_with("a.flac")).unwrap();
    let tags = db.get_tags(a.id).unwrap();
    assert_eq!(tags[0].value, "Curated", "bare scan did not clobber the DB edit");
}

#[test]
fn force_rescan_reseeds_tags_from_file() {
    let dir = tempfile::tempdir().unwrap();
    let db = musefs_db::Db::open_in_memory().unwrap();
    write_flac(&dir.path().join("a.flac"), &["TITLE=A-on-disk"], &[0xAA; 30]);
    musefs_core::scan_directory(&db, dir.path()).unwrap();
    let id = db.list_tracks().unwrap()[0].id;
    db.replace_tags(id, &[musefs_db::Tag::new("title", "Curated", 0)]).unwrap();

    let opts = musefs_core::ScanOptions { force: true, ..Default::default() };
    musefs_core::scan_directory_with(&db, dir.path(), &opts).unwrap();

    let tags = db.get_tags(id).unwrap();
    assert_eq!(tags[0].value, "A-on-disk", "--force re-seeds from the file");
}
```

> Uses `common::write_flac` (3-arg) — add it to this file's `use common::{…}` line. See "Test Fixture Conventions" above.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p musefs-core --test scan bare_rescan_is_additive force_rescan_reseeds`
Expected: `bare_rescan_is_additive…` FAILS (today scan clobbers → tag is "Title A", and `already_present` is 0).

- [ ] **Step 3: Implement the pre-filter in `scan_directory_with`**

After `files`/`tally` are built and before `db.apply_bulk_pragmas_self()`, insert:

```rust
    // Additive scan: drop files already tracked in the DB so existing rows are
    // never re-probed or re-ingested. `--force` keeps them, re-seeding Layer B.
    let mut already_present = 0u64;
    if !opts.force {
        let existing: std::collections::HashSet<String> = db
            .list_tracks()?
            .into_iter()
            .map(|t| t.backing_path)
            .collect();
        let before = files.len();
        files.retain(|p| {
            let key = if opts.follow_symlinks {
                match std::fs::canonicalize(p) {
                    Ok(abs) => abs.to_string_lossy().into_owned(),
                    Err(_) => return true, // can't canonicalize → let the pipeline handle/fail it
                }
            } else {
                p.to_string_lossy().into_owned()
            };
            !existing.contains(&key)
        });
        already_present = (before - files.len()) as u64;
    }
```

Then, after `let mut stats = run_pipeline(db, files, opts, WritePolicy::Full)?;`, set the counter:

```rust
    stats.already_present = already_present;
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p musefs-core --test scan bare_rescan_is_additive force_rescan_reseeds`
Expected: PASS.

- [ ] **Step 5: Full crate suite + clippy + re-anchor**

Run `python3 scripts/check_mutant_anchors.py` (check only); hand-anchor any drifted `file:line:col` entries per their `# guard:` tags (see Task 1 Step 7 — `--fix` won't move these op/fn-unique anchors), then `cargo test -p musefs-core && cargo clippy -p musefs-core --all-targets`.
Expected: anchors clean, tests pass, no warnings.

- [ ] **Step 6: Commit**

```bash
git add musefs-core/src/scan.rs musefs-core/tests/scan.rs .cargo/mutants.toml
git commit -m "$(cat <<'EOF'
feat(core): bare scan is additive; --force re-seeds existing tracks

scan_directory_with drops already-tracked paths unless ScanOptions.force,
so a re-scan never overwrites curated tags. Reports the skipped count as
ScanStats.already_present.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: `revalidate` — Layer-A refresh, existing-only scope, prune behind `--prune`

**Files:**
- Modify: `musefs-core/src/scan.rs` (`revalidate_with`)
- Test: `musefs-core/tests/incremental_refresh.rs`

**Interfaces:**
- Consumes: `WritePolicy::StructuralOnly` (already wired in Task 2), `ScanOptions.prune`.

- [ ] **Step 1: Write/adjust failing tests**

In `musefs-core/tests/incremental_refresh.rs` add (and repurpose any existing test that asserted a changed-file's tags get *replaced* — that assertion is now inverted):

```rust
#[test]
fn revalidate_changed_file_refreshes_layer_a_preserves_layer_b() {
    let dir = tempfile::tempdir().unwrap();
    let db = musefs_db::Db::open_in_memory().unwrap();
    let path = dir.path().join("a.flac");
    write_flac(&path, &["TITLE=A"], &[0xAA; 30]);
    musefs_core::scan_directory(&db, dir.path()).unwrap();
    let id = db.list_tracks().unwrap()[0].id;
    db.replace_tags(id, &[musefs_db::Tag::new("title", "Curated", 0)]).unwrap();

    // Mutate the backing file (new bytes + new embedded title) so the stamp changes.
    write_flac(&path, &["TITLE=B-on-disk"], &[0xBB; 40]);
    let stats = musefs_core::revalidate(&db, dir.path()).unwrap();

    assert_eq!(stats.updated, 1);
    assert_eq!(stats.pruned, 0, "no prune without --prune");
    let tags = db.get_tags(id).unwrap();
    assert_eq!(tags[0].value, "Curated", "revalidate preserved the DB edit");
    // (Optional, if the fixture varies audio bounds:) assert refreshed bounds/stamp.
}

#[test]
fn revalidate_ignores_new_files() {
    let dir = tempfile::tempdir().unwrap();
    let db = musefs_db::Db::open_in_memory().unwrap();
    write_flac(&dir.path().join("a.flac"), &["TITLE=A"], &[0xAA; 30]);
    musefs_core::scan_directory(&db, dir.path()).unwrap();

    write_flac(&dir.path().join("b.flac"), &["TITLE=B"], &[0xBB; 40]); // new, not in DB
    let stats = musefs_core::revalidate(&db, dir.path()).unwrap();

    assert_eq!(stats.updated, 0, "new file is scan's job, not revalidate's");
    assert_eq!(db.list_tracks().unwrap().len(), 1, "b.flac NOT ingested");
}

#[test]
fn revalidate_prunes_only_with_flag() {
    let dir = tempfile::tempdir().unwrap();
    let db = musefs_db::Db::open_in_memory().unwrap();
    let path = dir.path().join("a.flac");
    write_flac(&path, &["TITLE=A"], &[0xAA; 30]);
    musefs_core::scan_directory(&db, dir.path()).unwrap();
    std::fs::remove_file(&path).unwrap();

    let stats = musefs_core::revalidate(&db, dir.path()).unwrap();
    assert_eq!(stats.pruned, 0);
    assert_eq!(db.list_tracks().unwrap().len(), 1, "default revalidate keeps the row");

    let opts = musefs_core::ScanOptions { prune: true, ..Default::default() };
    let stats = musefs_core::revalidate_with(&db, dir.path(), &opts).unwrap();
    assert_eq!(stats.pruned, 1);
    assert_eq!(db.list_tracks().unwrap().len(), 0, "--prune deletes the gone track");
}
```

- [ ] **Step 1b: Repurpose the existing replacement-asserting tests**

`musefs-core/tests/incremental_refresh.rs` currently has tests asserting that revalidate **replaces** a changed file's tags (re-probe round-trips, ~lines 84–220). After Step 3a flips revalidate to `StructuralOnly`, those assertions invert. Grep the file for assertions that a changed file's DB tag now equals the file's new embedded tag, and update them to assert the **DB tag is preserved** (or delete the ones fully superseded by `revalidate_changed_file_refreshes_layer_a_preserves_layer_b`). Do this in THIS task so the commit is green.

- [ ] **Step 2: Run to verify failures (pre-implementation)**

Run: `cargo test -p musefs-core --test incremental_refresh revalidate_`
Expected: all three new tests FAIL before Steps 3a/3b/4 (today revalidate clobbers tags on a changed file, ingests new files, and always prunes).

- [ ] **Step 3a: Flip revalidate to the Layer-A write policy**

In `revalidate_with`, change the `run_pipeline` call from `WritePolicy::Full` (set in Task 2) to `StructuralOnly`:

```rust
let scan = run_pipeline(db, changed, opts, WritePolicy::StructuralOnly)?;
```

This is the commit where revalidate stops clobbering Layer B — done together with the scope/prune edits and test rewrite below so the suite is green at commit time.

- [ ] **Step 3b: Scope revalidate to existing tracks (skip new files)**

In `revalidate_with`, the loop currently pushes to `changed` for any non-skipped path. Change it so a path **not** in `existing` is ignored entirely. Replace the tail of the per-file loop:

```rust
        if let Some((stamp, id, format, has_fingerprint, has_content_hash)) =
            existing.get(&key).copied()
        {
            let needs_backfill = format == Format::Flac && !have_structural.contains(&id);
            let needs_checksum = match opts.checksum {
                ChecksumTier::None => false,
                ChecksumTier::Fingerprint => !has_fingerprint,
                ChecksumTier::Full => !has_fingerprint || !has_content_hash,
            };
            if crate::freshness::BackingStamp::from_metadata(&meta) == stamp
                && !needs_backfill
                && !needs_checksum
            {
                unchanged += 1;
                continue;
            }
            changed.push(path); // existing + changed/needs-refresh → refresh Layer A
        }
        // else: path not in the DB → revalidate ignores it (that is scan's job)
```

(That is: move `changed.push(path)` *inside* the `if let Some(...)` block.)

- [ ] **Step 4: Gate the prune + GC behind `opts.prune`**

Wrap the prune loop and `gc_orphan_art()`:

```rust
    let mut pruned = 0u64;
    if opts.prune {
        let canon_root = root;
        for track in db.list_tracks()? {
            if !Path::new(&track.backing_path).starts_with(canon_root) {
                continue;
            }
            if let Err(e) = std::fs::metadata(&track.backing_path)
                && e.kind() == std::io::ErrorKind::NotFound
            {
                db.delete_track(track.id)?;
                pruned += 1;
            }
        }
        db.gc_orphan_art()?;
    }
```

(`changed`/`run_pipeline`/`RevalidateStats` assembly stay as-is; `pruned` is now 0 unless `--prune`.)

- [ ] **Step 5: Run to verify pass**

Run: `cargo test -p musefs-core --test incremental_refresh revalidate_`
Expected: PASS (all three).

- [ ] **Step 6: Backfill-non-destructive regression test**

Add (V1 FLAC = a track row with no structural blocks; build it by inserting a track + tag directly via the DB, mirroring how `incremental_refresh.rs` already simulates a V1 row — reuse that pattern):

```rust
#[test]
fn revalidate_backfill_does_not_clobber_tags() {
    let dir = tempfile::tempdir().unwrap();
    let db = musefs_db::Db::open_in_memory().unwrap();
    let path = dir.path().join("a.flac");
    write_flac(&path, &["TITLE=OnDisk"], &[0xAA; 30]);
    musefs_core::scan_directory(&db, dir.path()).unwrap();
    let id = db.list_tracks().unwrap()[0].id;
    // Curate, then simulate a V1 row by clearing structural blocks.
    db.replace_tags(id, &[musefs_db::Tag::new("title", "Curated", 0)]).unwrap();
    db.set_structural_blocks(id, &[]).unwrap(); // V1: no structural blocks
    // Backing file UNCHANGED (same stamp) — only the backfill should fire.
    let stats = musefs_core::revalidate(&db, dir.path()).unwrap();
    assert_eq!(stats.updated, 1, "backfilled the missing structural blocks");
    let tags = db.get_tags(id).unwrap();
    assert_eq!(tags[0].value, "Curated", "backfill preserved the DB edit");
    assert!(!db.get_structural_blocks(id).unwrap().is_empty(), "blocks repopulated");
}
```

> Confirm `set_structural_blocks` is reachable on `&Db` from the test crate (it is part of `TrackSink`, which is private — if `Db::set_structural_blocks` is not public, simulate the V1 row another way the existing tests use, e.g. a dedicated test seam already present in `incremental_refresh.rs`). Match the existing file's approach.

Run: `cargo test -p musefs-core --test incremental_refresh revalidate_backfill`
Expected: PASS (this would FAIL on `main` — the bug this plan fixes).

- [ ] **Step 7: metrics-feature + clippy + re-anchor + full suite**

Run `python3 scripts/check_mutant_anchors.py` (check only) and hand-anchor any drifted `file:line:col` entries per their `# guard:` tags (see Task 1 Step 7 — `--fix` won't move these), then:
```bash
cargo test -p musefs-core
cargo test -p musefs-core --features metrics
cargo clippy -p musefs-core --all-targets
```
Expected: anchors clean, all PASS / clean.

- [ ] **Step 8: Commit**

```bash
git add musefs-core/src/scan.rs musefs-core/tests/incremental_refresh.rs .cargo/mutants.toml
git commit -m "$(cat <<'EOF'
feat(core): revalidate refreshes Layer A only, scoped to existing tracks

Revalidate now refreshes structural/serving facts for changed in-DB files
while preserving curated tags/art/binary-tags, ignores files not yet in the
DB (scan's job), and prunes gone tracks only with ScanOptions.prune. Fixes a
live bug where the V1/checksum backfill clobbered curated tags.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: CLI — `--force`, `revalidate` subcommand, `--prune`, deprecated alias

**Files:**
- Modify: `musefs-cli/src/lib.rs` (`Command::Scan`, new `Command::Revalidate`, `run_scan`, new `run_revalidate`, `run` dispatch, parse tests)

**Interfaces:**
- Consumes: `musefs_core::{scan_directory_with, revalidate_with, ScanOptions}`.
- Produces: `fn run_revalidate(db_path, targets, prune, jobs, follow_symlinks, quiet, checksum) -> Result<u64>`; `run_scan` gains a `force: bool` param.

- [ ] **Step 1: Write failing CLI parse tests**

In the `#[cfg(test)]` module of `musefs-cli/src/lib.rs` add:

```rust
#[test]
fn scan_accepts_force() {
    let cli = Cli::parse_from(["musefs", "scan", "/m", "--db", "/d", "--force"]);
    let Command::Scan { force, .. } = cli.command else { panic!("expected Scan") };
    assert!(force);
}

#[test]
fn revalidate_subcommand_parses_with_prune() {
    let cli = Cli::parse_from(["musefs", "revalidate", "/m", "--db", "/d", "--prune"]);
    let Command::Revalidate { prune, targets, .. } = cli.command else { panic!("expected Revalidate") };
    assert!(prune);
    assert_eq!(targets, vec![std::path::PathBuf::from("/m")]);
}

#[test]
fn scan_rejects_prune() {
    assert!(Cli::try_parse_from(["musefs", "scan", "/m", "--db", "/d", "--prune"]).is_err());
}

#[test]
fn revalidate_rejects_force() {
    assert!(Cli::try_parse_from(["musefs", "revalidate", "/m", "--db", "/d", "--force"]).is_err());
}
```

> The existing CLI tests destructure `Command::Scan { .. }` and exhaustively match `Command::{Scan,Mount,Vacuum}`. Adding a `Revalidate` variant breaks those matches — update each `match cli.command` / `let Command::… else` arm in the test module to add a `Command::Revalidate { .. } => …` arm (mirroring the existing `Command::Vacuum { .. } => unreachable!()` arms).

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p musefs-cli scan_accepts_force revalidate_subcommand_parses scan_rejects_prune revalidate_rejects_force`
Expected: FAIL (no `force` field, no `Revalidate` variant).

- [ ] **Step 3: Add `--force` to `Command::Scan`**

In the `Scan` variant, after the `revalidate` field add:

```rust
        /// Re-ingest files already in the DB, overwriting their curated tags/art
        /// with the file's embedded metadata. Off by default: bare `scan` only
        /// ingests files not yet in the DB.
        #[arg(long, env = "MUSEFS_FORCE", value_parser = clap::builder::BoolishValueParser::new())]
        force: bool,
```

Change the existing `revalidate` field's doc to mark it deprecated:

```rust
        /// DEPRECATED: use the `revalidate` subcommand. Forwards to it (now
        /// non-pruning). Removed next release.
        #[arg(long, env = "MUSEFS_REVALIDATE", value_parser = clap::builder::BoolishValueParser::new())]
        revalidate: bool,
```

- [ ] **Step 4: Add the `Revalidate` subcommand**

After the `Scan` variant (before `Mount`), add:

```rust
    /// Refresh tracks already in the store: re-probe files whose backing bytes
    /// changed (refreshing audio bounds / structural blocks / checksums) while
    /// preserving curated tags and art. Files not yet in the store are ignored
    /// (use `scan`). Never deletes anything unless `--prune`.
    Revalidate {
        /// One or more files or directories to revalidate (directories recurse).
        #[arg(required = true, num_args = 1..)]
        targets: Vec<PathBuf>,
        /// Path to the SQLite database.
        #[arg(long, env = "MUSEFS_DB")]
        db: PathBuf,
        /// Delete tracks whose backing file is gone (cascading tags/art) and GC
        /// orphaned art, scoped to the revalidated root. Off by default.
        #[arg(long, env = "MUSEFS_PRUNE", value_parser = clap::builder::BoolishValueParser::new())]
        prune: bool,
        #[arg(long, env = "MUSEFS_JOBS", default_value_t = 0)]
        jobs: usize,
        #[arg(long, env = "MUSEFS_FOLLOW_SYMLINKS", value_parser = clap::builder::BoolishValueParser::new())]
        follow_symlinks: bool,
        #[arg(long, short, env = "MUSEFS_QUIET", value_parser = clap::builder::BoolishValueParser::new())]
        quiet: bool,
        #[arg(long, value_enum, env = "MUSEFS_CHECKSUM", default_value_t = ChecksumMode::Fingerprint)]
        checksum: ChecksumMode,
    },
```

- [ ] **Step 5: Update `run_scan` (add `force`, deprecate the in-flag path)**

Change `run_scan`'s signature to add `force: bool` (after `revalidate`), and replace the `if revalidate { … } else { … }` body so the deprecated flag warns and delegates to the revalidate path, and the normal path threads `force`:

```rust
pub fn run_scan(
    db_path: &Path,
    targets: &[PathBuf],
    revalidate: bool,
    force: bool,
    jobs: usize,
    follow_symlinks: bool,
    quiet: bool,
    checksum: ChecksumMode,
    fast: bool,
    strict: bool,
) -> Result<u64> {
    if revalidate {
        if force {
            anyhow::bail!("--force and --revalidate are mutually exclusive");
        }
        log::warn!(
            "`scan --revalidate` is deprecated; use `revalidate` (now non-pruning — \
             add `--prune` to delete gone tracks). This alias will be removed next release."
        );
        // Deprecated alias inherits the new, non-pruning revalidate semantics.
        return run_revalidate(db_path, targets, false, jobs, follow_symlinks, quiet, checksum);
    }
    let strictness = match (fast, strict) {
        (true, true) => anyhow::bail!("--fast and --strict are mutually exclusive"),
        (true, false) => musefs_core::MatchStrictness::Fast,
        (false, true) => musefs_core::MatchStrictness::Strict,
        (false, false) => musefs_core::MatchStrictness::Auto,
    };
    let db =
        Db::open(db_path).with_context(|| format!("opening database at {}", db_path.display()))?;
    let reporter = ScanReporter::new(quiet);
    let opts = musefs_core::ScanOptions {
        jobs,
        follow_symlinks,
        progress: reporter.sink(),
        checksum: checksum.into(),
        strictness,
        force,
        ..Default::default()
    };
    let mut total_failed = 0u64;
    for target in targets {
        reporter.start_target();
        let start = Instant::now();
        let stats = musefs_core::scan_directory_with(&db, target, &opts)
            .with_context(|| format!("scanning {}", target.display()))?;
        total_failed += stats.failed;
        if !quiet {
            println!(
                "scanned {}: {} file(s), {} already present, skipped {}, failed {} in {}",
                target.display(),
                stats.scanned,
                stats.already_present,
                stats.skipped,
                stats.failed,
                HumanDuration(start.elapsed()),
            );
        }
    }
    reporter.finish();
    Ok(total_failed)
}
```

- [ ] **Step 6: Add `run_revalidate`**

Insert after `run_scan`:

```rust
/// Open the DB once and revalidate each target (refresh changed in-DB tracks'
/// structural data, preserving curated tags). With `prune`, delete tracks whose
/// backing file is gone under each target. Returns the total per-file `failed`
/// count across targets.
#[allow(clippy::too_many_arguments)]
pub fn run_revalidate(
    db_path: &Path,
    targets: &[PathBuf],
    prune: bool,
    jobs: usize,
    follow_symlinks: bool,
    quiet: bool,
    checksum: ChecksumMode,
) -> Result<u64> {
    let db =
        Db::open(db_path).with_context(|| format!("opening database at {}", db_path.display()))?;
    let reporter = ScanReporter::new(quiet);
    let opts = musefs_core::ScanOptions {
        jobs,
        follow_symlinks,
        progress: reporter.sink(),
        checksum: checksum.into(),
        prune,
        ..Default::default()
    };
    let mut total_failed = 0u64;
    for target in targets {
        reporter.start_target();
        let start = Instant::now();
        let stats = musefs_core::revalidate_with(&db, target, &opts)
            .with_context(|| format!("revalidating {}", target.display()))?;
        total_failed += stats.failed;
        if !quiet {
            println!(
                "revalidated {}: {} updated, {} unchanged, {} pruned, {} failed in {}",
                target.display(),
                stats.updated,
                stats.unchanged,
                stats.pruned,
                stats.failed,
                HumanDuration(start.elapsed()),
            );
        }
    }
    reporter.finish();
    Ok(total_failed)
}
```

- [ ] **Step 7: Update the `run` dispatch**

Replace the `Command::Scan { … }` arm's `run_scan(...)` call to pass `force`, and add a `Command::Revalidate` arm:

```rust
        Command::Scan {
            targets, db, revalidate, force, jobs, follow_symlinks, quiet, checksum, fast, strict,
        } => {
            let failed = run_scan(
                &db, &targets, revalidate, force, jobs, follow_symlinks, quiet, checksum, fast, strict,
            )?;
            Ok(if failed > 0 { ExitCode::from(2) } else { ExitCode::SUCCESS })
        }
        Command::Revalidate { targets, db, prune, jobs, follow_symlinks, quiet, checksum } => {
            let failed = run_revalidate(&db, &targets, prune, jobs, follow_symlinks, quiet, checksum)?;
            Ok(if failed > 0 { ExitCode::from(2) } else { ExitCode::SUCCESS })
        }
```

- [ ] **Step 8: Run the parse tests + full CLI suite**

Run: `cargo test -p musefs-cli`
Expected: PASS (including the new parse tests and the updated exhaustive matches). Fix any remaining `match cli.command` arms the compiler flags.

- [ ] **Step 9: Commit**

```bash
git add musefs-cli/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(cli): add `revalidate` subcommand, `scan --force`, `revalidate --prune`

scan gains --force (re-seed existing tracks). revalidate is promoted to its
own subcommand with --prune. `scan --revalidate` is a deprecated, warned,
non-pruning alias that forwards to revalidate; --force+--revalidate errors.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 7: CLI end-to-end integration tests

**Files:**
- Modify: `musefs-cli/tests/scan.rs`, `musefs/tests/cli_process.rs`

**Interfaces:** consumes the built `musefs` binary / `run`/`run_scan`/`run_revalidate`.

- [ ] **Step 1: Write failing e2e tests**

Add to `musefs-cli/tests/scan.rs` (reuse the file's existing harness for invoking a scan against a temp library + DB):

```rust
#[test]
fn cli_bare_scan_then_revalidate_preserves_tags() {
    // 1. scan a one-file library; 2. edit the tag in the DB; 3. bare-scan again
    //    → tag preserved; 4. revalidate → tag preserved; 5. scan --force → reseeded.
    // Build with the file's existing helpers (temp dir + fixture + Db path).
    // Assert via the DB read-back helper this file already uses.
}

#[test]
fn cli_revalidate_prune_deletes_gone_track() {
    // scan, delete the backing file, `revalidate` (no prune) keeps the row,
    // `revalidate --prune` removes it.
}
```

> Flesh these out using the concrete helpers in `musefs-cli/tests/scan.rs` (it already drives `run_scan`/the binary). Mirror Task 4/5's assertions at the CLI layer.

- [ ] **Step 2: Run to verify failure, then confirm they pass against the implementation**

Run: `cargo test -p musefs-cli --test scan cli_bare_scan_then_revalidate cli_revalidate_prune`
Expected: PASS once written against the Task 6 binary (these exercise wiring, which is already implemented — if they fail, the failure is a wiring bug to fix here).

- [ ] **Step 3: Process-level smoke (deprecation warning + exit codes)**

In `musefs/tests/cli_process.rs` add a test asserting `scan --revalidate` prints the deprecation warning to stderr (run with `RUST_LOG=warn`) and that `scan --prune` / `revalidate --force` exit non-zero (clap usage error). Reuse the file's `Command::cargo_bin("musefs")` harness.

- [ ] **Step 4: Full workspace test**

Run: `cargo test`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add musefs-cli/tests/scan.rs musefs/tests/cli_process.rs
git commit -m "$(cat <<'EOF'
test(cli): e2e additive scan, revalidate preserve/prune, deprecation warning

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 8: Python `run_scan` wrapper + beets callers + re-vendor

**Files:**
- Modify: `contrib/python-musefs/src/musefs_common/scan.py`
- Re-vendor: `contrib/picard/musefs/_common/scan.py`
- Modify: `contrib/beets/beetsplug/musefs.py`
- Test: `contrib/python-musefs/tests/…` (argv), `contrib/beets/tests/…` (caller)

**Interfaces:**
- Produces: `run_scan(binary, db_path, target, *, revalidate=False, force=False, prune=False, timeout=None)` emitting `revalidate <targets> --db <db> [--prune]` when `revalidate`, else `scan <targets> --db <db> [--force]`.

- [ ] **Step 1: Write the failing argv test**

In the python-musefs test that covers `run_scan` (grep `contrib/python-musefs/tests` for the existing argv/subprocess test; if none, add `test_scan.py`), monkeypatch `subprocess.run` to capture argv and assert:

```python
def test_run_scan_force_appends_force(monkeypatch):
    captured = {}
    def fake_run(argv, **kw):
        captured["argv"] = argv
        class R: returncode = 0; stdout = b""; stderr = b""
        return R()
    monkeypatch.setattr(subprocess, "run", fake_run)
    run_scan("musefs", "/db", "/m", force=True)
    assert captured["argv"] == ["musefs", "scan", "/m", "--db", "/db", "--force"]

def test_run_scan_revalidate_uses_subcommand_and_prune(monkeypatch):
    captured = {}
    def fake_run(argv, **kw):
        captured["argv"] = argv
        class R: returncode = 0; stdout = b""; stderr = b""
        return R()
    monkeypatch.setattr(subprocess, "run", fake_run)
    run_scan("musefs", "/db", "/m", revalidate=True, prune=True)
    assert captured["argv"] == ["musefs", "revalidate", "/m", "--db", "/db", "--prune"]
```

- [ ] **Step 2: Run to verify failure**

Run: `contrib/python-musefs/.venv/bin/python -m pytest contrib/python-musefs/tests -k run_scan -q` (or the project's configured runner). Expected: FAIL (no `force`/`prune` kwargs; revalidate still emits `scan --revalidate`).

- [ ] **Step 3: Update `run_scan` in `musefs_common/scan.py`**

Replace the signature and argv construction:

```python
def run_scan(binary, db_path, target, *, revalidate=False, force=False, prune=False, timeout=None):
    """Run the musefs scanner once for ``target`` (a path or iterable of paths),
    all targets under one process (one DB open). Modes:

    - default: ``<binary> scan <targets...> --db <db>`` — additive; ingests only
      files not already in the store, never overwriting curated tags.
    - ``force``: appends ``--force`` — re-ingests existing tracks, re-seeding tags
      from the file (the only way to overwrite curated tags via a scan).
    - ``revalidate``: ``<binary> revalidate <targets...> --db <db>`` — refreshes
      structural data for changed in-store files, preserving curated tags;
      ``prune`` appends ``--prune`` to delete rows whose backing file is gone.

    Raises ``ScanError`` (kind in ``"not_found" | "timeout" | "failed"``)."""
    if isinstance(target, (str, os.PathLike)):
        targets = [target]
    else:
        targets = list(target)
    display = str(targets[0]) if len(targets) == 1 else f"{len(targets)} target(s)"
    if revalidate:
        argv = [binary, "revalidate", *(str(t) for t in targets), "--db", str(db_path)]
        if prune:
            argv.append("--prune")
    else:
        argv = [binary, "scan", *(str(t) for t in targets), "--db", str(db_path)]
        if force:
            argv.append("--force")
    try:
        result = subprocess.run(argv, capture_output=True, timeout=timeout)
    except FileNotFoundError as exc:
        raise ScanError("not_found", binary=binary, target=display) from exc
    except subprocess.TimeoutExpired as exc:
        raise ScanError("timeout", binary=binary, target=display, timeout=timeout) from exc
    if result.returncode != 0:
        raise ScanError(
            "failed",
            binary=binary,
            target=display,
            returncode=result.returncode,
            stderr=result.stderr.decode(errors="replace").strip(),
        )
```

- [ ] **Step 4: Run the argv tests to verify pass**

Run: `contrib/python-musefs/.venv/bin/python -m pytest contrib/python-musefs/tests -k run_scan -q`
Expected: PASS.

- [ ] **Step 5: Re-vendor the Picard copy**

The Picard plugin vendors `musefs_common`. Re-run the project's vendor step (grep `contrib/picard` / `CLAUDE.md` for the re-vendor command; per the repo it is the same mechanism the schema/`_common` sync uses), then verify the drift-test:

Run: the Picard vendor-sync test (e.g. `… -m pytest contrib/picard -k vendor_sync`).
Expected: PASS (the Picard `_common/scan.py` is byte-identical to the source).

> If there is no automated vendor script, copy `contrib/python-musefs/src/musefs_common/scan.py` over `contrib/picard/musefs/_common/scan.py` verbatim and re-run the drift-test.

- [ ] **Step 6: Update beets callers (reset-to-backing → `--force`; `--revalidate` → prune)**

In `contrib/beets/beetsplug/musefs.py`:

`_run_scan` gains `force`/`prune` and forwards them:

```python
    def _run_scan(self, db_path, targets, *, revalidate=False, force=False, prune=False):
        """Run the musefs scanner once for the whole batch. The autoscan reset
        uses ``force`` (re-seed existing tracks to backing so the managed-tag
        merge re-applies on fresh `B`). ``revalidate`` runs the maintenance pass;
        ``prune`` deletes rows whose backing file is gone. Raises ui.UserError."""
        binary = self._bin()
        try:
            run_scan(
                binary, db_path, targets,
                revalidate=revalidate, force=force, prune=prune,
                timeout=SCAN_TIMEOUT_SECONDS,
            )
        except ScanError as exc:
            raise self._scan_user_error(exc)
```

Active command path (`_command`, ~lines 101–107): the autoscan/full-sync reset must force; `--revalidate` maps to a **pruning** revalidate. **Keep the existing outer `… and not opts.dry_run` gate and the `targets` computation** — only the inner call shape changes. A `--prune` under `--dry-run` would delete rows, so the dry-run gate is load-bearing. The current block is:

```python
        if (self._autoscan() or revalidate) and not opts.dry_run:
            targets = (
                [os.fsdecode(i.path) for i in items] if query else [os.fsdecode(lib.directory)]
            )
            if revalidate:
                self._run_scan(db_path, targets, revalidate=True, prune=True)
            else:
                self._run_scan(db_path, targets, force=True)
```

(Only the two `_run_scan(...)` calls are new — the surrounding `(autoscan or revalidate) and not dry_run` guard and the `targets = …` computation are unchanged from today.)

Passive reconcile path (`_reconcile_pending`, ~line 145): the per-item reset must force:

```python
            if self._autoscan():
                self._run_scan(db_path, [os.fsdecode(i.path) for i in items], force=True)
```

Update the `--revalidate` option help (line 71) to read `forward to the \`musefs revalidate\` subcommand, pruning rows whose backing file is gone…`.

- [ ] **Step 7: Write/adjust beets caller tests**

In the beets test that asserts the scan invocation (grep `contrib/beets/tests` for the `_run_scan`/`run_scan` mock), assert: autoscan full-sync calls `run_scan(..., force=True)`; `--revalidate` calls `run_scan(..., revalidate=True, prune=True)`; passive reconcile calls `force=True`. Use the venv runner.

Run: `contrib/beets/.venv/bin/python -m pytest contrib/beets/tests -k "scan or revalidate or autoscan" -q`
Expected: PASS.

- [ ] **Step 8: ruff + commit**

```bash
ruff check contrib && ruff format --check contrib
git add contrib/python-musefs/src/musefs_common/scan.py contrib/picard/musefs/_common/scan.py contrib/beets/beetsplug/musefs.py contrib/python-musefs/tests contrib/beets/tests
git commit -m "$(cat <<'EOF'
feat(contrib): wrappers emit new scan/revalidate verbs; beets resets via --force

run_scan gains force/prune and emits the `revalidate` subcommand. beets
autoscan resets existing tracks with `scan --force` (preserving its
reset-then-merge model); `beet musefs --revalidate` maps to `revalidate
--prune`. Re-vendored the Picard _common copy.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 9: Documentation

**Files:**
- Modify: `docs/src/architecture/tree-scanning.md`, `docs/src/architecture/store.md`, `README.md`, `docs/src/changelog.md`, `CLAUDE.md`, contrib integration docs.

- [ ] **Step 1: Architecture — scan vs revalidate + two-layer model**

In `docs/src/architecture/tree-scanning.md`, document: bare `scan` is additive (files not in the DB); `--force` re-seeds existing tracks; `revalidate` refreshes Layer A (audio bounds / stamp / checksums / structural blocks) for changed in-DB files while preserving Layer B (tags / art / binary tags); `--prune` is the only deletion. Add the two-layer A/B definition.

- [ ] **Step 2: Store contract**

In `docs/src/architecture/store.md`, update the external-writer contract note: a re-scan no longer overwrites curated tags; only `scan --force` does; only `revalidate --prune` deletes tracks.

- [ ] **Step 3: README usage**

In `README.md`, replace `scan --revalidate` usage with the `scan` / `scan --force` / `revalidate` / `revalidate --prune` matrix (mirror the spec's command table). Note `--revalidate` is deprecated.

- [ ] **Step 4: Changelog (breaking)**

In `docs/src/changelog.md` (Keep-a-Changelog headers — do not use GFM admonitions; linkcheck is sensitive), add an **Unreleased** entry under a `### Changed` / `### Deprecated` split: bare `scan` is now additive (BREAKING — old full re-import is `scan --force`); `revalidate` is a subcommand and no longer prunes by default (use `--prune`); `scan --revalidate` deprecated, removed next release; fixed a bug where the revalidate backfill clobbered curated tags.

- [ ] **Step 5: CLAUDE.md everyday commands + wrapper docstrings**

If `CLAUDE.md`'s "Everyday commands" references scan invocation, update. Confirm the wrapper docstrings edited in Task 8 read correctly. Update contrib integration docs (`docs/src/integrations/*`, beets/Picard) that mention `scan --revalidate`.

- [ ] **Step 6: Commit (docs-only → cargo gate skips)**

```bash
git add docs README.md CLAUDE.md
git commit -m "$(cat <<'EOF'
docs: scan is additive, revalidate refreshes Layer A only

Document the two-layer model, the scan/revalidate verb split, --force /
--prune, and the breaking change + deprecation in the changelog.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 10: Full-suite verification + final gates

- [ ] **Step 1: Whole workspace**

Run: `cargo fmt --all --check && cargo clippy --all-targets && cargo test`
Expected: clean / all pass.

- [ ] **Step 2: metrics feature**

Run: `cargo test -p musefs-core --features metrics`
Expected: PASS (getattr/read counts unaffected; confirms scan-path moves didn't disturb instrumentation).

- [ ] **Step 3: Mutation in-diff gate (local)**

Per the repo's local gate (see memory: `cargo mutants --in-place`, serial, `/tmp` TMPDIR, sanity-check the diff): run the in-diff mutation gate over the changed lines and record the bench/result. Add `exclude_re` / desc-anchored excludes for any hang-class or unobservable mutants surfaced, per the existing `mutants.toml` precedent — do not weaken assertions to kill them.

- [ ] **Step 4: Python suites (venvs)**

Run: `contrib/beets/.venv/bin/python -m pytest contrib/beets -q` and the Picard suite via the system-package invocation (`/usr/bin/python3` + `PYTHONPATH` to `/usr/lib/picard`, per the repo's Picard test note) — default pytest silently skips the real-Picard tests.
Expected: PASS (or documented skips).

- [ ] **Step 5: Finish the branch**

Use the `superpowers:finishing-a-development-branch` skill to decide merge/PR. PR body must include `Fixes #<issue>` if a tracking issue exists, and call out the **breaking** scan-semantics change.

---

## Self-Review

**Spec coverage:**
- Two-layer model → Task 1 (`refresh_structural_into`), Tasks 4/5 (enforced).
- Command matrix (scan additive / `--force` / revalidate Layer-A / `--prune`) → Tasks 4, 5, 6.
- Moved-file retarget preserved under bare scan → Task 4 (pre-filter removes only known paths; unknown/moved paths still flow through `ingest_unit`'s retarget branch with `WritePolicy::Full`). **Add an explicit moved-file CLI/integration assertion** — see note below.
- Deprecated alias mechanics (warn, non-pruning, conflict errors, env binding) → Task 6 (`run_scan` warn+delegate, `--force`+`--revalidate` bail, clap rejects cross-flags; `MUSEFS_REVALIDATE` stays on `scan`, `MUSEFS_FORCE` on scan, `MUSEFS_PRUNE` on revalidate).
- Backfill-clobber bug fix → Task 5 Step 6.
- `already_present` distinct from `skipped` → Task 3.
- Vendored wrapper fan-out + beets autoscan `--force` → Task 8.
- Docs/changelog/breaking → Task 9.

**Gap found & fixed inline:** the moved-file case has a unit-level guarantee (Task 4 pre-filter only drops *known* paths) but no explicit test. **Add to Task 4 Step 1** a third test:

```rust
#[test]
fn bare_scan_retargets_moved_file_preserving_tags() {
    let dir = tempfile::tempdir().unwrap();
    let db = musefs_db::Db::open_in_memory().unwrap();
    let a = dir.path().join("a.flac");
    write_flac(&a, &["TITLE=T"], &[0xAA; 30]);
    musefs_core::scan_directory(&db, dir.path()).unwrap(); // default checksum = fingerprint
    let id = db.list_tracks().unwrap()[0].id;
    db.replace_tags(id, &[musefs_db::Tag::new("title", "Curated", 0)]).unwrap();
    // Move the file (same bytes) → old path gone, new path present.
    std::fs::rename(&a, dir.path().join("b.flac")).unwrap();
    musefs_core::scan_directory(&db, dir.path()).unwrap();
    let tracks = db.list_tracks().unwrap();
    assert_eq!(tracks.len(), 1, "retargeted, not duplicated");
    assert!(tracks[0].backing_path.ends_with("b.flac"), "row retargeted to new path");
    let tags = db.get_tags(tracks[0].id).unwrap();
    assert_eq!(tags[0].value, "Curated", "move preserved curated tags");
}
```

(Default `scan_directory` uses `ScanOptions::default()` → `ChecksumTier::Fingerprint`, so the fingerprint exists for the retarget match. Confirm `ScanOptions::default().checksum` is `Fingerprint`; if it is `None`, set `checksum: ChecksumTier::Fingerprint` via `scan_directory_with` in this test.)

**Placeholder scan:** Test bodies in Tasks 7 are described, not coded, because they must bind to each test file's existing private harness (fixture + DB-path helpers) — every such step names the exact helpers to reuse and the exact assertions to mirror from Tasks 4/5. All *new-symbol* code (core functions, CLI variants, wrapper) is shown in full.

**Type consistency:** `WritePolicy` (Task 1) used identically in Tasks 1–2. `refresh_structural_into` signature matches `ingest_into`'s. `run_scan`'s new `force` param ordering (after `revalidate`) matches the `run` dispatch call (Task 6 Step 7). `run_revalidate` param order matches its call site. `run_scan(force=…, prune=…)` Python kwargs match between wrapper (Task 8 Step 3) and beets callers (Task 8 Step 6). `ScanStats.already_present` defined in Task 3, set in Task 4, printed in Task 6 Step 5.

**Open verification deferred to the implementer (named at each site):** exact `musefs-db` read-back/helper names (`get_tags`, `get_structural_blocks`, `Db::open_in_memory`, `list_tracks`/`tracks`, `set_structural_blocks` visibility) — each test step says "match the sibling tests in this file." This is deliberate: the plan must not guess DB method names that vary; it pins the *behavior* and points at the existing pattern.
