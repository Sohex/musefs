# Phase 4b — DB Hardening (Design)

**Source audit:** `docs/audits/2026-05-29-test-audit.md` (the `musefs-db`
mutation survivors from the phase-1 inventory + findings #10, #11, #12)
**Created:** 2026-05-29
**Status:** design — awaiting plan

## Goal

Drive the **`musefs-db` mutation survivors** toward zero with additive tests and a
small, feature-gated test affordance, and close findings **#10/#11/#12**. This is
the **db** slice of Phase 4 (Core & DB); Phase 4a (`musefs-core`) is already
complete. Finishing 4b closes Phase 4.

The phase-1 inventory recorded, for `musefs-db`:

- **2 missed survivors** — `lib.rs:55` (`Db::user_version → Ok(1)`) and
  `schema.rs:93` (`< → <=` in `migrate`).
- **20 unviable mutants** that replace a function body with `Ok(Default::default())`
  but don't compile because the relevant types have no `Default` — concentrated in
  `tags.rs` (8), `tracks.rs` (5), `lib.rs` (4), `art.rs` (3). Unviable mutants are
  not survivors, but they are **blind spots**: 20 functions whose whole-body
  replacement is never tested. Making them viable and then killing the resulting
  survivors is in scope.

**All changes are additive tests plus one feature-gated `Default` affordance; no
production logic change is expected** (these are coverage gaps and a tool
limitation, not bugs). The byte-identity invariant is untouched: the db layer does
not perform positioned backing reads.

## Decomposition decisions (carried from brainstorming)

- **Phase 4 splits into 4a (core, done) and 4b (db, this doc).**
- **Finding #15 (ESTALE)** was discharged in 4a (it concerns the core backing-read
  path), not here.
- **`Default for Db`** (and the model-struct `Default`s) is the headline 4b
  decision — resolved below in B1.

## Guiding principle: verify, don't trust

The 2026-05-29 audit was executed by a weaker model; its findings are **leads, not
facts.** Every survivor and finding below was re-read against the actual source on
the 4b base before this design was written, and two audit/inventory claims were
already corrected:

1. **The inventory's "`Db` has no `Default`" is an oversimplification.** Most
   `tags`/`tracks`/`art` methods return `i64`/`Vec`/`Option`/`()` — all already
   `Default`-able, so `Ok(Default::default())` compiles and those mutants are
   *viable* (and mostly caught). Only `lib.rs`'s `Db`-returning fns
   (`open`/`open_in_memory`/`open_readonly`) genuinely need `Db: Default`; the
   `tags`/`tracks`/`art` unviables need `Default` on the **model structs** they
   construct (`Track`, `Art`, `ArtMeta`, `Tag`, `TrackArt`) and on the `Format`
   enum. The exact set is confirmed empirically from the campaign's
   `mutants.out/unviable.txt` (see B1).
2. **Finding #11's "concurrent-deletion race" does not exist.** `gc_orphan_art` is
   a single `DELETE … WHERE id NOT IN (SELECT art_id FROM track_art)` statement —
   there is no in-process race path to test. The real gaps are the *positive* GC
   cases (still-referenced art survives; exact removed count) and `set_track_art`
   relink/unlink semantics. Reframed in B4.
3. **Finding #12's "GROUP BY assembly" is a misnomer.** `tags_grouped` groups
   **Rust-side via a `HashMap`** (`out.entry(track_id).or_default().push(tag)`),
   not SQL `GROUP BY`. The gaps are the empty/single/multi-value/multi-track
   accumulation cases. Reframed in B5.

Line numbers in the inventory are approximate (captured at CI sha `81d6d845d`);
**locate every target by its code construct, never by the raw line number**, and
re-confirm before each kill.

## The `mutants` feature (B1, the enabling decision)

cargo-mutants mutates `src/` in a non-test build, so a `#[cfg(test)]` affordance is
invisible to it; the `Default`s must exist in the (feature-gated) library build.
A new **`mutants`** Cargo feature on `musefs-db` carries every `Default` needed to
make the 20 `Ok(Default::default())` mutants compile, kept out of the normal and
public build. The name mirrors `musefs-format`'s existing `fuzzing` feature
(feature named after the testing activity that requires it).

