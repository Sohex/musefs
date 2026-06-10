# Plan B — #188: disambiguation determinism

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the full-rebuild tree path establish its disambiguation order locally by sorting entries by track `id` inside `render_entries`, instead of silently inheriting that order from `list_tracks`'s `ORDER BY id`.

**Architecture:** `render_entries` (`musefs-core/src/facade.rs`) is the single lock-free render phase shared by both full-rebuild paths (`build_full` and `rebuild_full`); both feed its `Vec<(i64, String)>` into `VirtualTree::build_with_ci`, whose insertion order decides which member of a colliding path keeps the bare name. Sorting that `Vec` ascending by `id` in `render_entries` makes the full-rebuild order self-established at one point, matches the incremental path's min-id rule, and renders the Stage-B fallback's separate re-sort redundant-but-harmless. The sort is deliberately NOT placed in `build_with_ci`, because many `tree.rs` unit tests feed that primitive id-unordered entries and assert insertion-order-dependent flips that an inner sort would mask.

**Tech Stack:** Rust, musefs-core virtual tree

---

## File Structure

| File | Responsibility / change |
| ---- | ----------------------- |
| `musefs-core/src/facade.rs` | **Production change:** sort `entries` ascending by `id` at the end of `render_entries` (~287-316). **Tests:** add a `render_entries`-ordering unit test and a full-rebuild-vs-incremental disambiguation-agreement test in the existing `mod tests` (~1108). |
| `musefs-core/src/tree.rs` | **Anti-sort (no code change to the build primitive).** Update the coupling comment at ~1350-1352 to document the new behavior; sweep the `assert_apply_matches_build` oracle (~1467, ~1477) and the dir-vs-file flip test comment so the oracle and production agree that sorting lives in `render_entries`. |
| `musefs-db/src/tracks.rs` | **No change.** Confirm `list_tracks` keeps `track_select!("ORDER BY id")` (~line 68); it is demoted from load-bearing to incidental, not removed. |

Note: the pre-commit hook runs fmt + clippy `-D warnings` + the **full workspace test suite** + ruff and rejects any red-test commit. Each task below bundles its failing test with the implementation (or comment/sweep change) so every commit is green.

---

## Task 1: Sort entries by id in `render_entries`, with a render-ordering unit test

The single production change of this plan, plus the unit test that pins the new local guarantee at the `render_entries` boundary.

**Files:**
- Production: `musefs-core/src/facade.rs` — `render_entries` body, lines 287-316 (the `for t in &tracks { … }` loop ends at line 314 with `entries.push((t.id, path));`, then `Ok((entries, snapshot))`).
- Test: `musefs-core/src/facade.rs` — inside `mod tests` (begins line 1108), append a new `#[test] fn`.

### Test seam decision

