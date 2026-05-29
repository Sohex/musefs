# Phase 4b — DB Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Drive `musefs-db` mutation survivors toward zero — unblock the 20 unviable `Ok(Default::default())` mutants via a feature-gated `Default` affordance, kill the 2 known survivors, and close coverage findings #10/#11/#12 — completing Phase 4.

**Architecture:** A new `mutants` Cargo feature on `musefs-db` carries every `Default` impl the unviable mutants need (a test-only `Default for Db` that opens an in-memory, unmigrated connection, plus `cfg_attr` `derive(Default)` on the model structs and `Format`). The campaign leg of `scripts/mutants.sh` builds with that feature so the 20 become viable; additive integration tests close the coverage findings. Work splits into two PRs: **PR 1** lands the feature + tests + campaign-independent docs and dispatches the cargo-mutants campaign; **PR 2** consumes the campaign artifact to do the newly-viable survivor sweep.

**Tech Stack:** Rust, `rusqlite` (bundled SQLite), `cargo-mutants` (CI-only, stable toolchain), `tempfile` dev-dep.

**Spec:** `docs/superpowers/specs/test-audit-remediation/2026-05-29-phase4b-db-hardening-design.md`

---

## Verification convention (read once before starting)

cargo-mutants is **not available locally**. Every "kill" is verified by **hand-apply**:

1. Run the new test → it passes (production code is correct).
2. Locate the mutated construct **by pattern** (never by raw line number — inventory line numbers are approximate), apply the exact mutation, rerun **just that test** → it must **fail** (failed assertion *or* panic).
3. `git checkout -- <file>` to revert, rerun → passes again. **Never leave a mutation applied.**

If step 2 still passes and the mutation provably yields identical behavior, record an **equivalent mutant** (one-line rationale) instead of contriving a test.

## File Structure

PR 1 touches:

- `musefs-db/Cargo.toml` — declare the `mutants` feature (new `[features]` section).
- `musefs-db/src/models.rs` — `cfg_attr` `derive(Default)` on `Track`, `Art`, `ArtMeta`, `Tag`, `TrackArt`; `derive(Default)` + a `#[default]` variant on `Format`. (NOT `NewTrack`/`NewArt` — input types.)
- `musefs-db/src/lib.rs` — feature-gated `impl Default for Db` (in-memory, unmigrated, FK/timeout pragmas set, no `migrate`).
- `scripts/mutants.sh` — add `--features mutants` to the `musefs-db` leg.
- `.github/workflows/ci.yml` — add a `cargo test -p musefs-db --features mutants` step to the `check` job.
- `musefs-db/tests/schema.rs` — feature-gated `user_version` kill test (B2).
- `musefs-db/tests/tracks.rs` — `upsert_track` per-column conflict test (B3).
- `musefs-db/tests/art.rs` — gc-keeps/count, shared-art survival, `set_track_art` replace tests (B4).
- `musefs-db/tests/tags.rs` — `tags_grouped` empty + multivalue-order tests (B5).
- `docs/superpowers/specs/test-audit-remediation/2026-05-29-mutation-inventory.md` — annotate db rows; correct the unviable note; record #11/#12 framing.
- `docs/superpowers/specs/test-audit-remediation/2026-05-29-remediation-tracking.md` — Phase 4b status.

PR 2 touches: the inventory + tracking docs (final annotations) and possibly new `#[cfg(feature = "mutants")]` kill tests in the db test files (set determined by the campaign artifact).

---

## PR 1 — feature, kills, coverage, docs

### Task 1: Declare the `mutants` feature and the gated model `Default`s

**Files:**
- Modify: `musefs-db/Cargo.toml`
- Modify: `musefs-db/src/models.rs`

- [ ] **Step 1: Add the feature to `Cargo.toml`**

Insert a `[features]` section (after the `[dependencies]` block, before `[dev-dependencies]`):

