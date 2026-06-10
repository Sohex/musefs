# Plan B — #188: disambiguation determinism

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the full-rebuild tree path establish its disambiguation order locally by sorting entries by track `id` inside `render_entries`, instead of silently inheriting that order from `list_tracks`'s `ORDER BY id`.

**Architecture:** `render_entries` (`musefs-core/src/facade.rs`) is the single lock-free render phase shared by both full-rebuild paths (`build_full` and `rebuild_full`); both feed its `Vec<(i64, String)>` into `VirtualTree::build_with_ci`, whose insertion order decides which member of a colliding path keeps the bare name (`disambiguate` gives the bare name to the first inserted, ` (k)` to later ones — `tree.rs:277`). Sorting that `Vec` ascending by `id` in `render_entries` makes the full-rebuild order self-established at one point and matches the incremental path's min-id rule. The sort is deliberately NOT placed in `build_with_ci`, because many `tree.rs` unit tests feed that primitive id-unordered entries and assert insertion-order-dependent flips that an inner sort would mask.

**Tech Stack:** Rust, musefs-core virtual tree

---

## The mutation-gate problem this plan must solve (read before Task 1)

The per-PR mutation gate (`.github/workflows/mutants.yml`, `in-diff` job) mutates every Rust line this PR changes in `musefs-core/**`. So the production sort statement we add WILL be mutated, and a committed test must KILL that mutant or the gate fails.

Here is the trap: `list_tracks` is hard-coded `ORDER BY id` (`musefs-db/src/tracks.rs:66-70`), SQLite assigns `id` ascending by insertion, and `render_entries` is fed ONLY by `list_tracks` (its two callers `build_full` and `rebuild_full` pass nothing else; verified via references). **So a real `Db` can never hand `render_entries` id-unordered rows.** A sort statement placed bare inside `render_entries` is therefore a *no-op on every reachable input* — removing it changes no DB-backed output, the mutant survives, and the gate (or a manual reviewer) is right to flag it. Investigation confirmed there is **no** production producer of id-unordered entries into `render_entries`; `list_tracks`'s clause is the sole upstream order, so option (a) "find a real unordered scenario" is impossible.

**Resolution chosen: option (b) — extract the sort into a small pure helper and unit-test that helper directly.** `render_entries` calls `Self::order_entries(entries)` as part of its `Ok((..))` return. The helper

```rust
fn order_entries(mut entries: Vec<(i64, String)>) -> Vec<(i64, String)> {
    entries.sort_by_key(|(id, _)| *id);
    entries
}
```

is mutated by cargo-mutants into bodies that return the *unsorted* `entries` (statement deleted) or an empty/`Default` `Vec`. A direct unit test that feeds a **descending** id pair and asserts **ascending** output kills both classes — the sort logic is now load-bearing under a committed test, independent of `list_tracks`. Because the call lives inside the function's return expression (not as a standalone deletable statement), `render_entries` itself gains no surviving call-site mutant: its only function-level mutant ("replace body with `Ok(Default::default())`") is already killed by the existing `render_entries_returns_paths_and_snapshot` test (`facade.rs:1341`).

Honest framing of the two higher-level tests: a DB-backed test through `build_full`/`render_entries` documents the end-to-end guarantee (lower id owns the bare colliding name) but does **not** by itself bite if the sort is removed, precisely because the DB input is already id-ordered. The `order_entries` unit test is the regression guard. The plan says so explicitly rather than pretending a DB-backed test fails.

---

## File Structure

| File | Responsibility / change |
| ---- | ----------------------- |
| `musefs-core/src/facade.rs` | **Production change:** add a pure `order_entries` helper next to `render_entries` and call it in `render_entries`'s return (`render_entries` body is `facade.rs:287-316`). **Tests:** add the `order_entries` regression unit test (the mutation-gate killer) plus a DB-backed full-rebuild disambiguation test, in the existing `mod tests` (begins `facade.rs:1108`). |
| `musefs-core/src/tree.rs` | **Anti-sort (no code change to the build primitive).** Update the coupling comment in `apply_changes_handles_dir_vs_file_min_id_flip` (`tree.rs:1353-1355`) and the `assert_apply_matches_build` doc comment (`tree.rs:1451-1456`) so the oracle and production agree that sorting lives in `render_entries`, not `list_tracks`. |
| `musefs-db/src/tracks.rs` | **No change.** Confirm `list_tracks` keeps `track_select!("ORDER BY id")` (`tracks.rs:67`); it is demoted from load-bearing to incidental, not removed. |