- `#[cfg(feature = "mutants")] impl Default for Db` → an **in-memory, unmigrated**
  connection. Unmigrated is deliberate: it leaves `user_version == 0`, which is
  observably distinct from the always-migrated `1` and so **kills the `lib.rs:55`
  `user_version → Ok(1)` mutant** (see B2). A panic on the (practically
  impossible) in-memory open failure is acceptable for a test-only affordance.
  - **Pragma caveat (reviewer-flagged):** `Default` does **not** call
    `configure()`, so without care it would also skip `foreign_keys = ON` and the
    busy-timeout. To avoid a subtly half-configured handle, `Default` still sets the
    `foreign_keys`/`busy_timeout` pragmas — it only skips `schema::migrate()` (and
    WAL, which is file-only). It nonetheless has **no schema** (no tables), so a
    `Db::default()` is usable *only* for the version-0 observability and for making
    the 20 mutants compile. **All behavioral/cascade-asserting tests (B3/B4/B5) use
    `Db::open_in_memory()`** (migrated, version 1, FK on); only the `user_version`
    kill test (B2) uses `Db::default()`. This split is stated so an implementer
    never asserts cascade semantics against the schemaless `Default` handle.
- `#[cfg_attr(feature = "mutants", derive(Default))]` on `Track`, `Art`, `ArtMeta`,
  `Tag`, `TrackArt`, and `Format` (the enum gets `#[cfg_attr(feature = "mutants",
  default)]` on one variant, e.g. `Flac`). Gated, not unconditional, to keep the
  production API clean (consistent with the in-memory `Db: Default` being
  test-only). The precise set is whatever `unviable.txt` shows; the list above is
  the verified prediction from reading each unviable function's return/construction
  type. **`NewTrack`/`NewArt` are intentionally excluded** — they are *input* types,
  never returned (constructed as a return value) by a mutated function, so no
  `Ok(Default::default())` mutant needs their `Default`.
  - These model `Default`s live in `models.rs`, which is **not** in
    `scripts/mutants.sh`'s `--file` list (only `schema/lib/tracks/art/tags.rs` are
    mutated). That is correct and intended: the derives are not themselves mutation
    targets — they exist solely to let the mutants in the five listed files compile.
- The three `Db`-returning fns — `open`, `open_in_memory`, `open_readonly` — are
  the `lib.rs` unviables that need `Db: Default`. All three are expected to be
  **straightforwardly killable** once viable: the `Default` handle is unmigrated
  (version 0), read-write, and non-WAL, so it is observably different from what
  each constructor returns (e.g. `open_readonly` yields a read-only handle to an
  existing schema; `open_in_memory` yields a migrated version-1 handle).
- Wire `--features mutants` into the `musefs-db` leg of `scripts/mutants.sh`
  (the `run_crate musefs-db --test-workspace=true …` call), mirroring the format
  leg's `--features fuzzing`. The PR `--in-diff` gate in `mutants.yml` calls
  cargo-mutants directly without the feature; that is acceptable (fast best-effort
  gate) and noted, not fixed here.
- Add **both** a `cargo clippy -p musefs-db --features mutants --all-targets -D
  warnings` step **and** a `cargo test -p musefs-db --features mutants` step to
  `ci.yml`'s `check` job. The clippy step is required because the existing repo-wide
  Clippy step runs without the feature, so the gated `Default` code would otherwise
  never be linted in CI.

Tests that depend on these `Default`s are themselves `#[cfg(feature = "mutants")]`
and run via `cargo test -p musefs-db --features mutants` — added alongside the
normal test invocations so they are exercised outside the campaign too.

## The hand-apply verification method (use for every kill)

cargo-mutants is not available locally. For each targeted
`function: construct: mutation`:

1. Run the new test → it passes (production code is correct).
2. Locate the construct by pattern, apply the exact mutation, rerun **just that
   test** → it must **fail** (a failed assertion *or* a panic both count).
3. Revert (`git checkout -- <file>`), rerun → passes again.

If step 2 still passes, strengthen the test, or — if the mutation provably yields
identical behavior — record it as an **equivalent mutant** with a one-line
rationale (inventory row gets `missed → **equivalent**`). Never leave a mutation
applied.

For the `Default`-dependent kills (the 20 newly-viable + `user_version`), the
authoritative check is the `--features mutants` campaign; the hand-apply is run
under `cargo test -p musefs-db --features mutants`.

## Test placement

