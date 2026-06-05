# HeaderCache → quick_cache Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace `HeaderCache`'s hand-rolled sharded LRU in `musefs-core/src/reader.rs` with quick_cache 0.6.23, keeping the five-method public API byte-for-byte stable (issue #136).

**Architecture:** `HeaderCache` keeps its public surface (`new`, `with_budget`, `resolve`, `retain`, `remove`) and its `build` method untouched; only the storage swaps — `Vec<Mutex<Shard>>` + hand-rolled doubly-linked LRU becomes one `quick_cache::sync::Cache<i64, Arc<ResolvedFile>, CacheBytesWeighter>` (S3-FIFO, byte-weighted via `cache_bytes.max(1)`, internally sharded, eager-ish eviction). `facade.rs`, benches, and integration tests need zero changes.

**Tech Stack:** Rust, quick_cache 0.6.23 (pinned: `retain()` is absent in early 0.6.x), criterion benches, cargo-mutants in-diff gate.

**Spec:** `docs/superpowers/specs/2026-06-05-header-cache-quick-cache-design.md` — read it first; it records the decided trade-offs (S3-FIFO ≠ LRU is accepted; no hot-entry-survival test, it's a flaky-CI trap; budget assertions are end-state only because per-insert-synchronous eviction is undocumented).

**Worktree note:** Execute on a feature branch (main is protected). If using a worktree, create it via superpowers:using-git-worktrees.

---

## File map

| File | Change |
|---|---|
| `musefs-core/Cargo.toml` | add `quick_cache = "0.6.23"` |
| `musefs-core/src/reader.rs` | delete `CACHE_SHARDS`, `LruNode`, `Shard` + its impls; add `CacheBytesWeighter`; rewrite `HeaderCache` internals; delete 11 shard tests; add 2 property tests |
| `musefs-core/src/lock.rs` | module-doc audit list only (HeaderCache line) |
| `BENCHMARKS.md` | before/after `read_throughput` table |

Everything else (facade.rs, benches/read_throughput.rs, tests/) compiles unchanged because the public API is stable.

---

### Task 1: Branch + baseline bench

**Files:** none modified.

- [ ] **Step 1: Create the branch**

```bash
cd /home/cfutro/git/musefs
git checkout main && git pull
git checkout -b header-cache-quick-cache
```

- [ ] **Step 2: Record the baseline serve-path bench**

```bash
cargo bench -p musefs-core --bench read_throughput 2>&1 | tee /tmp/bench-before.txt
```

Expected: criterion output with timings for the bench groups (e.g. `cold_first_read/...`, `seek_read/...`). Criterion saves the baseline under `target/criterion/` and will print change estimates on the "after" run. Keep `/tmp/bench-before.txt` for Task 7's BENCHMARKS.md table.

### Task 2: Add the quick_cache dependency

**Files:**
- Modify: `musefs-core/Cargo.toml` (the `[dependencies]` table, currently lines 12–22)

- [ ] **Step 1: Add the dependency**

In `musefs-core/Cargo.toml`, `[dependencies]` table (keep alphabetical order — insert between `parking_lot` and `sharded-slab`):

```toml
quick_cache = "0.6.23"
```

The patch-level minimum matters: `retain()` does not exist in early 0.6.x (verified absent in 0.6.0/0.6.2/0.6.9). Do NOT set `default-features = false` — the default `parking_lot` feature is wanted (musefs-core already depends on parking_lot 0.12 directly).

- [ ] **Step 2: Verify it builds and the API assumptions hold**

```bash
cargo build -p musefs-core
```

Expected: clean build, `Compiling quick_cache v0.6.x` (x ≥ 23) in the output.

Then confirm the four API signatures this plan leans on exist in the resolved version (one command, no network):

```bash
cargo doc -p quick_cache --no-deps -q && grep -o 'fn with_weighter\|fn retain\|fn weight\|fn remove' target/doc/quick_cache/sync/struct.Cache.html | sort -u
```

Expected output contains all of: `fn remove`, `fn retain`, `fn weight`, `fn with_weighter`. If any is missing, STOP and check docs.rs/quick_cache for the resolved version — do not improvise a replacement primitive.

- [ ] **Step 3: Commit**

```bash
git add musefs-core/Cargo.toml Cargo.lock
git commit -m "$(cat <<'EOF'
Add quick_cache 0.6.23 dependency to musefs-core (#136)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Task 3: Swap the HeaderCache internals

**Files:**
- Modify: `musefs-core/src/reader.rs` — imports (lines 1–13), the `CACHE_SHARDS`/`LruNode`/`Shard` block (lines 40–158), the `HeaderCache` struct (lines 160–165), `impl HeaderCache`'s non-`build` methods (lines 185–236), and 11 tests in `cache_bound_tests`

The surviving test suite is the safety net for this refactor: `header_cache_resolve_caches_by_content_version`, `resolve_is_safe_under_concurrent_access`, `header_cache_retain_drops_absent_tracks` (this one also catches an inverted `retain` predicate polarity instantly), `header_cache_remove_drops_one_track_only`, `default_cache_budget_is_64_mib`, `read_segments_returns_empty_past_end_of_range`, and the `build_*` tests all pass through the public API and must stay green unmodified.

- [ ] **Step 1: Update the imports**

`HashMap` is used only by `Shard`; after the swap it's dead. Replace line 1 and add the quick_cache imports after the `musefs_format` block (line 8):

```rust
use std::collections::HashSet;
```

and (new lines, with the other extern imports):

```rust
use quick_cache::sync::Cache;
use quick_cache::Weighter;
```

`use std::sync::Mutex;` stays — `ResolvedFile::last_page` still uses it.

- [ ] **Step 2: Delete the hand-rolled cache**

Delete all of (current lines 40–158):

- `const CACHE_SHARDS: usize = 16;`
- `struct LruNode { ... }`
- `struct Shard { ... }` (with its doc comment)
- `impl Shard { ... }` (`new`, `unlink`, `push_front`, `get`, `insert`, `retain_keys`, `remove_key`)
- `impl crate::lock::Clearable for Shard { ... }`

- [ ] **Step 3: Add the weighter and the new struct**

Where the `HeaderCache` struct currently sits (after `ResolvedFile`, before `DEFAULT_CACHE_BUDGET`):

```rust
/// Weighs an entry by its resident inline bytes. The `.max(1)` is load-bearing:
/// quick_cache ignores zero-weight entries when evicting, and every
/// StructureOnly layout has `cache_bytes == 0`, so an unweighted entry would
/// escape the byte budget entirely.
#[derive(Clone)]
struct CacheBytesWeighter;

impl Weighter<i64, Arc<ResolvedFile>> for CacheBytesWeighter {
    fn weight(&self, _key: &i64, val: &Arc<ResolvedFile>) -> u64 {
        val.cache_bytes.max(1)
    }
}

/// A per-mount cache of resolved files keyed by track id; an entry
/// self-invalidates when the track's `content_version` changes. Backed by
/// quick_cache: S3-FIFO eviction, byte-weighted, internally sharded.
pub struct HeaderCache {
    cache: Cache<i64, Arc<ResolvedFile>, CacheBytesWeighter>,
    mode: Mode,
}

/// Default resident-bytes budget for the header cache (64 MiB).
pub const DEFAULT_CACHE_BUDGET: u64 = 64 * 1024 * 1024;

/// Item-count sizing hint for quick_cache's internal structures (not a bound):
/// the default budget over 4 KiB, a typical inline tag region. A const so the
/// arithmetic is outside any function body (nothing for cargo-mutants to chew
/// on — the hint has no observable behavior to pin).
const CACHE_ESTIMATED_ITEMS: usize = (DEFAULT_CACHE_BUDGET / 4096) as usize;
```

(`DEFAULT_CACHE_BUDGET` is unchanged — shown for placement.)

- [ ] **Step 4: Rewrite the non-`build` methods of `impl HeaderCache`**

`build` is untouched. Replace `new`, `with_budget`, `shard` (deleted), `retain`, `remove`, and `resolve` with:

```rust
impl HeaderCache {
    pub fn new(mode: Mode) -> HeaderCache {
        HeaderCache::with_budget(mode, DEFAULT_CACHE_BUDGET)
    }
    pub fn with_budget(mode: Mode, budget: u64) -> HeaderCache {
        HeaderCache {
            cache: Cache::with_weighter(CACHE_ESTIMATED_ITEMS, budget, CacheBytesWeighter),
            mode,
        }
    }
    /// Drop cached resolutions for tracks no longer present (`live` = current ids).
    pub fn retain(&self, live: &HashSet<i64>) {
        self.cache.retain(|id, _| live.contains(id));
    }
    /// Drop one track's cached resolution (changelog-refresh removal path).
    pub fn remove(&self, id: i64) {
        self.cache.remove(&id);
    }
    /// Resolve a track to its layout, caching on a content-version miss. Validation
    /// (`stat`) and synthesis run outside the cache; quick_cache's internal locks
    /// are only touched by the brief get and insert.
    pub fn resolve<M>(&self, db: &Db<M>, track_id: i64) -> Result<Arc<ResolvedFile>> {
        let track = db
            .get_track(track_id)?
            .ok_or(CoreError::TrackNotFound(track_id))?;

        // Always validate the backing file first — a stale file is an error even
        // on a cache hit, because the audio region may have shifted.
        crate::metrics::on_stat();
        let meta = std::fs::metadata(&track.backing_path)?;
        if meta.len() != track.backing_size as u64 || mtime_secs(&meta) != track.backing_mtime {
            return Err(CoreError::BackingChanged(track.backing_path.clone()));
        }

        if let Some(hit) = self.cache.get(&track_id) {
            if hit.content_version == track.content_version {
                return Ok(hit);
            }
        }
        let resolved = self.build(db, &track, &meta)?;
        self.cache.insert(track_id, resolved.clone());
        Ok(resolved)
    }
    // ... build() stays exactly as-is ...
}
```

Behavior notes (from the spec, deliberate):
- quick_cache may decline to *admit* a cold entry under pressure; `resolve` returns the freshly built `Arc` regardless, so a non-admitted entry only means a rebuild on next open.
- No `Mutex<Shard>` remains, so reader.rs no longer calls `crate::lock::lock_or_clear` at all.

- [ ] **Step 5: Delete the 11 implementation-specific tests**

In `mod cache_bound_tests`, delete exactly these (they poke `Shard`/linked-list internals that no longer exist):

1. `shard_evicts_least_recently_used_over_byte_budget`
2. `shard_insert_reaccounts_bytes_on_reinsert`
3. `shard_evicts_and_subtracts_evicted_bytes`
4. `shard_keeps_both_entries_at_exactly_budget`
5. `shard_never_evicts_the_sole_entry_even_over_budget`
6. `shard_reset_clears_all_entries`
7. `shard_remove_key_reaccounts_bytes`
8. `shard_remove_key_is_noop_for_absent_id`
9. `shard_retain_keys_drops_dead_and_reaccounts`
10. `with_budget_divides_evenly_across_shards`
11. `shard_routes_by_modulo_not_division`

Nothing else in the module is touched. In particular `header_cache_retain_drops_absent_tracks` and `header_cache_remove_drops_one_track_only` are public-API tests and MUST stay (despite names that sound cache-internal), and the `entry()` helper STAYS — it builds `ResolvedFile` directly and Task 4's new tests use it.

- [ ] **Step 6: Run the surviving suite**

```bash
cargo test -p musefs-core
```

Expected: PASS, zero failures. Watch specifically for `header_cache_retain_drops_absent_tracks` (catches inverted `retain` polarity — quick_cache keeps entries where the predicate returns `true`, same as std) and `header_cache_resolve_caches_by_content_version` (its `Arc::ptr_eq` now depends on quick_cache admitting a lone small entry into an empty cache — the spec flags this as the test that flexes, not the design, if it ever fails).

- [ ] **Step 7: Commit**

```bash
git add musefs-core/src/reader.rs
git commit -m "$(cat <<'EOF'
Replace HeaderCache's hand-rolled sharded LRU with quick_cache (#136)

The byte-budgeted LRU (HashMap + Option<i64> doubly-linked list with
manual unlink/push_front surgery, 16 Mutex shards) becomes a single
quick_cache::sync::Cache: S3-FIFO, byte-weighted by cache_bytes.max(1),
internally sharded. Public API unchanged. Eviction-order tests that
asserted the removed mechanism are deleted.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Task 4: Property tests for the budget bound and the zero-weight guard

**Files:**
- Modify: `musefs-core/src/reader.rs` (`mod cache_bound_tests`)

These replace the *intent* of the deleted eviction tests and are the in-diff mutation gate's teeth against the weigher (`.max(1)` → kill via the zero-weight test; `weight → 1` → kill via the flood test's `len() < 64`).