```toml
[features]
# Test-only: gates `Default` impls (Db + model structs) so cargo-mutants'
# `Ok(Default::default())` mutants compile. Named after the activity that needs
# it, mirroring musefs-format's `fuzzing` feature. Not for production use.
mutants = []
```

- [ ] **Step 2: Gate `derive(Default)` on the model structs**

In `musefs-db/src/models.rs`, add a `cfg_attr` line beneath each existing derive. For the `Format` enum, also mark a default variant:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "mutants", derive(Default))]
pub enum Format {
    #[cfg_attr(feature = "mutants", default)]
    Flac,
    Mp3,
    M4a,
    Opus,
    Vorbis,
    OggFlac,
    Wav,
}
```

Then add `#[cfg_attr(feature = "mutants", derive(Default))]` immediately above each of these structs' opening line (keep their existing `#[derive(...)]`):

- `pub struct Track {` (line ~69)
- `pub struct Tag {` (line ~100)
- `pub struct Art {` (line ~117)
- `pub struct ArtMeta {` (line ~128)
- `pub struct TrackArt {` (line ~136)

Do **not** add it to `NewTrack` or `NewArt` — they are input types, never returned as a `Default::default()` value by a mutated function.

- [ ] **Step 3: Verify both feature states compile**

Run: `cargo build -p musefs-db && cargo build -p musefs-db --features mutants`
Expected: both succeed. (Feature-off build is byte-for-byte unaffected; feature-on adds the `Default` impls.)

- [ ] **Step 4: Verify clippy is clean under the feature**

Run: `cargo clippy -p musefs-db --features mutants --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 5: Commit**

```bash
git add musefs-db/Cargo.toml musefs-db/src/models.rs
git commit -m "feat(db): add mutants feature gating model Default derives

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 2: Feature-gated `Default for Db` + the `user_version` kill (B2)

**Files:**
- Modify: `musefs-db/src/lib.rs`
- Test: `musefs-db/tests/schema.rs`

- [ ] **Step 1: Write the failing test**

Append to `musefs-db/tests/schema.rs`:

```rust
#[cfg(feature = "mutants")]
#[test]
fn default_db_is_unmigrated_version_zero() {
    // Kills `Db::user_version -> Ok(1)`: Default opens an UNMIGRATED in-memory
    // connection, so user_version is 0, distinct from the always-migrated 1.
    let db = Db::default();
    assert_eq!(db.user_version().unwrap(), 0);
}
```

- [ ] **Step 2: Run the test to verify it fails (no `Default` yet)**

Run: `cargo test -p musefs-db --features mutants default_db_is_unmigrated_version_zero`
Expected: **compile error** — `Db: Default` is not satisfied / `Db::default` not found.

- [ ] **Step 3: Implement `Default for Db`**

In `musefs-db/src/lib.rs`, add immediately after the closing `}` of the `impl Db { … }` block:

```rust
#[cfg(feature = "mutants")]
impl Default for Db {
    /// Test-only (the `mutants` feature). An in-memory, **unmigrated** connection
    /// (so `user_version == 0`, distinct from the always-migrated `1`). Sets the
    /// FK/busy-timeout pragmas like a real connection, but runs no migration, so it
    /// has **no schema**. Use only for the version-0 kill and to let
    /// `Ok(Default::default())` mutants compile; behavioral tests use
    /// `open_in_memory()`.
    fn default() -> Self {
        let conn = Connection::open_in_memory().expect("in-memory sqlite open");
        conn.busy_timeout(Duration::from_secs(5))
            .expect("set busy_timeout");
        conn.pragma_update(None, "foreign_keys", true)
            .expect("enable foreign_keys");
        Db { conn, path: None }
    }
}
```