Note: the pre-commit hook runs fmt + clippy `-D warnings` + the **full workspace test suite** + ruff and rejects any red-test commit. Each task below bundles its failing test with the implementation (or comment/sweep change) so every commit is green.

---

## Task 1: Extract `order_entries`, sort `render_entries`, with the mutation-gate-killing unit test

The single production change of this plan: a pure `order_entries(Vec) -> Vec` helper that `render_entries` calls in its return, plus the direct unit test on that helper that fails (and kills the mutant) if the sort is removed.

**Files:**
- Production: `musefs-core/src/facade.rs` — `render_entries` body (`facade.rs:287-316`); add `order_entries` immediately after it.
- Test: `musefs-core/src/facade.rs` — inside `mod tests` (begins `facade.rs:1108`), append two new `#[test] fn`s.

### Test seam decision

`render_entries` is crate-private and fed only by `list_tracks` (`ORDER BY id`), so no DB-backed input is ever id-unordered — a bare sort inside `render_entries` is unobservable through the public path and would survive the mutation gate. We therefore extract the sort into the pure helper `order_entries` and pin it with a direct unit test that constructs a **descending** id input the DB could never produce. That test is the committed regression guard: it fails if the sort body is removed or mutated. The DB-backed test (Task 2 / Step 3 below) documents the end-to-end disambiguation guarantee but is not the bite.

### Steps

- [ ] **Step 1: Extract the helper and call it from `render_entries`.** Use `replace_symbol_body` on `impl Musefs/render_entries` to change only the return line, then `insert_after_symbol` (after `render_entries`) to add `order_entries`. The complete new `render_entries` body:

  ```rust
      /// DB read + path render with no allocator: the lock-free phase shared by
      /// `build_full` and `rebuild_full`. Confining all `Db` access here is what
      /// lets `rebuild_full` hold `inodes` only across the pure-CPU `build_with`.
      ///
      /// The returned entries are ordered by `order_entries` (ascending by track
      /// `id`), which is what makes both full-rebuild paths establish disambiguation
      /// order locally rather than inheriting it from `list_tracks`'s `ORDER BY id`
      /// (#188): the build path's insertion order decides which member of a colliding
      /// path keeps the bare name, and that must match the incremental path's min-id
      /// rule regardless of the source query's ordering.
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
          Ok((Self::order_entries(entries), snapshot))
      }

      /// Establish the canonical full-rebuild order: ascending by track `id`. This
      /// is the single point that fixes which member of a colliding rendered path
      /// keeps the bare name in `build_with_ci`'s insertion order (#188); it must NOT
      /// move into the build primitive, whose `tree.rs` tests feed it id-unordered
      /// entries on purpose. Kept as a pure helper so its sort is observable (and
      /// mutation-testable) independent of `list_tracks`'s incidental `ORDER BY id`.
      fn order_entries(mut entries: Vec<(i64, String)>) -> Vec<(i64, String)> {
          entries.sort_by_key(|(id, _)| *id);
          entries
      }
  ```