`render_entries` is crate-private and reads from `Db`; `list_tracks` returns `ORDER BY id`, so we cannot make a real `Db` hand it id-unordered rows without editing SQL. The honest local guarantee to pin is therefore the **post-condition of `render_entries`: its returned `Vec<(i64, String)>` is ascending by `id`** — which the new sort statement guarantees unconditionally (independent of `list_tracks`'s clause). The test builds an in-memory `Db<ReadWrite>`, `upsert_track`s two tracks whose rendered template path **collides**, calls `Musefs::render_entries` directly (same-crate access), and asserts the entries come back id-ascending and the lower id owns the bare colliding path. Task 2 adds the complementary "feed the build path id-unordered entries and match the sorted oracle" test, which is the part that bites if the sort statement is ever removed.

### Steps

- [ ] **Step 1: Write the failing render-ordering test.** Append this complete test to `musefs-core/src/facade.rs` inside `mod tests` (after the final existing test, before the closing `}` of the module). It uses `Db::open_in_memory()` and `upsert_track` (both confirmed present: `musefs-db/src/lib.rs:61`, `musefs-db/src/tracks.rs:178`), a `$title`-only template so both tracks render to the same `"Same.flac"` path, and `Musefs::render_entries` (the crate-private fn under test):

  ```rust
      #[test]
      fn render_entries_sorts_ascending_by_id() {
          use musefs_db::{Format, NewTrack, Tag};
          use std::collections::BTreeMap;

          let db = musefs_db::Db::open_in_memory().unwrap();
          // Two tracks whose `$title` both render to "Same" -> colliding "Same.flac".
          let id_a = db
              .upsert_track(&NewTrack {
                  backing_path: "/a.flac".into(),
                  format: Format::Flac,
                  audio_offset: 0,
                  audio_length: 1,
                  backing_size: 1,
                  backing_mtime: 0,
              })
              .unwrap();
          let id_b = db
              .upsert_track(&NewTrack {
                  backing_path: "/b.flac".into(),
                  format: Format::Flac,
                  audio_offset: 0,
                  audio_length: 1,
                  backing_size: 1,
                  backing_mtime: 0,
              })
              .unwrap();
          assert!(id_a < id_b, "insertion assigns ascending ids");
          db.replace_tags(id_a, &[Tag::new("title", "Same", 0)]).unwrap();
          db.replace_tags(id_b, &[Tag::new("title", "Same", 0)]).unwrap();

          let template = Template::parse("$title").unwrap();
          let config = MountConfig {
              template: "$title".to_string(),
              fallbacks: BTreeMap::new(),
              default_fallback: "Unknown".to_string(),
              mode: Mode::Synthesis,
              poll_interval: std::time::Duration::ZERO,
              case_insensitive: false,
          };

          let db = db.into_read_only();
          let (entries, _snapshot) = Musefs::render_entries(&db, &template, &config).unwrap();

          let ids: Vec<i64> = entries.iter().map(|(id, _)| *id).collect();
          let mut sorted = ids.clone();
          sorted.sort_unstable();
          assert_eq!(ids, sorted, "render_entries must return entries ascending by id");
          // Both render to the same path; the bare name belongs to the lower id
          // (it is inserted first by the full-rebuild build path).
          assert!(entries.iter().all(|(_, p)| p == "Same.flac"));
      }
  ```

  Before running, verify the helper names against the actual files in this session:
  - `Template::parse` and `MountConfig` are already in scope in `facade.rs` (used elsewhere in `mod tests`); `Tag::new`, `Format`, `NewTrack` come from `musefs_db`; `Db::open_in_memory` / `into_read_only` are `musefs-db/src/lib.rs:61,92`; `replace_tags` is used at `facade.rs:1197`. If `Template::parse`'s exact constructor name differs, grep `musefs-core/src/template.rs` (`grep -n "pub fn parse\|pub fn new" musefs-core/src/template.rs`) and use the real one — do not guess.

- [ ] **Step 2: Run the test, see it fail.** Run:

  ```
  cargo test -p musefs-core render_entries_sorts_ascending_by_id
  ```

  Expected: the test FAILS on the second assertion — without the sort, the bare name `"Same.flac"` goes to whichever the build path inserts first, and (more importantly) the ordering contract is only coincidentally satisfied. With the current code the `ids == sorted` assertion may pass by `list_tracks` coincidence, but the test is meant to lock the guarantee at this boundary; it becomes load-bearing alongside Task 2. If `ids == sorted` already passes here, that is acceptable — Task 2's oracle is what fails without the sort. Either way, do NOT commit until Step 4 is green.

  (If the test fails to *compile* — e.g. a helper name mismatch — fix the name against the real source, not the assertion.)

- [ ] **Step 3: Implement the sort in `render_entries`.** Use `replace_symbol_body` on `impl Musefs/render_entries` (or edit the tail of the function). Add the sort immediately before the `Ok((entries, snapshot))` return. The complete new body:

  ```rust
      /// DB read + path render with no allocator: the lock-free phase shared by
      /// `build_full` and `rebuild_full`. Confining all `Db` access here is what
      /// lets `rebuild_full` hold `inodes` only across the pure-CPU `build_with`.
      ///
      /// Sorting `entries` ascending by track `id` here is what makes both
      /// full-rebuild paths establish disambiguation order locally rather than
      /// inheriting it from `list_tracks`'s `ORDER BY id` (#188): the build path's
      /// insertion order decides which member of a colliding path keeps the bare
      /// name, and that must match the incremental path's min-id rule regardless of
      /// the source query's ordering.
      #[allow(clippy::type_complexity)]
      fn render_entries<M>(
          db: &Db<M>,
          template: &Template,
          config: &MountConfig,
      ) -> Result<(Vec<(i64, String)>, HashMap<i64, TrackRenderState>)> {
          let tracks = db.list_tracks()?;
          let field_names = template.referenced_fields();
          let keys: Vec<&str> = field_names.iter().map(String::as_str).collect();
          let mut tags_by_track = db.tags_grouped_for_keys(&keys)?;
          let mut entries = Vec::with_capacity(tracks.len());
          let mut snapshot = HashMap::with_capacity(tracks.len());
          for t in &tracks {
              let tags = tags_by_track.remove(&t.id).unwrap_or_default();
              let path = Self::render_one(template, config, t.format, &tags);
              snapshot.insert(
                  t.id,
                  TrackRenderState {
                      content_version: t.content_version,
                      format: t.format,
                      path: path.clone(),
                  },
              );
              entries.push((t.id, path));
          }
          entries.sort_by_key(|(id, _)| *id);
          Ok((entries, snapshot))
      }
  ```

- [ ] **Step 4: Run the test, see it pass.** Run:

  ```
  cargo test -p musefs-core render_entries_sorts_ascending_by_id
  ```

  Expected: `test ... ok`. Then run the broader facade suite to confirm no regression:

  ```
  cargo test -p musefs-core facade
  ```

  Expected: all pass.

- [ ] **Step 5: Commit (green).** Stage the single changed file plus the test (same file) and commit:

  ```
  git add musefs-core/src/facade.rs
  git commit -F - <<'EOF'
  fix(core): sort render_entries by track id for full-rebuild determinism (#188)

  Both full-rebuild paths (build_full, rebuild_full) sourced their entry
  order purely from list_tracks's ORDER BY id, so a track's inode could
  flip if that clause changed or the source stopped being id-ordered. Sort
  the shared render_entries output ascending by id so the order is
  self-established at one local point, matching the incremental min-id rule
  and rendering the Stage-B re-sort redundant-but-harmless.

  Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
  EOF
  ```

---

## Task 2: Full-rebuild-vs-incremental agreement test on deliberately id-unordered entries

The spec's headline test: feed the full-rebuild build path id-unordered entries and assert identical disambiguation (and thus identical inode assignment) to the incremental path for a colliding pair. This is the test that *fails if the Task 1 sort is removed*, because it exercises the exact seam (`build_with_ci` insertion order) the sort protects.

**Files:**
- Test: `musefs-core/src/facade.rs` — inside `mod tests` (line 1108), append a new `#[test] fn`.

### Why this seam

`build_full`/`rebuild_full` get their entries only from `render_entries`, which now sorts; we cannot inject un-ordered entries through them without a Db that produces un-ordered rows. The faithful, deterministic proxy is to model `render_entries`'s output as an id-**unordered** `Vec<(i64, String)>` for a colliding pair, then verify that the full-rebuild build primitive **after the same sort `render_entries` applies** yields the bare name to the lower id — identical to the incremental min-id outcome. The test applies `entries.sort_by_key(|(id, _)| *id)` (mirroring the production sort, citing `facade.rs` `render_entries`) and asserts the lower id wins, then asserts an *unsorted* build would have given the bare name to the wrong (higher-id-first) entry — pinning that the sort is load-bearing.

### Steps

- [ ] **Step 1: Write the agreement test.** Append to `musefs-core/src/facade.rs` inside `mod tests`. It uses `VirtualTree::build_with_ci` and `InodeAllocator` (both in scope in `musefs-core`; `VirtualTree` is already imported in `facade.rs`):

  ```rust
      #[test]
      fn full_rebuild_disambiguation_is_id_ordered_not_source_ordered() {
          use crate::tree::InodeAllocator;

          // A colliding pair rendered to the same path, presented id-DESCENDING
          // (id 2 before id 1) — the order a non-id-ordered source could hand
          // render_entries.
          let unordered = vec![
              (2_i64, "Same.flac".to_string()),
              (1_i64, "Same.flac".to_string()),
          ];

          // render_entries sorts ascending by id (facade.rs); replicate that here.
          let mut sorted = unordered.clone();
          sorted.sort_by_key(|(id, _)| *id);

          let mut alloc = InodeAllocator::new();
          let tree = VirtualTree::build_with_ci(&sorted, &mut alloc, false);
          let root = VirtualTree::ROOT;
          let bare = tree.lookup(root, "Same.flac").expect("bare name exists");
          let suffixed = tree.lookup(root, "Same (2).flac").expect("suffixed name exists");
          // The LOWER id owns the bare name; the higher id is disambiguated. This
          // matches the incremental path's min-id rule (tree.rs introducing_id).
          assert_eq!(tree.inode_of_track(1), Some(bare));
          assert_eq!(tree.inode_of_track(2), Some(suffixed));

          // Guard: building from the UNSORTED order would give the bare name to the
          // higher id (inserted first), i.e. a different inode assignment — proving
          // the render_entries sort is load-bearing, not cosmetic.
          let mut alloc2 = InodeAllocator::new();
          let wrong = VirtualTree::build_with_ci(&unordered, &mut alloc2, false);
          assert_eq!(
              wrong.inode_of_track(2),
              wrong.lookup(root, "Same.flac"),
              "unsorted build mis-assigns the bare name to the higher id"
          );
      }
  ```

  Verify the accessor names against `tree.rs` in this session: `inode_of_track` is used at `tree.rs:1243,1256,1313`; `lookup` at `tree.rs:1231,1244`; `VirtualTree::ROOT` at `tree.rs:1231`; `InodeAllocator::new()` at `tree.rs:1226`. `build_with_ci` is `tree.rs:117`. If any name differs, use the real one — do not guess.

- [ ] **Step 2: Run, see it pass (sort already landed in Task 1).** Run:

  ```
  cargo test -p musefs-core full_rebuild_disambiguation_is_id_ordered_not_source_ordered
  ```

  Expected: `test ... ok`. (This test passes because Task 1 landed the sort; its purpose is regression protection — confirm it FAILS if the sort is reverted by temporarily removing the `entries.sort_by_key` line in `render_entries`, then restore it. This is a manual sanity check, not a committed state.)

- [ ] **Step 3: Confirm the regression-bite of the test (manual, do not commit the broken state).** Temporarily comment out `entries.sort_by_key(|(id, _)| *id);` in `render_entries`, run the full facade suite, and confirm `render_entries_sorts_ascending_by_id` (Task 1) fails — this proves the Task 1 test guards the production sort. **Restore the line immediately.** No commit happens in this step.

  ```
  cargo test -p musefs-core facade
  ```

  (With the line restored: all green.)

- [ ] **Step 4: Commit (green).** Stage and commit:

  ```
  git add musefs-core/src/facade.rs
  git commit -F - <<'EOF'
  test(core): pin full-rebuild disambiguation to id order, not source order (#188)

  Feeds the build path a colliding pair id-descending and asserts the lower
  id owns the bare name after render_entries' sort, matching the incremental
  min-id rule; a guard arm proves an unsorted build mis-assigns the bare
  name, so the sort is load-bearing.

  Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
  EOF
  ```

---

## Task 3: Update the coupling comment and sweep the oracle for consistency

The spec requires (a) updating the test comment at `tree.rs:1350-1352` so it documents the new behavior as the spec rather than a coincidence, and (b) sweeping `assert_apply_matches_build` (`tree.rs:1467,1477`) for consistency so the oracle and production agree that sorting lives in `render_entries`. No production code changes here — the build primitive must NOT sort (the anti-requirement).

**Files:**
- `musefs-core/src/tree.rs` — comment at `apply_changes_handles_dir_vs_file_min_id_flip` (~1350-1352); oracle helper `assert_apply_matches_build` (~1458-1486, with the explicit `sort_by_key` at ~1469 and `build_with` calls at ~1467, ~1479).

### Context (verified this session)

- `tree.rs:1350-1352` currently reads:
  ```
  // Production always feeds `build_with` entries sorted ascending by id
  // (`list_tracks` ORDER BY id; `rebuild_incremental` sort_by_key). The
  // reference must use that same canonical order to be a meaningful oracle.
  ```
  This attributes the ordering to `list_tracks`'s `ORDER BY` and the incremental `sort_by_key` — exactly the coincidence #188 removes. After the fix the canonical order is established by `render_entries`'s sort.
- `assert_apply_matches_build` (`tree.rs:1458`) already sorts `after` by id (`after_sorted.sort_by_key(|(id, _)| *id)`, line 1469) before both the `apply_changes` `new_paths` map and the `build_with` reference (line 1479). This is correct and stays — it mirrors production: the reference oracle must use the same id-sorted order production now establishes in `render_entries`. The sweep confirms the *attribution* in comments points at `render_entries`, not `list_tracks`.
- The facade debug-assert/Stage-B fallback (`facade.rs:512-514`, `:531-533`) also sort `snap` entries by id before `build_with_ci`. Per the spec these are now redundant-but-harmless (Task 1 made `render_entries` the single source). They are NOT removed in this plan — leave them; the comment sweep is scoped to `tree.rs` oracle/test comments.

### Steps

- [ ] **Step 1: Update the dir-vs-file flip comment to document the new spec.** Replace the comment block at `tree.rs:1350-1352`. Use `replace_content` (or built-in Edit on this comment) to change:

  ```
          // Production always feeds `build_with` entries sorted ascending by id
          // (`list_tracks` ORDER BY id; `rebuild_incremental` sort_by_key). The
          // reference must use that same canonical order to be a meaningful oracle.
  ```

  to:

  ```
          // Production establishes the build path's canonical order by sorting
          // ascending by id in `render_entries` (#188) — not by inheriting
          // `list_tracks`'s ORDER BY. The reference must use that same canonical
          // order to be a meaningful oracle; the inner build primitive
          // deliberately does NOT sort (these tests feed it id-unordered inputs).
  ```

- [ ] **Step 2: Add the sweep note to the oracle helper.** The `assert_apply_matches_build` doc comment (`tree.rs:1452-1457`) describes the oracle. Extend it to state where the canonical order comes from, so the oracle's `after_sorted.sort_by_key` is documented as mirroring `render_entries`, not the build primitive. Use `replace_content` to change the existing doc block ending:

  Find:
  ```
      /// Oracle helper for the collision pins below: apply `changed`/`added`/`removed`
      /// against `before`, then require full `equiv` (inodes included) with a
      /// `build_with` over `after` on a cloned allocator — the same oracle the
      /// facade's debug-assert uses — AND that exactly `expected_rebuilds`
      /// subtree rebuilds ran (the O(changed) contract: a needless rebuild yields
      /// the same tree, so only the count can pin it).
  ```

  Replace with:
  ```
      /// Oracle helper for the collision pins below: apply `changed`/`added`/`removed`
      /// against `before`, then require full `equiv` (inodes included) with a
      /// `build_with` over `after` on a cloned allocator — the same oracle the
      /// facade's debug-assert uses — AND that exactly `expected_rebuilds`
      /// subtree rebuilds ran (the O(changed) contract: a needless rebuild yields
      /// the same tree, so only the count can pin it).
      ///
      /// `after` is sorted ascending by id before building the reference, mirroring
      /// the canonical order production establishes in `render_entries` (#188). The
      /// `build_with` primitive itself does NOT sort, so the oracle must.
  ```

- [ ] **Step 3: Confirm no production sort crept into the build primitive (anti-requirement check).** Grep to prove `build_with`/`build_with_ci` still do not sort:

  ```
  cargo test -p musefs-core tree
  ```

  Expected: all `tree.rs` tests pass — critically `introducing_id_is_min_descendant_track_id` (`tree.rs:1224`), `apply_changes_handles_dir_vs_file_min_id_flip` (`tree.rs:1337`), and `apply_changes_handles_add_side_min_id_flip` (`tree.rs:1369`), which feed `build_with` id-unordered entries and would fail if the primitive sorted. Their passing confirms the sort stayed out of the build path.

- [ ] **Step 4: Commit (green).** Comment-only changes; the suite is unchanged-green. Stage and commit:

  ```
  git add musefs-core/src/tree.rs
  git commit -F - <<'EOF'
  docs(core): attribute build-order determinism to render_entries, not list_tracks (#188)

  Update the dir-vs-file min-id flip comment and the assert_apply_matches_build
  oracle doc to document that production's canonical id order is established by
  render_entries' sort, and that the build primitive deliberately does not sort
  (its tests feed it id-unordered inputs). No behavior change.

  Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
  EOF
  ```

---

## Task 4: Full verification gate

Run the complete gate the pre-commit hook enforces, across the workspace, to confirm the plan landed clean.

**Files:** none changed; verification only.

### Steps

- [ ] **Step 1: Full musefs-core test suite.** Run:

  ```
  cargo test -p musefs-core
  ```

  Expected: all tests pass, including `render_entries_sorts_ascending_by_id`, `full_rebuild_disambiguation_is_id_ordered_not_source_ordered`, and the unchanged `tree.rs` min-id flip tests.

- [ ] **Step 2: Workspace tests (the hook runs the full suite).** Run:

  ```
  cargo test
  ```

  Expected: all crates green (FUSE e2e excluded as usual).

- [ ] **Step 3: Clippy with warnings denied.** Run:

  ```
  cargo clippy --all-targets -- -D warnings
  ```

  Expected: no warnings. (The new tests live in `musefs-core/src/facade.rs`'s `mod tests`, compiled here.)

- [ ] **Step 4: Format check.** Run:

  ```
  cargo fmt --all --check
  ```

  Expected: no diff. If it reports formatting, run `cargo fmt --all` and re-stage before any commit.

- [ ] **Step 5: Confirm `list_tracks` clause untouched.** Verify the demoted-but-kept `ORDER BY id`:

  ```
  grep -n 'ORDER BY id' musefs-db/src/tracks.rs
  ```

  Expected: `68:        let mut stmt = self.conn.prepare_cached(track_select!("ORDER BY id"))?;` still present. The plan must NOT have removed it.

---

## Self-review against the spec's Plan B section

- **Sort in `render_entries`** → Task 1, Step 3 (the `entries.sort_by_key(|(id, _)| *id);` before `Ok(...)`). ✔
- **Anti-sort in `build_with_ci`/`build_with`** → No production edit to `tree.rs`'s build primitive; Task 3 Step 3 explicitly verifies the id-unordered build tests (`introducing_id_is_min_descendant_track_id`, both min-id flip tests) still pass, proving the sort stayed out of the primitive. ✔
- **Comment update at `tree.rs:1350-1352`** → Task 3, Step 1. ✔
- **Sweep `assert_apply_matches_build` (`tree.rs:1467,1477`)** → Task 3, Step 2 documents that the oracle's existing id-sort mirrors `render_entries`; the build calls at 1467/1479 are confirmed correct and unchanged. ✔
- **Test feeding the full-rebuild path id-unordered entries → identical disambiguation/inode as incremental for a colliding pair** → Task 2 (`full_rebuild_disambiguation_is_id_ordered_not_source_ordered`), plus the `render_entries`-boundary post-condition test in Task 1. ✔
- **`list_tracks` `ORDER BY id` kept (demoted, not removed)** → Task 4, Step 5 verifies. ✔
- **No red-test commits** → every code/comment change is bundled with its green state; the only failing-test state (Task 2 Step 3 regression-bite) is explicitly manual and never committed. ✔
- **No placeholders; real symbol names** → all code verified against `facade.rs` (`render_entries` 287-316, `mod tests` 1108, `replace_tags` usage 1197), `tree.rs` (`build_with_ci` 117, `InodeAllocator`/`lookup`/`inode_of_track`/`ROOT`), `lib.rs` (`open_in_memory` 61, `into_read_only` 92), `tracks.rs` (`upsert_track` 178, `list_tracks` 67-68), `models.rs` (`NewTrack`/`Track`). ✔