(`Connection`, `Duration` are already imported in `lib.rs`.)

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p musefs-db --features mutants default_db_is_unmigrated_version_zero`
Expected: **PASS**.

- [ ] **Step 5: Hand-apply verify the kill**

In `musefs-db/src/lib.rs`, temporarily replace the `user_version` body so it returns `Ok(1)`:

```rust
pub fn user_version(&self) -> Result<i64> {
    Ok(1)
}
```

Run: `cargo test -p musefs-db --features mutants default_db_is_unmigrated_version_zero`
Expected: **FAIL** (`assertion left == right failed: 1 vs 0`).
Then revert: `git checkout -- musefs-db/src/lib.rs` and rerun → PASS.

- [ ] **Step 6: Commit**

```bash
git add musefs-db/src/lib.rs musefs-db/tests/schema.rs
git commit -m "test(db): kill user_version Ok(1) mutant via mutants-gated Default

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 3: Wire the `mutants` feature into the campaign and CI

**Files:**
- Modify: `scripts/mutants.sh`
- Modify: `.github/workflows/ci.yml`

- [ ] **Step 1: Add `--features mutants` to the db campaign leg**

In `scripts/mutants.sh`, change the `musefs-db` case (the `run_crate musefs-db …` invocation) so the first line reads:

```bash
      run_crate musefs-db --test-workspace=true --features mutants \
        --file musefs-db/src/schema.rs \
        --file musefs-db/src/lib.rs \
        --file musefs-db/src/tracks.rs \
        --file musefs-db/src/art.rs \
        --file musefs-db/src/tags.rs
```