- [ ] **Step 2: Write the regression unit test for `order_entries`.** Append to `musefs-core/src/facade.rs` inside `mod tests` (after the final existing test, before the module's closing `}`). This is the test that FAILS if the sort is removed or mutated — it feeds a descending pair a real `Db` could never produce:

  ```rust
      #[test]
      fn order_entries_sorts_ascending_by_id() {
          // A real Db never hands render_entries id-unordered rows (list_tracks is
          // ORDER BY id), so this descending input is constructed directly to pin
          // the sort itself. Deleting/mutating order_entries' sort fails this test.
          let unordered = vec![
              (9_i64, "z.flac".to_string()),
              (2_i64, "a.flac".to_string()),
              (5_i64, "m.flac".to_string()),
          ];
          let ordered = Musefs::order_entries(unordered);
          let ids: Vec<i64> = ordered.iter().map(|(id, _)| *id).collect();
          assert_eq!(ids, vec![2, 5, 9], "order_entries must sort ascending by id");
          // The pairing is preserved, not just the id column.
          assert_eq!(
              ordered,
              vec![
                  (2_i64, "a.flac".to_string()),
                  (5_i64, "m.flac".to_string()),
                  (9_i64, "z.flac".to_string()),
              ]
          );
      }
  ```

- [ ] **Step 3: Run the regression test, confirm it passes with the sort in place.** Run:

  ```
  cargo test -p musefs-core order_entries_sorts_ascending_by_id
  ```

  Expected: `test ... ok` (the sort is present).

- [ ] **Step 4: Prove the test bites — remove the sort, see it FAIL, then restore.** This is the verification that the committed test actually guards the production sort (not a coincidence). Temporarily change `order_entries`'s body to `entries` (drop the `sort_by_key` line) and run:

  ```
  cargo test -p musefs-core order_entries_sorts_ascending_by_id
  ```

  Expected: the test FAILS — `assertion `left == right` failed`, `left: [9, 2, 5]`, `right: [2, 5, 9]`. **Restore the `entries.sort_by_key(|(id, _)| *id);` line immediately.** No commit happens in this step; it is the manual bite-check that mirrors what the mutation gate does to this line.

- [ ] **Step 5: Re-run green and run the broader facade suite.** With the sort restored:

  ```
  cargo test -p musefs-core order_entries_sorts_ascending_by_id
  cargo test -p musefs-core facade
  ```

  Expected: all pass (including the unchanged `render_entries_returns_paths_and_snapshot`, which exercises `render_entries` end-to-end through `order_entries`).

- [ ] **Step 6: Commit (green).** Stage the single changed file (production helper + test live in it) and commit:

  ```
  git add musefs-core/src/facade.rs
  git commit -F - <<'EOF'
  fix(core): sort render_entries by track id for full-rebuild determinism (#188)

  Both full-rebuild paths (build_full, rebuild_full) sourced their entry
  order purely from list_tracks's ORDER BY id, so a track's inode could
  flip if that clause changed or the source stopped being id-ordered.
  Extract the ordering into a pure order_entries helper that render_entries
  calls in its return, so the order is self-established at one local point
  matching the incremental min-id rule. The helper is unit-tested directly
  on a descending input (which a real Db can never produce), making the
  sort observable and mutation-testable independent of list_tracks's now
  incidental ORDER BY id.

  Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
  EOF
  ```

---

## Task 2: DB-backed full-rebuild disambiguation test (end-to-end documentation)

A test that drives a colliding pair through the **production** full-rebuild path (`render_entries` → `build_full` → `build_with_ci`) on a real `Db` and asserts the lower id owns the bare name — identical to the incremental path's min-id rule. This documents the end-to-end guarantee. It is NOT the regression bite (the DB input is already id-ordered, so it passes with or without the sort); Task 1's `order_entries` test is the bite. This test's value is pinning the *observable contract* — that the production path gives the bare colliding name to the lower id.

**Files:**
- Test: `musefs-core/src/facade.rs` — inside `mod tests` (begins `facade.rs:1108`), append a new `#[test] fn`.

### Why this seam

`build_full`/`rebuild_full` get their entries only from `render_entries`, so we cannot inject id-unordered entries through them. Instead we build a real in-memory `Db`, upsert two tracks whose `$title` both render to `"Same"` (→ colliding `"Same.flac"`), and call `build_full` (the production full-rebuild) on it. SQLite assigns the first-inserted track the lower id; `build_full` inserts ascending by id (via `order_entries`), so the lower id claims `"Same.flac"` and the higher gets `"Same (2).flac"` — matching `tree.rs`'s `introducing_id` min-id rule. The assertion is on the observable disambiguation, not on re-sorting entries in the test body.

### Steps

- [ ] **Step 1: Write the DB-backed full-rebuild test.** Append to `musefs-core/src/facade.rs` inside `mod tests`. It uses `Db::open_in_memory` (`musefs-db/src/lib.rs:61`), `upsert_track` (`musefs-db/src/tracks.rs:178`), `replace_tags` + `Tag::new` (`musefs-db/src/tags.rs:174`, `musefs-db/src/models.rs:125`), `Musefs::build_full` (`facade.rs:318`), and `InodeAllocator::new` (in scope via the module-level `use crate::tree::{InodeAllocator, ...}` at `facade.rs:16`):

  ```rust
      #[test]
      fn full_rebuild_gives_bare_colliding_name_to_lower_id() {
          use musefs_db::{Format, NewTrack, Tag};
          use std::collections::BTreeMap;

          let db = musefs_db::Db::open_in_memory().unwrap();
          // Two tracks whose `$title` both render to "Same" -> colliding "Same.flac".
          // Insertion order fixes ascending ids: id_a < id_b.
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

          let config = MountConfig {
              template: "$title".to_string(),
              fallbacks: BTreeMap::new(),
              default_fallback: "Unknown".to_string(),
              mode: Mode::Synthesis,
              poll_interval: std::time::Duration::ZERO,
              case_insensitive: false,
          };
          let template = Template::parse(&config.template);

          let mut alloc = InodeAllocator::new();
          let (tree, _snapshot) = Musefs::build_full(&db, &template, &config, &mut alloc).unwrap();

          let root = VirtualTree::ROOT;
          let bare = tree.lookup(root, "Same.flac").expect("bare name exists");
          let suffixed = tree.lookup(root, "Same (2).flac").expect("suffixed name exists");
          // The LOWER id owns the bare name; the higher id is disambiguated. This
          // matches the incremental path's min-id rule (tree.rs introducing_id).
          assert_eq!(tree.inode_of_track(id_a), Some(bare));
          assert_eq!(tree.inode_of_track(id_b), Some(suffixed));
      }
  ```

  Helper names verified this session: `Template::parse(&str) -> Template` is **infallible** (`template.rs:29`, returns `Template`, NOT `Result`) — call it WITHOUT `.unwrap()`; the existing `render_entries_returns_paths_and_snapshot` test calls it the same way (`facade.rs:1369`). `build_full<M>(db, template, config, alloc)` returns `Result<(VirtualTree, HashMap<...>)>` (`facade.rs:318`). `render_entries` is generic over `M`, so passing the `Db<ReadWrite>` from `open_in_memory` directly works — no `into_read_only` needed. `lookup(parent, name) -> Option<u64>` (`tree.rs:170`), `inode_of_track(id) -> Option<u64>` (`tree.rs:193`), `VirtualTree::ROOT` (used at `tree.rs:1230`). `Tag::new(key, value, ordinal)` (`models.rs:125`). `Format::Flac` → `render_one` appends `.flac`.

- [ ] **Step 2: Run the test, confirm it passes.** Run:

  ```
  cargo test -p musefs-core full_rebuild_gives_bare_colliding_name_to_lower_id
  ```

  Expected: `test ... ok`. (Honest note: this passes with OR without the Task 1 sort, because the DB hands `render_entries` id-ascending rows either way — `list_tracks` is `ORDER BY id`. It documents the observable contract; the bite against sort removal is Task 1's `order_entries_sorts_ascending_by_id`, verified in Task 1 Step 4. Do NOT expect this test to fail when the sort is removed.)

- [ ] **Step 3: Commit (green).** Stage and commit:

  ```
  git add musefs-core/src/facade.rs
  git commit -F - <<'EOF'
  test(core): pin full-rebuild colliding-name disambiguation to id order (#188)

  Drives a colliding pair through the production full-rebuild path
  (render_entries -> build_full -> build_with_ci) on a real in-memory Db
  and asserts the lower id owns the bare "Same.flac" while the higher id is
  disambiguated to "Same (2).flac", matching the incremental min-id rule.
  Documents the observable contract; the order_entries unit test is the
  regression guard for the sort itself.

  Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
  EOF
  ```

---

## Task 3: Update the coupling comment and sweep the oracle for consistency

The spec requires (a) updating the test comment in `apply_changes_handles_dir_vs_file_min_id_flip` (`tree.rs:1353-1355`) so it documents the new behavior as the spec rather than a coincidence, and (b) sweeping `assert_apply_matches_build` (`tree.rs:1451-1485`) for consistency so the oracle and production agree that sorting lives in `render_entries`. No production code changes here — the build primitive must NOT sort (the anti-requirement).

**Files:**
- `musefs-core/src/tree.rs` — comment in `apply_changes_handles_dir_vs_file_min_id_flip` (`tree.rs:1353-1355`); oracle helper `assert_apply_matches_build` doc comment (`tree.rs:1451-1456`, with the explicit `after_sorted.sort_by_key` at `tree.rs:1469` and `build_with` calls at `tree.rs:1466`, `tree.rs:1478`).

### Context (verified this session)

- `tree.rs:1353-1355` currently reads:
  ```
          // Production always feeds `build_with` entries sorted ascending by id
          // (`list_tracks` ORDER BY id; `rebuild_incremental` sort_by_key). The
          // reference must use that same canonical order to be a meaningful oracle.
  ```
  This attributes the ordering to `list_tracks`'s `ORDER BY` and the incremental `sort_by_key` — exactly the coincidence #188 removes. After the fix the canonical order is established by `render_entries`'s `order_entries` sort.
- `assert_apply_matches_build` (`tree.rs:1451`) already sorts `after` by id (`after_sorted.sort_by_key(|(id, _)| *id)`, `tree.rs:1469`) before both the `apply_changes` `new_paths` map and the `build_with` reference (`tree.rs:1478`). This is correct and stays — it mirrors production: the reference oracle must use the same id-sorted order production now establishes in `render_entries`. The sweep confirms the *attribution* in comments points at `render_entries`, not `list_tracks`.
- The facade debug-assert and Stage-B fallback in `rebuild_incremental` (`facade.rs:409-546`) build their `entries` from `snap.iter()` — a `HashMap`, so iteration order is nondeterministic — and `sort_by_key(|(id, _)| *id)` them before `build_with_ci`. **These sorts are load-bearing in their own right and are NOT made redundant by the `render_entries` sort** (they never pass through `render_entries`; they sort an unordered HashMap snapshot directly). The spec's "redundant-but-harmless" wording refers only to the abstract Stage-B *re-sort vs. the render order* and is loose; do NOT remove these sorts. They are out of scope for this plan; the comment sweep is scoped to `tree.rs` oracle/test comments only.

### Steps

- [ ] **Step 1: Update the dir-vs-file flip comment to document the new spec.** Use `replace_content` (literal mode) on `musefs-core/src/tree.rs` to change:

  ```
          // Production always feeds `build_with` entries sorted ascending by id
          // (`list_tracks` ORDER BY id; `rebuild_incremental` sort_by_key). The
          // reference must use that same canonical order to be a meaningful oracle.
  ```

  to:

  ```
          // Production establishes the build path's canonical order by sorting
          // ascending by id in `render_entries` (its `order_entries` helper, #188)
          // — not by inheriting `list_tracks`'s ORDER BY. The reference must use
          // that same canonical order to be a meaningful oracle; the inner build
          // primitive deliberately does NOT sort (these tests feed it id-unordered
          // inputs).
  ```

- [ ] **Step 2: Add the sweep note to the oracle helper doc.** The `assert_apply_matches_build` doc comment (`tree.rs:1451-1456`) describes the oracle. Use `replace_content` (literal mode) to change:

  ```
      /// Oracle helper for the collision pins below: apply `changed`/`added`/`removed`
      /// against `before`, then require full `equiv` (inodes included) with a
      /// `build_with` over `after` on a cloned allocator — the same oracle the
      /// facade's debug-assert uses — AND that exactly `expected_rebuilds`
      /// subtree rebuilds ran (the O(changed) contract: a needless rebuild yields
      /// the same tree, so only the count can pin it).
  ```

  to:

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

- [ ] **Step 3: Confirm no production sort crept into the build primitive (anti-requirement check).** Run the `tree.rs` suite:

  ```
  cargo test -p musefs-core tree
  ```

  Expected: all `tree.rs` tests pass — critically `introducing_id_is_min_descendant_track_id` (`tree.rs:1223`), `apply_changes_handles_dir_vs_file_min_id_flip` (`tree.rs:1336`), and `apply_changes_handles_add_side_min_id_flip` (`tree.rs:1368`), which feed `build_with` id-unordered entries and would fail if the primitive sorted. Their passing confirms the sort stayed out of the build path.

- [ ] **Step 4: Commit (green).** Comment-only changes; the suite is unchanged-green. Stage and commit:

  ```
  git add musefs-core/src/tree.rs
  git commit -F - <<'EOF'
  docs(core): attribute build-order determinism to render_entries, not list_tracks (#188)

  Update the dir-vs-file min-id flip comment and the assert_apply_matches_build
  oracle doc to document that production's canonical id order is established by
  render_entries' order_entries sort, and that the build primitive deliberately
  does not sort (its tests feed it id-unordered inputs). No behavior change.

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

  Expected: all tests pass, including `order_entries_sorts_ascending_by_id`, `full_rebuild_gives_bare_colliding_name_to_lower_id`, and the unchanged `tree.rs` min-id flip tests.

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

  Expected: `67:        let mut stmt = self.conn.prepare_cached(track_select!("ORDER BY id"))?;` still present (the `ORDER BY id` is on the `prepare_cached` line; `list_tracks` spans `tracks.rs:66-70`). The plan must NOT have removed it.

- [ ] **Step 6: Local mutation bite on the changed lines (optional but recommended — matches the CI in-diff gate).** The CI `in-diff` job (`.github/workflows/mutants.yml`) mutates exactly the lines this PR changed. Run it locally over the diff to confirm `order_entries`'s sort is killed and no survivor is introduced:

  ```
  git diff main...HEAD -- '*.rs' > /tmp/b188.diff
  cargo mutants --in-diff /tmp/b188.diff -j2 --exclude 'musefs-latencyfs/**'
  ```

  Expected: no surviving mutants. In particular the `order_entries` sort mutants (return-unsorted / return-empty) are CAUGHT by `order_entries_sorts_ascending_by_id`. If a survivor appears, the fix is not protected — do not merge.

---

## Self-review against the spec's Plan B section

- **Sort in `render_entries`** → Task 1, Step 1 (the `order_entries` helper, sorting ascending by id, called from `render_entries`'s return). ✔
- **Anti-sort in `build_with_ci`/`build_with`** → No production edit to `tree.rs`'s build primitive; Task 3 Step 3 explicitly verifies the id-unordered build tests (`introducing_id_is_min_descendant_track_id`, both min-id flip tests) still pass, proving the sort stayed out of the primitive. ✔
- **Comment update at the dir-vs-file flip test** → Task 3, Step 1 (`tree.rs:1353-1355`). ✔
- **Sweep `assert_apply_matches_build`** → Task 3, Step 2 documents that the oracle's existing id-sort (`tree.rs:1469`) mirrors `render_entries`; the `build_with` calls (`tree.rs:1466`, `tree.rs:1478`) are confirmed correct and unchanged. ✔
- **Test feeding the full-rebuild path a colliding pair → lower id keeps the bare name (matching incremental disambiguation)** → Task 2 (`full_rebuild_gives_bare_colliding_name_to_lower_id`), through the real `render_entries`→`build_full` path. ✔
- **Committed test that fails if the production sort is removed** → Task 1's `order_entries_sorts_ascending_by_id` (the regression guard; bite verified in Task 1 Step 4 and by the local mutation run in Task 4 Step 6). The DB-backed Task 2 test does NOT bite (DB input is already id-ordered) and the plan says so honestly. ✔
- **`list_tracks` `ORDER BY id` kept (demoted, not removed)** → Task 4, Step 5 verifies. ✔
- **No red-test commits** → every code/comment change is bundled with its green state; the only failing-test states (Task 1 Step 4 bite-check) are explicitly manual and never committed. ✔
- **No placeholders; real symbol names** → all code verified against `template.rs` (`parse` infallible, `:29`), `facade.rs` (`render_entries` `:287-316`, `build_full` `:318`, `mod tests` `:1108`, `render_entries_returns_paths_and_snapshot` `:1341`, module import of `InodeAllocator`/`VirtualTree` `:16`), `tree.rs` (`build_with_ci` `:117`, `lookup` `:170`, `inode_of_track` `:193`, `disambiguate` `:277`, `ROOT` use `:1230`, dir-vs-file flip `:1336`, `assert_apply_matches_build` `:1451`), `lib.rs` (`open_in_memory` `:61`), `tracks.rs` (`upsert_track` `:178`, `list_tracks` `:66-70`), `tags.rs` (`replace_tags` `:174`), `models.rs` (`NewTrack` `:97`, `Tag::new` `:125`). ✔