`musefs-db` tests are **integration tests** under `musefs-db/tests/` (each file is
its own crate; `common/mod.rs` holds shared helpers). 4b **extends the existing
files** beside the behavior under test — `tests/tracks.rs`, `tests/art.rs`,
`tests/tags.rs`, `tests/schema.rs` — rather than adding the audit's suggested new
`tracks_cascade.rs`/`art_gc.rs`/`tags_grouping.rs`, matching the established pattern
and keeping the kill→test mapping unambiguous. Feature-gated kill tests live in the
same files behind `#[cfg(feature = "mutants")]`.

## Components

### B1 — `mutants` feature + `Default` affordance + harness wiring

As described above. Deliverables: the feature in `musefs-db/Cargo.toml`, the
gated `impl Default for Db` and model `Default`s, the `--features mutants` wiring
in `scripts/mutants.sh`, and a `cargo test -p musefs-db --features mutants` step in
the normal test CI. Opening the campaign = dispatch `mutants.yml`
(`workflow_dispatch`) on the 4b branch.

### B2 — known kills / equivalents

- **`lib.rs:55` `Db::user_version → Ok(1)`** (inventory line; ~line 54 in current
  source — locate by construct) — killed by a `mutants`-gated test asserting
  `Db::default().user_version()? == 0` (the unmigrated in-memory Db is at version 0;
  the `Ok(1)` mutant would report 1). No non-`Default` route exists: the public API
  only ever yields a migrated, version-1 `Db`.
- **`schema.rs:93` `< → <=` in `migrate`** — the loop body (`if current < target`)
  is only reached when `current < latest`; the fast-path `>= latest` early-return
  plus a single migration (`MIGRATIONS.len() == 1`) means the only reaching state
  is `(current = 0, target = 1)`, where `<` and `<=` agree. Recorded **equivalent**
  with that rationale after hand-apply confirmation. (A second future migration
  would make it killable; out of scope.)

### B3 — finding #10 (tracks): `tests/tracks.rs`

Existing coverage: insert/get-by-id-and-path, upsert keeps same id, list, missing →
`None`, rescan preserves `content_version`, delete cascades tags+track_art.

**Already covered (no new test — strengthen only if a mutant survives):**
`delete_track` leaving the referenced `art` row in place is **already asserted** by
`delete_track_cascades_tags_and_track_art` (`tests/tracks.rs:110-111`:
`assert!(db.get_art(art_id).unwrap().is_some())`). Do not re-add it; revisit only if
the campaign shows a related survivor that this test does not pin.

Add:

- `upsert_track` conflict path updates **every** mutable column
  (`format`/`audio_offset`/`audio_length`/`backing_size`/`backing_mtime`) while
  preserving `id`. The existing `upsert_updates_existing_row_keeping_same_id` checks
  id stability only, not per-column updates — this is the real #10 gap.
- Audit's "untested SQL error branches" re-verified: most are bare `?`-propagation
  with no distinct reachable branch; any genuinely reachable one gets a test, the
  rest are noted as propagation-only.

### B4 — finding #11 (art), reframed: `tests/art.rs`

Existing coverage: sha256 dedup, get data+len, set/get track_art, linking bumps
`content_version`, `read_art_chunk` slice, gc removes unreferenced **including the
`removed == 1` return value and that the referenced row survives**
(`gc_orphan_art_removes_unreferenced_rows`). That single-reference keep/remove case
is therefore **not** re-added. Add only the genuinely-new gaps:

- Art shared by two tracks survives until **both** unlink it (delete one track or
  re-`set_track_art` to drop the link; assert the art row persists, then gc only
  after the last reference is gone).
- `set_track_art` replace semantics: re-link (same art, new ordinal/description),
  unlink (empty items clears the rows), reorder ordinals — it `DELETE`s then
  re-`INSERT`s within one transaction.
- `upsert_art` dedup id-stability: re-inserting identical bytes returns the same id
  (the `ON CONFLICT(sha256) DO NOTHING` + select-id path).

### B5 — finding #12 (tags), reframed: `tests/tags.rs`

Existing coverage: replace+get ordered, replace overwrites, `tags_grouped` returns
all tags by track. Add `tags_grouped` accumulation cases:

- empty db → empty map (no spurious entries);
- single track, multi-value key → values preserved in `(key, ordinal)` order;
- multiple tracks → isolated into distinct map entries (no cross-track bleed);
- grouped ordering matches per-track `get_tags` (the documented drop-in contract).

### B6 — newly-viable survivor sweep (post-campaign)