(Mirrors the format leg's existing `--features fuzzing`.)

- [ ] **Step 2: Add the gated-test step to CI**

In `.github/workflows/ci.yml`, in the `check` job, add a step immediately after the "Property tests (fuzzing feature)" step (after line ~35):

```yaml
      - name: DB mutants-feature tests
        run: cargo test -p musefs-db --features mutants
```

- [ ] **Step 3: Verify the harness script parses and the feature wiring is valid**

Run: `bash -n scripts/mutants.sh && MUTANTS_LIST=1 scripts/mutants.sh musefs-db | head -5`
Expected: no syntax error; cargo-mutants enumerates db mutants (the `--list` path builds nothing). If cargo-mutants is not installed locally, `bash -n` passing is sufficient; the list step is best-effort.

- [ ] **Step 4: Verify the gated test step runs locally**

Run: `cargo test -p musefs-db --features mutants`
Expected: PASS (includes `default_db_is_unmigrated_version_zero`).

- [ ] **Step 5: Commit**

```bash
git add scripts/mutants.sh .github/workflows/ci.yml
git commit -m "ci(db): build the mutants campaign and a CI test leg with --features mutants

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 4: Finding #10 — `upsert_track` per-column conflict update (B3)

**Files:**
- Test: `musefs-db/tests/tracks.rs`

> Note: `delete_track` leaving the `art` row is **already covered** by `delete_track_cascades_tags_and_track_art` (`tests/tracks.rs:110-111`). Do not re-add it. The real #10 gap is that `upsert_updates_existing_row_keeping_same_id` checks only `id` + `audio_offset`, not every conflict-updated column.

- [ ] **Step 1: Write the failing test**

Append to `musefs-db/tests/tracks.rs`:

```rust
#[test]
fn upsert_conflict_updates_all_mutable_columns() {
    let db = Db::open_in_memory().unwrap();
    let id = db.upsert_track(&new_track("/m/a.flac")).unwrap();

    // Same backing_path => ON CONFLICT update path; change every mutable column.
    let changed = NewTrack {
        backing_path: "/m/a.flac".to_string(),
        format: Format::Mp3,
        audio_offset: 222,
        audio_length: 333,
        backing_size: 444,
        backing_mtime: 555,
    };
    let id2 = db.upsert_track(&changed).unwrap();
    assert_eq!(id, id2, "conflict update must keep the same id");

    let t = db.get_track(id).unwrap().expect("track");
    assert_eq!(t.format, Format::Mp3);
    assert_eq!(t.audio_offset, 222);
    assert_eq!(t.audio_length, 333);
    assert_eq!(t.backing_size, 444);
    assert_eq!(t.backing_mtime, 555);
}
```

- [ ] **Step 2: Run the test to verify it passes (production is correct)**

Run: `cargo test -p musefs-db --test tracks upsert_conflict_updates_all_mutable_columns`
Expected: **PASS**.

- [ ] **Step 3: Hand-apply verify it pins the conflict-update columns**

In `musefs-db/src/tracks.rs`, in `upsert_track`'s `ON CONFLICT(backing_path) DO UPDATE SET` clause, temporarily delete the `audio_length = excluded.audio_length,` assignment (and separately try `backing_size`/`backing_mtime`).
Run the test → Expected: **FAIL** (the dropped column keeps its old value).
Revert: `git checkout -- musefs-db/src/tracks.rs`, rerun → PASS.

- [ ] **Step 4: Commit**

```bash
git add musefs-db/tests/tracks.rs
git commit -m "test(db): pin upsert_track conflict update across all mutable columns (#10)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 5: Finding #11 — art GC and link semantics (B4)

**Files:**
- Test: `musefs-db/tests/art.rs`

> Reframed: `gc_orphan_art` is one `DELETE … WHERE id NOT IN (…)` — no in-process race. `upsert_art` dedup id-stability is already covered by `identical_bytes_dedup_to_one_row`. The gaps are the *positive* GC cases and `set_track_art` replace semantics.

- [ ] **Step 1: Write the failing tests**

Append to `musefs-db/tests/art.rs`:

```rust
#[test]
fn gc_keeps_referenced_art_and_returns_removed_count() {
    let db = Db::open_in_memory().unwrap();
    let track = db.upsert_track(&new_track("/m/a.flac")).unwrap();
    let kept = db.upsert_art(&jpeg(vec![1, 2, 3])).unwrap();
    let orphan = db.upsert_art(&jpeg(vec![4, 5, 6])).unwrap();
    db.set_track_art(
        track,
        &[TrackArt { art_id: kept, picture_type: 3, description: String::new(), ordinal: 0 }],
    )
    .unwrap();

    let removed = db.gc_orphan_art().unwrap();
    assert_eq!(removed, 1, "exactly the one orphan is removed");
    assert!(db.get_art(kept).unwrap().is_some(), "referenced art survives gc");
    assert!(db.get_art(orphan).unwrap().is_none(), "orphan is removed");
}

#[test]
fn shared_art_survives_until_last_reference_gone() {
    let db = Db::open_in_memory().unwrap();
    let t1 = db.upsert_track(&new_track("/m/a.flac")).unwrap();
    let t2 = db.upsert_track(&new_track("/m/b.flac")).unwrap();
    let art = db.upsert_art(&jpeg(vec![7, 7, 7])).unwrap();
    let link = |ord| TrackArt { art_id: art, picture_type: 3, description: String::new(), ordinal: ord };
    db.set_track_art(t1, &[link(0)]).unwrap();
    db.set_track_art(t2, &[link(0)]).unwrap();

    // Drop one reference: still linked by t2 => survives gc.
    db.set_track_art(t1, &[]).unwrap();
    assert_eq!(db.gc_orphan_art().unwrap(), 0);
    assert!(db.get_art(art).unwrap().is_some());

    // Drop the last reference: now an orphan.
    db.set_track_art(t2, &[]).unwrap();
    assert_eq!(db.gc_orphan_art().unwrap(), 1);
    assert!(db.get_art(art).unwrap().is_none());
}

#[test]
fn set_track_art_replaces_links() {
    let db = Db::open_in_memory().unwrap();
    let t = db.upsert_track(&new_track("/m/a.flac")).unwrap();
    let a = db.upsert_art(&jpeg(vec![1])).unwrap();
    let b = db.upsert_art(&jpeg(vec![2])).unwrap();

    db.set_track_art(
        t,
        &[
            TrackArt { art_id: a, picture_type: 3, description: "front".to_string(), ordinal: 0 },
            TrackArt { art_id: b, picture_type: 4, description: "back".to_string(), ordinal: 1 },
        ],
    )
    .unwrap();
    assert_eq!(db.get_track_art(t).unwrap().len(), 2);

    // Replace: a single, re-described link (relink + reorder).
    db.set_track_art(
        t,
        &[TrackArt { art_id: b, picture_type: 3, description: "now-front".to_string(), ordinal: 0 }],
    )
    .unwrap();
    let got = db.get_track_art(t).unwrap();
    assert_eq!(got.len(), 1, "old links are cleared before insert");
    assert_eq!(got[0].art_id, b);
    assert_eq!(got[0].description, "now-front");

    // Empty items clears all links.
    db.set_track_art(t, &[]).unwrap();
    assert!(db.get_track_art(t).unwrap().is_empty());
}
```

- [ ] **Step 2: Run the tests to verify they pass**

Run: `cargo test -p musefs-db --test art gc_keeps_referenced_art_and_returns_removed_count shared_art_survives_until_last_reference_gone set_track_art_replaces_links`
Expected: all **PASS**.

- [ ] **Step 3: Hand-apply spot-checks**

- In `musefs-db/src/art.rs::gc_orphan_art`, change `NOT IN` to `IN` → `gc_keeps_referenced_art_and_returns_removed_count` must FAIL. Revert.
- In `set_track_art`, comment out the `DELETE FROM track_art WHERE track_id = ?1` statement → `set_track_art_replaces_links` must FAIL (old links linger). Revert with `git checkout -- musefs-db/src/art.rs`.

Each: rerun after revert → PASS.

- [ ] **Step 4: Commit**

```bash
git add musefs-db/tests/art.rs
git commit -m "test(db): pin gc_orphan_art positive cases + set_track_art replace semantics (#11)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 6: Finding #12 — `tags_grouped` accumulation cases (B5)

**Files:**
- Test: `musefs-db/tests/tags.rs`

> Reframed: `tags_grouped` groups **Rust-side via a `HashMap`**, not SQL `GROUP BY`. Multi-track isolation is already covered by `tags_grouped_returns_all_tags_by_track`. The gaps are the empty-db and single-track multi-value ordering cases.

- [ ] **Step 1: Write the failing tests**

Append to `musefs-db/tests/tags.rs`:

```rust
#[test]
fn tags_grouped_empty_db_is_empty_map() {
    let db = Db::open_in_memory().unwrap();
    assert!(db.tags_grouped().unwrap().is_empty());
}

#[test]
fn tags_grouped_preserves_key_ordinal_order_for_multivalue() {
    let db = Db::open_in_memory().unwrap();
    let t = db.upsert_track(&new_track("/m/a.flac")).unwrap();
    db.replace_tags(
        t,
        &[
            Tag::new("artist", "Second", 1),
            Tag::new("artist", "First", 0),
            Tag::new("genre", "Rock", 0),
        ],
    )
    .unwrap();

    let grouped = db.tags_grouped().unwrap();
    assert_eq!(
        grouped.get(&t),
        Some(&vec![
            Tag::new("artist", "First", 0),
            Tag::new("artist", "Second", 1),
            Tag::new("genre", "Rock", 0),
        ]),
        "multi-value group must be ordered by (key, ordinal), matching get_tags"
    );
}
```

- [ ] **Step 2: Run the tests to verify they pass**

Run: `cargo test -p musefs-db --test tags tags_grouped_empty_db_is_empty_map tags_grouped_preserves_key_ordinal_order_for_multivalue`
Expected: both **PASS**.

- [ ] **Step 3: Hand-apply spot-check**

In `musefs-db/src/tags.rs::tags_grouped`, remove `, key, ordinal` from the `ORDER BY track_id, key, ordinal` clause → `tags_grouped_preserves_key_ordinal_order_for_multivalue` must FAIL (insertion order `Second, First` is preserved instead of `First, Second`). Revert with `git checkout -- musefs-db/src/tags.rs`, rerun → PASS.

- [ ] **Step 4: Commit**

```bash
git add musefs-db/tests/tags.rs
git commit -m "test(db): pin tags_grouped empty + multi-value ordering (#12)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 7: Confirm `schema.rs` `< → <=` is an equivalent mutant (B2)

**Files:** none (verification only — recorded in Task 8).

- [ ] **Step 1: Reason + hand-apply**

In `musefs-db/src/schema.rs::migrate`, the loop body `if current < target` is reached only when the fast-path `>= latest` early-return did not fire — i.e. `current < latest`. With a single migration (`MIGRATIONS.len() == 1`), the only reaching state is `(current = 0, target = 1)`, where `0 < 1` and `0 <= 1` are both true. So `< → <=` cannot change behavior.

Apply `< → <=` to that line and run the schema tests:
Run: `cargo test -p musefs-db --test schema`
Expected: **PASS** (mutation is behavior-preserving — confirms equivalence). Revert with `git checkout -- musefs-db/src/schema.rs`.

- [ ] **Step 2: No commit** (recorded as an inventory annotation in Task 8).

---

### Task 8: PR 1 documentation — framing corrections + known-mutant annotations (B7, partial)

**Files:**
- Modify: `docs/superpowers/specs/test-audit-remediation/2026-05-29-mutation-inventory.md`
- Modify: `docs/superpowers/specs/test-audit-remediation/2026-05-29-remediation-tracking.md`

- [ ] **Step 1: Annotate the inventory's known db survivors**

In the inventory's `musefs-db` "Surviving mutants → phase" table, update the two rows:

- `lib.rs:55` → `missed → **killed** (phase 4b)`
- `schema.rs:93` → `missed → **equivalent**` with rationale: "loop body only reached at (current=0, target=1) due to the fast-path `>= latest` early-return + single migration, where `<`/`<=` agree."

- [ ] **Step 2: Correct the unviable note**

In the inventory's "Tool limitations to revisit" bullet for `musefs-db`, replace the "Db has no Default" oversimplification with the verified analysis: only `lib.rs`'s `Db`-returning fns need `Db: Default`; the `tags`/`tracks`/`art` unviables need `Default` on the model structs (`Track`/`Art`/`ArtMeta`/`Tag`/`TrackArt`) and `Format`. Note these are now unblocked behind the `mutants` feature (PR 1), with the survivor sweep in PR 2.

- [ ] **Step 3: Record the #11 / #12 framing corrections**

Add a short note (in the inventory near the db section, or the tracking doc's Phase 4 entry) that finding #11's "concurrent-deletion race" does not exist (single `DELETE … WHERE id NOT IN`) and #12's "GROUP BY assembly" is actually Rust-side `HashMap` grouping — tests target the real gaps accordingly.

- [ ] **Step 4: Update the tracking doc Phase 4b status**

In `2026-05-29-remediation-tracking.md`, update the Phase 4 section: mark 4b PR 1 landed (feature + known kills + #10/#11/#12 coverage + framing corrections), with the B6 newly-viable survivor sweep tracked as the PR-2 follow-up gated on a dispatched campaign. Update the header `Status:` line accordingly.

- [ ] **Step 5: Run the full local gate**

Run:
```bash
cargo fmt --all -- --check \
  && cargo clippy --all-targets -- -D warnings \
  && cargo test --workspace \
  && cargo test -p musefs-format --features fuzzing \
  && cargo test -p musefs-db --features mutants
```
Expected: all green.

- [ ] **Step 6: Commit**

```bash
git add docs/superpowers/specs/test-audit-remediation/2026-05-29-mutation-inventory.md \
        docs/superpowers/specs/test-audit-remediation/2026-05-29-remediation-tracking.md
git commit -m "docs: Phase 4b PR1 — inventory annotations + #11/#12 framing corrections

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

- [ ] **Step 7: Open PR 1 and dispatch the campaign**

Push the branch and open the PR (`gh pr create`). Then dispatch the full mutation campaign on the branch so its artifact is ready for PR 2:

```bash
gh workflow run mutants.yml --ref phase-4b-db-hardening
```

The `full` job's `mutants-musefs-db` artifact (built with `--features mutants`) is PR 2's input. Note in the PR description that PR 2 follows once the campaign completes.

---

## PR 2 — newly-viable survivor sweep (post-campaign)

### Task 9: Kill the newly-viable db survivors and finalize the inventory (B6)

> **Gated on the dispatched campaign.** This task cannot be fully specified ahead of time — the exact survivor set is unknown until the `--features mutants` campaign runs. It defines the procedure; the implementer enumerates and kills the actual survivors. Do this on a fresh branch off `main` after PR 1 merges.

**Files:**
- Test: `musefs-db/tests/{tracks,art,tags,schema}.rs` (new `#[cfg(feature = "mutants")]` kill tests as needed)
- Modify: the inventory + tracking docs (final annotations)

- [ ] **Step 1: Fetch the campaign artifact**

From the dispatched `mutants.yml` run, download the `mutants-musefs-db` artifact:

```bash
gh run download <run-id> -n mutants-musefs-db -D /tmp/mutants-db
```

The per-result lists are under `mutants.out/` (`missed.txt`, `timeout.txt`, `caught.txt`, `unviable.txt`).

- [ ] **Step 2: Enumerate the newly-viable survivors**

The 20 previously-unviable mutants (tags 8 / tracks 5 / lib 4 / art 3) are now viable. Diff `missed.txt` + `timeout.txt` against the kills already landed in PR 1 (Tasks 2/4/5/6). The remainder is the sweep set. Record each as a `function: construct: mutation` line.

- [ ] **Step 3: Kill each survivor (TDD + hand-apply, one commit per cluster)**

For each survivor, add a test (a `#[cfg(feature = "mutants")]` test where the kill needs a `Default`-constructed value; otherwise a plain `open_in_memory()` test) beside the construct in the matching `tests/*.rs` file. Verify with the hand-apply method (locate by construct → apply mutation → `cargo test -p musefs-db --features mutants <test>` must FAIL → revert → PASS). If a mutant proves equivalent, record it with a one-line rationale instead.

- [ ] **Step 4: Finalize the inventory + tracking doc**

Annotate every swept db row `missed → **killed** (phase 4b)` / `**equivalent**`. Mark Phase 4 (4a + 4b) **complete** in `2026-05-29-remediation-tracking.md` and update its header `Status:` line.

- [ ] **Step 5: Run the full local gate**

Run:
```bash
cargo fmt --all -- --check \
  && cargo clippy --all-targets -- -D warnings \
  && cargo test --workspace \
  && cargo test -p musefs-db --features mutants
```
Expected: all green.

- [ ] **Step 6: Commit and open PR 2**

```bash
git add musefs-db/tests docs/superpowers/specs/test-audit-remediation
git commit -m "test(db): Phase 4b PR2 — kill newly-viable survivors; Phase 4 complete

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

- [ ] **Step 7 (acceptance): re-dispatch the campaign to confirm zero db survivors**

```bash
gh workflow run mutants.yml --ref <pr2-branch>
```
Expected: the db leg shows the 2 known + the 20 unblocked all killed or documented-equivalent.

---

## Self-Review notes (for the implementer)

- **Spec coverage:** B1 → Tasks 1–3; B2 → Tasks 2 & 7; B3 → Task 4; B4 → Task 5; B5 → Task 6; B7 → Tasks 8 (PR 1) & 9 (PR 2); B6 → Task 9.
- **Type consistency:** `TrackArt` fields used in tests (`art_id`, `picture_type`, `description`, `ordinal`) and `NewTrack` fields (`backing_path`, `format`, `audio_offset`, `audio_length`, `backing_size`, `backing_mtime`) match `musefs-db/src/models.rs`. Helpers `new_track(path)` and `jpeg(data)` come from `tests/common/mod.rs`.
- **Feature-off invariance:** every `Default`-dependent test is `#[cfg(feature = "mutants")]`; `cargo test --workspace` (feature off) is unaffected, so normal CI stays green while the gated leg and the campaign exercise the kills.