- [ ] **Step 1: Write the two tests**

Add to `mod cache_bound_tests` (the `entry()` helper already there builds a `ResolvedFile` with `cache_bytes == inline_len`):

```rust
#[test]
fn cache_weight_stays_within_budget_after_flood() {
    let cache = HeaderCache::with_budget(Mode::Synthesis, 4096);
    for id in 0..64i64 {
        cache.cache.insert(id, entry(0, 256)); // 64 × 256 B = 16 KiB ≫ 4 KiB
    }
    // End-state assertion only: quick_cache does not document per-insert
    // synchronous eviction, so the per-insert bound is not guaranteed.
    assert!(
        cache.cache.weight() <= 4096,
        "total weight {} exceeds the 4096-byte budget",
        cache.cache.weight()
    );
    assert!(
        cache.cache.len() < 64,
        "no eviction happened: all 64 over-budget entries are resident"
    );
}

#[test]
fn zero_cache_bytes_entry_still_weighs_one() {
    // StructureOnly layouts have cache_bytes == 0; the weigher's .max(1) keeps
    // them inside the weighted bound instead of escaping it (quick_cache
    // ignores zero-weight entries when evicting).
    let cache = HeaderCache::with_budget(Mode::StructureOnly, 1024);
    cache.cache.insert(1, entry(0, 0));
    assert_eq!(cache.cache.weight(), 1);
    assert!(cache.cache.get(&1).is_some());
}
```