After the campaign runs with `--features mutants`, download the
`mutants-musefs-db` artifact and enumerate the now-viable survivors among the 20
(`mutants.out/missed.txt` minus what the B2–B5 tests already cover). Kill each via
the hand-apply method (a `mutants`-gated test where the kill needs a `Default`),
recording genuine equivalents with rationale. This component is **method-defined**:
the exact survivor set is not enumerable until the campaign runs, so the plan
specifies the procedure and a sweep checklist, not a fixed test list.

**Scheduling dependency (reviewer-flagged).** The full campaign
(`mutants.yml` `full` job) runs **only** on the weekly Monday cron or via manual
`workflow_dispatch` — it is not a PR gate. B6 therefore depends on a human
dispatching the campaign on the 4b branch and waiting for the artifact, which can
take hours and may not align with the PR's merge window. To keep the main PR
self-contained and not block it on a manual CI step, **B6 is split into its own
follow-up PR**: the 4b feature PR lands B1–B5 + the B7 items that don't depend on
the campaign (feature, `Default`s, harness wiring, the known kills, the #10/#11/#12
coverage, framing corrections), and a second small PR lands the B6 sweep + the
final inventory annotations once the dispatched campaign's artifact is in hand. The
plan should structure the work as these two PRs.

### B7 — docs

- Annotate the inventory's `musefs-db` rows: `lib.rs:55` → killed (4b),
  `schema.rs:93` → equivalent, and the newly-viable survivors from B6 →
  killed/equivalent. Update the unviable note to reflect the corrected
  `Default`-target analysis (models + `Format`, not just `Db`).
- Record the #11 (no race) and #12 (HashMap, not GROUP BY) framing corrections.
- Mark Phase 4 complete (4a + 4b) in the tracking doc.

## Equivalent mutants

- **`schema.rs:93` `< → <=`** — equivalent given the fast-path early-return + single
  migration (B2).
- Any newly-viable B6 mutant that proves equivalent is recorded **then**, with
  hand-apply evidence — none assumed up front beyond `schema.rs:93`.

## Implementation ordering

**PR 1 (main 4b PR):** B1 first (it gates B2's `user_version` kill and the whole
campaign), then B2–B5 in parallel (additive, independent tests), plus the
campaign-independent B7 items (framing corrections, the `lib.rs:55`/`schema.rs:93`
annotations). Dispatch the `mutants.yml` `full` job on the branch as soon as B1 is
pushed, so it runs while B2–B5 are written.

**PR 2 (B6 follow-up):** once the dispatched campaign's `mutants-musefs-db`
artifact is in hand, do the B6 survivor sweep and the final inventory annotations +
Phase 4 completion marker. This split (see B6) keeps PR 1 from blocking on a manual,
hours-long CI dispatch.

## Error handling

No new error paths. Tests assert existing mappings (`DbError` via `?`, FK-cascade
semantics, dedup/GC contracts). If a survivor reveals a real bug, the scoped fix
stays within the owning function; none is expected (coverage gaps + a tool
limitation, not defects). The byte-identity invariant is not in this layer.

## Acceptance

| Component | Check |
|-----------|-------|
| B1 | `mutants` feature compiles; `impl Default for Db` yields version 0; gated model `Default`s present; `scripts/mutants.sh` db leg passes `--features mutants`; CI runs both `cargo clippy -p musefs-db --features mutants …` and `cargo test -p musefs-db --features mutants` |
| B2 | `user_version` test red under `→ Ok(1)` (feature-gated); `schema.rs:93` confirmed equivalent by hand-apply |
| B3 | `delete_track`-leaves-art and `upsert_track` all-columns tests added and green; error-branch audit recorded |
| B4 | gc-keeps-referenced + exact-count, shared-art-survives, `set_track_art` relink/unlink/reorder, `upsert_art` dedup tests added |
| B5 | `tags_grouped` empty/single-multi/multi-track/ordering tests added |
| B6 (PR 2) | every newly-viable db survivor killed or recorded equivalent (from the dispatched campaign's artifact) |
| B7 | framing + known-mutant annotations land in PR 1; final survivor annotations + Phase 4 completion land in PR 2 |
| PR 1 | `cargo test --workspace` + `--features fuzzing` + `cargo test -p musefs-db --features mutants` + `clippy --all-targets -D warnings` + `clippy -p musefs-db --features mutants` + `fmt --check` green; campaign dispatched on the branch |
| PR 2 | the dispatched db campaign shows the 2 known + the 20 unblocked killed or documented-equivalent |