(Direct `cache.cache.insert` mirrors how the deleted tests drove `Shard` directly; same-module access to the private field is the established pattern here.)

- [ ] **Step 2: Run them**

```bash
cargo test -p musefs-core cache_bound -- --nocapture
```

Expected: PASS including the two new tests. If `cache_weight_stays_within_budget_after_flood` fails on the `weight()` assertion, re-read the spec's eviction-timing note before "fixing" anything — the end-state form is the agreed assertion; a failure here means quick_cache's bound is softer than even the end-state guarantee and the design needs revisiting with the user, not a looser test.

- [ ] **Step 3: Commit**

```bash
git add musefs-core/src/reader.rs
git commit -m "$(cat <<'EOF'
Property tests: cache byte-budget bound and zero-weight guard (#136)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Task 5: Update lock.rs's poison-audit doc

**Files:**
- Modify: `musefs-core/src/lock.rs:15` (module doc comment only)

- [ ] **Step 1: Fix the audit line**

The module doc audits "every serving-path `std::sync::Mutex`". Replace line 15:

```rust
//!   reader.rs HeaderCache shards  -> cat 1 (clear): pure cache, repopulated from the DB.
```

with:

```rust
//!   reader.rs HeaderCache         -> n/a since #136: backed by quick_cache's own
//!                                    internal locking; no std::sync::Mutex to poison.
```

The `ResolvedFile::last_page` line below it stays — that Mutex still exists.

- [ ] **Step 2: Verify and commit**

```bash
cargo build -p musefs-core
git add musefs-core/src/lock.rs
git commit -m "$(cat <<'EOF'
lock.rs poison audit: HeaderCache no longer holds std Mutexes (#136)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Task 6: Workspace validation

**Files:** none expected to change (fmt may touch reader.rs).

- [ ] **Step 1: Format, lint (all targets!), full test suite**

```bash
cargo fmt --all
cargo fmt --all --check
cargo clippy --all-targets
cargo test
```

Expected: all clean. `--all-targets` is non-negotiable: `benches/read_throughput.rs` and the `tests/` integration files consume `HeaderCache` and only compile under it (this exact trap has bitten before). The Python contrib trees are untouched by this change — no plugin checks needed.

- [ ] **Step 2: Commit any fmt fallout**

```bash
git status --short
# only if fmt changed files:
git add -u && git commit -m "$(cat <<'EOF'
cargo fmt (#136)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Task 7: After-bench + BENCHMARKS.md

**Files:**
- Modify: `BENCHMARKS.md` (append a dated section; match the existing table style — `| workload | before | after | Δ |`)

- [ ] **Step 1: Run the after-bench**

```bash
cargo bench -p musefs-core --bench read_throughput 2>&1 | tee /tmp/bench-after.txt
```

Expected: criterion prints change estimates vs the Task 1 baseline (look for "change: ..." lines; "No change in performance detected" or improvements are both acceptable — this change removes the serve path's last shared lock but the cache was rarely contended in single-reader benches).

- [ ] **Step 2: Record in BENCHMARKS.md**

Append a section in the established format (see the "Ogg representative benches" section near the end of the file for the house style):

```markdown
## 2026-06-05 — HeaderCache: hand-rolled sharded LRU → quick_cache (#136)

S3-FIFO byte-weighted cache replaces the 16-shard Mutex LRU; the serve
path's last shared std lock is gone. `read_throughput` before/after:

| workload | before | after | Δ |
|----------|-------:|------:|--:|
| <fill from /tmp/bench-before.txt and /tmp/bench-after.txt, one row per criterion group> |
```

Fill the table with the real numbers — every group criterion reports, not a selection.

- [ ] **Step 3: Commit**

```bash
git add BENCHMARKS.md
git commit -m "$(cat <<'EOF'
BENCHMARKS.md: read_throughput before/after for the quick_cache swap (#136)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Task 8: In-diff mutation gate (CI parity)

**Files:** none (verification only; test additions if mutants escape).

- [ ] **Step 1: Run the gate exactly as CI does**

Always `-j2`, output on /tmp, do NOT set TMPDIR. Sanity-check the diff first — an empty diff mutates nothing and exits 0, a silent false pass:

```bash
cd /home/cfutro/git/musefs
git diff "$(git merge-base main HEAD)...HEAD" -- '*.rs' > mutants.diff
grep -q '^@@ ' mutants.diff && echo DIFF-OK
cargo mutants --in-diff mutants.diff -j2 --exclude 'musefs-latencyfs/**' --output /tmp/mutants-out/in-diff
```

Expected: `DIFF-OK`, then cargo-mutants ends with **0 missed**. The mutable surface is small: the weigher (`.max(1)`, `weight → 0/1` — killed by Task 4's two tests), the `retain` predicate (polarity/constants — killed by `header_cache_retain_drops_absent_tracks`), `remove` (killed by `header_cache_remove_drops_one_track_only`), and `resolve`'s version/metadata comparisons (killed by the surviving resolve/build tests). `CACHE_ESTIMATED_ITEMS` is a const initializer, outside cargo-mutants' reach by construction.

- [ ] **Step 2: If any mutant is MISSED**

Strengthen or add a test in `cache_bound_tests` to kill it (preferred). Only if it is provably equivalent (no observable behavior difference through any public API), add a documented `exclude_re` entry to `.cargo/mutants.toml` following the house style there — every entry carries a per-class rationale. Then re-run Step 1. Commit whatever changed:

```bash
git add -u
git commit -m "$(cat <<'EOF'
Kill in-diff mutants for the quick_cache swap (#136)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Task 9: Push and PR

- [ ] **Step 1: Pre-push check** (per project memory: fmt gate, exit status directly)

```bash
cargo fmt --all --check && echo FMT-OK
```

Expected: `FMT-OK`.

- [ ] **Step 2: Push and open the PR**

```bash
git push -u origin header-cache-quick-cache
gh pr create --title "Replace HeaderCache's hand-rolled sharded LRU with quick_cache (#136)" --body "$(cat <<'EOF'
Closes #136.

`HeaderCache`'s 16-shard Mutex LRU (HashMap + Option<i64> doubly-linked
list with manual unlink/push_front surgery, hand-kept byte accounting)
is replaced by `quick_cache::sync::Cache`: S3-FIFO eviction, byte-weighted
by `cache_bytes.max(1)`, internally sharded, all `&self`. Public API
(`new`/`with_budget`/`resolve`/`retain`/`remove`) is unchanged, so
facade.rs, benches, and integration tests are untouched.

Design: `docs/superpowers/specs/2026-06-05-header-cache-quick-cache-design.md`

- Weigher floors at 1: StructureOnly entries have `cache_bytes == 0` and
  zero-weight entries escape quick_cache's weighted bound.
- Eviction-order tests asserting the removed mechanism are deleted;
  end-state budget-bound + zero-weight property tests replace their intent
  (S3-FIFO admission is probabilistic — exact-victim assertions would flake).
- lock.rs poison audit updated: no HeaderCache std Mutex remains.
- BENCHMARKS.md: read_throughput before/after.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

Expected: PR URL printed. CI must show the required `ci-ok` / `coverage-ok` aggregator checks green before merge (main is ruleset-protected).

---

## Self-review notes (done at planning time)

- **Spec coverage:** dependency+pin (Task 2), struct/weigher/API swap (Task 3), `.max(1)` zero-weight guard (Tasks 3+4), end-state budget test (Task 4), test split survive/rewrite/delete (Tasks 3+4), lock.rs doc (Task 5), clippy `--all-targets`/fmt/workspace tests (Task 6), BENCHMARKS.md (Tasks 1+7), mutation gate (Task 8). The spec's `estimated_items_capacity = budget / 4096` is implemented as the `CACHE_ESTIMATED_ITEMS` const derived from `DEFAULT_CACHE_BUDGET` instead of per-call arithmetic — same intent (a sizing hint), deliberately const so the unobservable arithmetic can't produce unkillable mutants (spec explicitly allows the implementer to adjust).
- **Type consistency:** `CacheBytesWeighter` (Tasks 3, 4), `cache` field name (Tasks 3, 4), `CACHE_ESTIMATED_ITEMS` (Task 3 only), `with_budget(Mode, u64)` unchanged everywhere.
- **No placeholders:** every code step carries the actual code; the only deliberate fill-in is the BENCHMARKS.md numbers table, which cannot exist until the benches run.
