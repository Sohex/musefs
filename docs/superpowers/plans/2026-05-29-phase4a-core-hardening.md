# Phase 4a — Core Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Drive the `musefs-core` mutation survivors toward zero with additive tests (kill the killable, record genuine/masked equivalents with evidence, record the two `disambiguate` infinite-loop mutants as timeout-detected), and discharge findings #9 and #15.

**Architecture:** All changes are additive `#[cfg(test)]` code. No production logic changes are expected; the one contingency is a small scoped off-by-one fix if a kill reveals a real bug (flag it, never assume it). Tests extend the in-module test modules (`reader.rs`, `scan.rs`, `tree.rs`) for kills that need private access (`Shard`, `HeaderCache`, `read_segments`, `disambiguate`), and extend `musefs-core/tests/facade.rs` (the integration file with the `Musefs` harness) for the facade kills.

**Tech Stack:** Rust, `cargo test`, SQLite (`musefs-db`), `tempfile`. cargo-mutants is **not** available locally — every kill is verified by the hand-apply method below.

---

## The hand-apply verification method (the core rhythm of every task)

cargo-mutants is unavailable locally, so each kill is proven by hand. For a test `T` targeting `function: construct: mutation`:

1. Write `T`. Run it → it **passes** (production code is correct).
2. Open the source, locate the construct **by pattern** (line numbers below are approximate — captured 2026-05-29, locate by the code construct), apply the exact mutation, rerun just `T` → it must **fail** (a failed assertion *or* a panic both count as a kill).
3. `git checkout -- <file>` to revert, rerun `T` → passes again. **Never leave a mutation applied.**

If step 2 still passes: strengthen the test, or — if the mutation provably yields identical behavior — record it as an **equivalent mutant** with the hand-apply evidence instead of contriving a test.

**Timeout mutants are the exception** (Task C4-2): the mutation makes a loop non-terminating, so hand-applying would hang the suite. Confirm by reasoning + a covering test, record as timeout-detected, never hand-apply.

### Standard commands

- Run one test: `cargo test -p musefs-core <test_name>`
- Run a module's tests: `cargo test -p musefs-core <module>::`
- Revert a hand-applied mutation: `git checkout -- musefs-core/src/<file>.rs`
- Final gate (Task C6): `cargo test --workspace && cargo test -p musefs-format --features fuzzing && cargo clippy --all-targets -- -D warnings && cargo fmt --check`

---

## File map

| File | Role in this plan | What changes |
|------|-------------------|--------------|
| `musefs-core/src/reader.rs` | LRU cache + layout build + serve | Extend `mod cache_bound_tests` (C1, C2) |
| `musefs-core/src/scan.rs` | scan/probe/ingest/revalidate | Add `mod hardening_tests` + extend `ogg_probe_tests` (C3, #9) |
| `musefs-core/src/tree.rs` | path disambiguation | Extend `mod tests` (C4) |
| `musefs-core/tests/facade.rs` | facade behavioral tests (has `Musefs` harness) | Add tests using `config()`/`scanned_db()` (C5) |
| `docs/audits/2026-05-29-test-audit.md` | survivor inventory | Annotate rows (C6) |
| `musefs-core/src/reader.rs` | finding #15 | One doc comment near `read_exact_at`/`BackingChanged` (C6) |

---

## Component C1 — reader LRU cache (`Shard`, `HeaderCache` math)

All C1 tests go in `musefs-core/src/reader.rs` inside `mod cache_bound_tests`, which already defines the `entry(content_version, inline_len) -> Arc<ResolvedFile>` helper and imports `Db`, `Format`, `NewTrack`. `Shard`/`HeaderCache` private fields (`bytes`, `budget`) are visible there.

### Task C1-1: `Shard::insert` re-insert byte accounting

**Files:**
- Test: `musefs-core/src/reader.rs` (in `mod cache_bound_tests`)
- Construct under test: `Shard::insert` (`reader.rs` ~line 108): `self.bytes -= old_bytes;` and `self.bytes += add;`

- [ ] **Step 1: Write the test**

```rust
    #[test]
    fn shard_insert_reaccounts_bytes_on_reinsert() {
        let mut s = Shard::new(1000);
        s.insert(1, entry(0, 100));
        assert_eq!(s.bytes, 100);
        // Re-insert the SAME key with a DIFFERENT cache_bytes: old must be
        // subtracted, new added → 100 - 100 + 30 = 30.
        s.insert(1, entry(0, 30));
        assert_eq!(s.bytes, 30);
        assert_eq!(s.map.len(), 1);
    }
```

- [ ] **Step 2: Run → pass**

Run: `cargo test -p musefs-core shard_insert_reaccounts_bytes_on_reinsert`
Expected: PASS.

- [ ] **Step 3: Hand-apply each mutation, confirm fail, revert**

For each, edit `reader.rs`, rerun the test, confirm FAIL, then `git checkout -- musefs-core/src/reader.rs`:
- `self.bytes -= old_bytes;` → `self.bytes += old_bytes;` → bytes becomes 230. FAIL ✓
- `self.bytes -= old_bytes;` → `self.bytes /= old_bytes;` → bytes becomes 31. FAIL ✓
- `self.bytes += add;` → `self.bytes -= add;` → first insert underflows `0 - 100` → panic. FAIL ✓
- `self.bytes += add;` → `self.bytes /= add;` → first insert `0 / 100 = 0`, second `0/30=0`, assert `bytes==30` FAILs. ✓

- [ ] **Step 4: Commit** (deferred to end of C1, Task C1-4)

### Task C1-2: `Shard::insert` eviction guard + eviction subtract

**Files:**
- Test: `musefs-core/src/reader.rs` (in `mod cache_bound_tests`)
- Construct: `Shard::insert` evict loop (~line 113): `while self.bytes > self.budget && self.map.len() > 1 { ... self.bytes -= n.value.cache_bytes; }`

- [ ] **Step 1: Write three tests**

```rust
    #[test]
    fn shard_evicts_and_subtracts_evicted_bytes() {
        let mut s = Shard::new(100);
        s.insert(1, entry(0, 60));
        s.insert(2, entry(0, 60)); // 120 > 100, len 2 > 1 → evict LRU key 1
        assert!(s.get(1).is_none());
        assert!(s.get(2).is_some());
        assert_eq!(s.bytes, 60); // 120 - 60(evicted) = 60
    }

    #[test]
    fn shard_keeps_both_entries_at_exactly_budget() {
        let mut s = Shard::new(100);
        s.insert(1, entry(0, 50));
        s.insert(2, entry(0, 50)); // bytes == 100 == budget → strictly-not-over → keep both
        assert!(s.get(1).is_some());
        assert!(s.get(2).is_some());
        assert_eq!(s.bytes, 100);
    }

    #[test]
    fn shard_never_evicts_the_sole_entry_even_over_budget() {
        let mut s = Shard::new(100);
        s.insert(1, entry(0, 200)); // over budget, but map.len() == 1 → kept
        assert!(s.get(1).is_some());
        assert_eq!(s.bytes, 200);
    }
```

- [ ] **Step 2: Run → all pass**

Run: `cargo test -p musefs-core shard_`
Expected: PASS (all `shard_*` including C1-1).

- [ ] **Step 3: Hand-apply, confirm fail, revert** (revert after each)

- `self.bytes > self.budget` → `self.bytes >= self.budget`: `shard_keeps_both_entries_at_exactly_budget` FAILs (at 100>=100 it evicts key 1). ✓
- `... && self.map.len() > 1` → `... || self.map.len() > 1`: `shard_never_evicts_the_sole_entry_even_over_budget` FAILs (200>100 || … evicts the only entry). ✓
- `self.bytes -= n.value.cache_bytes;` → `+=`: `shard_evicts_and_subtracts_evicted_bytes` asserts `bytes==60`, becomes 180. FAIL ✓
- `self.bytes -= n.value.cache_bytes;` → `/=`: becomes `120 / 60 = 2`. FAIL ✓

### Task C1-3: `Shard::retain_keys`

**Files:**
- Test: `musefs-core/src/reader.rs` (in `mod cache_bound_tests`)
- Construct: `Shard::retain_keys` (~line 124): whole-fn, `filter(|k| !live.contains(k))`, `self.bytes -= n.value.cache_bytes;`

- [ ] **Step 1: Write the test**

```rust
    #[test]
    fn shard_retain_keys_drops_dead_and_reaccounts() {
        use std::collections::HashSet;
        let mut s = Shard::new(1000);
        s.insert(1, entry(0, 100));
        s.insert(2, entry(0, 100));
        s.insert(3, entry(0, 100));
        let live: HashSet<i64> = [2, 3].into_iter().collect();
        s.retain_keys(&live);
        assert!(s.get(1).is_none());
        assert!(s.get(2).is_some());
        assert!(s.get(3).is_some());
        assert_eq!(s.bytes, 200); // dropped exactly the 100 bytes of key 1
    }
```

- [ ] **Step 2: Run → pass**

Run: `cargo test -p musefs-core shard_retain_keys_drops_dead_and_reaccounts`
Expected: PASS.

- [ ] **Step 3: Hand-apply, confirm fail, revert**

- whole body → `{}` (the `→()` mutant): key 1 retained → `get(1).is_none()` FAILs. ✓
- `.filter(|k| !live.contains(k))` → `.filter(|k| live.contains(k))` (delete `!`): drops 2 and 3, keeps 1 → `get(2).is_some()` FAILs. ✓
- `self.bytes -= n.value.cache_bytes;` → `+=` → bytes 400; → `/=` → bytes 3. assert `bytes==200` FAILs. ✓

### Task C1-4: cache constants + `with_budget` + `shard` routing

**Files:**
- Test: `musefs-core/src/reader.rs` (in `mod cache_bound_tests`)
- Constructs: `DEFAULT_CACHE_BUDGET = 64 * 1024 * 1024` (~line 158); `HeaderCache::with_budget` `budget / CACHE_SHARDS` (~line 181); `HeaderCache::shard` `track_id % CACHE_SHARDS` (~line 187).

- [ ] **Step 1: Write the tests**

```rust
    #[test]
    fn default_cache_budget_is_64_mib() {
        // A literal that diverges under *→+ and *→/ at every site.
        assert_eq!(DEFAULT_CACHE_BUDGET, 67_108_864);
    }

    #[test]
    fn with_budget_divides_evenly_across_shards() {
        // 16384 / 16 = 1024; 16384 % 16 = 0 (→ .max(1) = 1); 16384 * 16 = 262144.
        let cache = HeaderCache::with_budget(Mode::Synthesis, 16_384);
        assert_eq!(cache.shard(0).budget, 1024);
    }

    #[test]
    fn shard_routes_by_modulo_not_division() {
        // 1 and 17 share a shard under % (both ≡ 1) but differ under / (0 vs 1).
        let cache = HeaderCache::with_budget(Mode::Synthesis, 16 * 1024 * 1024);
        cache.shard(1).insert(1, entry(0, 50));
        assert!(cache.shard(17).bytes > 0, "17 and 1 must map to the same shard");
        assert_eq!(cache.shard(2).bytes, 0, "2 maps to a different shard");
    }
```

> Note on `shard_routes_by_modulo_not_division`: each `cache.shard(x)` call takes and releases the shard lock within its statement (the guard is a temporary), so there is no deadlock even when two calls resolve to the same mutex.

- [ ] **Step 2: Run → pass**

Run: `cargo test -p musefs-core -- default_cache_budget_is_64_mib with_budget_divides_evenly_across_shards shard_routes_by_modulo_not_division`
Expected: PASS.

- [ ] **Step 3: Hand-apply, confirm fail, revert**

- `64 * 1024 * 1024`: change any `*` to `+` or `/` → value ≠ 67_108_864 → `default_cache_budget_is_64_mib` FAILs. ✓ (verify each of the two `*`.)
- `(budget / CACHE_SHARDS as u64)` → `%` → `16384 % 16 = 0` → `.max(1) = 1` ≠ 1024 FAIL; → `*` → huge ≠ 1024 FAIL. ✓
- `(track_id as u64 % CACHE_SHARDS as u64)` → `/`: id 1 → idx 0, id 17 → idx 1; insert lands on idx 0, `cache.shard(17).bytes` reads idx 1 → 0 → `> 0` FAILs. ✓

- [ ] **Step 4: Commit C1**

```bash
git add musefs-core/src/reader.rs
git commit -m "test(reader): Phase 4a C1 — LRU shard accounting, eviction, retain, cache math kills

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Component C2 — reader layout build & serve (`HeaderCache::build`, `read_segments`)

C2 tests also extend `mod cache_bound_tests` (reuse `entry`, `write_flac_local`, `mtime_secs`, `Db`, `Format`, `NewTrack`). `build` is exercised through the public `resolve`; `read_segments` is private and called directly.

### Verified analysis (read this before writing C2 read tests)

The serve-path range guards are largely **masked**, so most are equivalent — kill what is killable, record the rest with evidence:

- `read_at`'s early-out `offset >= total_len || size == 0` is **re-checked verbatim** at the top of `read_segments`, to which `read_at` always delegates. So `read_at`'s `||→&&` and `>=→>` mutants produce identical output (masked by `read_segments`) → **equivalent**.
- `read_segments`'s `ov_start < ov_end` → `<=` only admits zero-width segments (`n == 0`), which append nothing and never index out of bounds → **equivalent**.
- `read_segments`'s early-out `>=→>` differs only at `offset == total_len`, where `end == offset`, the capacity is 0, and every segment's overlap is empty → identical output → **equivalent**.
- `read_segments`'s early-out `||→&&` is **killable**: with `offset > total_len` and `size != 0`, the mutated guard is false, execution proceeds, `end = min(offset+size, total_len) = total_len < offset`, and `Vec::with_capacity((end - offset) as usize)` underflows (`total_len - offset`) → panic in debug. Correct code returns `Ok(vec![])`.

### Task C2-1: `read_segments` early-out (`||→&&`) + the masked-equivalent records

**Files:**
- Test: `musefs-core/src/reader.rs` (in `mod cache_bound_tests`)
- Construct: `read_segments` (~line 401): `if offset >= resolved.total_len || size == 0 { return Ok(Vec::new()); }`

- [ ] **Step 1: Write the test**

```rust
    #[test]
    fn read_segments_returns_empty_past_end_of_range() {
        let db = musefs_db::Db::open_in_memory().unwrap();
        let resolved = entry(0, 10); // total_len 10, single Inline[10]
        // offset strictly past total_len, non-zero size: correct returns empty.
        let out = read_segments(&resolved, &db, None, 11, 1).unwrap();
        assert!(out.is_empty());
        // Also pin size == 0 at a valid offset.
        let out0 = read_segments(&resolved, &db, None, 0, 0).unwrap();
        assert!(out0.is_empty());
    }
```

- [ ] **Step 2: Run → pass**

Run: `cargo test -p musefs-core read_segments_returns_empty_past_end_of_range`
Expected: PASS.

- [ ] **Step 3: Hand-apply the killable mutation, confirm fail, revert**

- `offset >= resolved.total_len || size == 0` → `... && ...`: with `(11, 1)` the guard is `true && false = false`, execution proceeds, `end - offset = 10 - 11` underflows → **panic**. Test FAILs (panic = kill). ✓ Revert.

- [ ] **Step 4: Record the masked equivalents (no test)**

For each below, hand-apply, run `cargo test -p musefs-core` (the whole crate, since these touch shared serve paths), confirm **still green**, revert, and note for C6's inventory annotation that the mutant is `missed → equivalent (masked)` with the rationale from the analysis above:
- `read_at` early-out `||→&&` and `>=→>` (masked by `read_segments`).
- `read_segments` early-out `>=→>` (masked: `end == offset` ⇒ empty output).
- `read_segments` `ov_start < ov_end` → `<=` (masked: zero-width append is a no-op).

### Task C2-2: `build` audio-bounds guard + `cache_bytes` accounting

**Files:**
- Test: `musefs-core/src/reader.rs` (in `mod cache_bound_tests`)
- Construct: `HeaderCache::build` (~line 248) synthesis guard `audio_offset < 0 || audio_length < 0 || (audio_offset+audio_length) > meta.len()`; and `cache_bytes` fold (~line 348): `Segment::Inline(b) => b.len()`, `.sum::<u64>() + match { Opus|Vorbis|OggFlac => estimated_ogg_index_bytes(...), _ => 0 }`.

This task needs a helper that upserts a track with **chosen** audio bounds over a real FLAC backing file, so `resolve`'s backing size/mtime validation passes and only the audio-bounds guard is exercised.

- [ ] **Step 1: Write a local helper + the tests**

```rust
    // Upsert a track over `path` with explicit audio bounds; returns (db, track_id).
    // Backing size/mtime are taken from the real file so resolve()'s backing
    // validation passes and only build()'s audio-bounds guard is in play.
    fn track_with_bounds(
        path: &std::path::Path,
        audio_offset: i64,
        audio_length: i64,
    ) -> (musefs_db::Db, i64) {
        use musefs_db::{Format, NewTrack};
        let db = musefs_db::Db::open_in_memory().unwrap();
        let meta = std::fs::metadata(path).unwrap();
        let id = db
            .upsert_track(&NewTrack {
                backing_path: path.to_string_lossy().to_string(),
                format: Format::Flac,
                audio_offset,
                audio_length,
                backing_size: meta.len() as i64,
                backing_mtime: mtime_secs(&meta),
            })
            .unwrap();
        (db, id)
    }

    #[test]
    fn build_rejects_audio_region_past_end_of_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.flac");
        let _ = write_flac_local(&path); // ignore real bounds; we set bogus ones
        let len = std::fs::metadata(&path).unwrap().len() as i64;
        // audio_offset valid, audio_length valid, but offset+length > file size.
        let (db, id) = track_with_bounds(&path, len, 5);
        let cache = HeaderCache::new(Mode::Synthesis);
        assert!(matches!(
            cache.resolve(&db, id),
            Err(CoreError::BackingChanged(_))
        ));
    }

    #[test]
    fn build_accepts_audio_region_ending_exactly_at_eof() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.flac");
        let (audio_offset, audio_length) = write_flac_local(&path);
        // write_flac_local places audio at EOF: audio_offset + audio_length == file size.
        let (db, id) = track_with_bounds(&path, audio_offset, audio_length);
        let cache = HeaderCache::new(Mode::Synthesis);
        let resolved = cache.resolve(&db, id).expect("exact-fit bounds must resolve");
        assert!(resolved.total_len > 0);
    }

    #[test]
    fn build_cache_bytes_counts_inline_segments() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.flac");
        let (audio_offset, audio_length) = write_flac_local(&path);
        let (db, id) = track_with_bounds(&path, audio_offset, audio_length);
        let cache = HeaderCache::new(Mode::Synthesis);
        let resolved = cache.resolve(&db, id).unwrap();
        // FLAC layout has a non-empty Inline metadata segment and no ogg index,
        // so cache_bytes == sum(inline lens) > 0. Both the Inline arm-delete and
        // +→* (which multiplies by the 0 ogg term) collapse this to 0.
        let inline_sum: u64 = resolved
            .layout
            .segments()
            .iter()
            .map(|s| match s {
                Segment::Inline(b) => b.len() as u64,
                _ => 0,
            })
            .sum();
        assert!(inline_sum > 0);
        assert_eq!(resolved.cache_bytes, inline_sum);
    }
```

- [ ] **Step 2: Run → pass**

Run: `cargo test -p musefs-core -- build_rejects_audio_region_past_end_of_file build_accepts_audio_region_ending_exactly_at_eof build_cache_bytes_counts_inline_segments`
Expected: PASS.

- [ ] **Step 3: Hand-apply, confirm fail, revert**

Guard `audio_offset < 0 || audio_length < 0 || (...) > meta.len()`:
- `> meta.len()` → `>= meta.len()`: `build_accepts_audio_region_ending_exactly_at_eof` FAILs (exact-fit now errors). ✓
- second `||` → `&&` (i.e. `audio_offset < 0 || (audio_length < 0 && (...) > meta.len())`): `build_rejects_audio_region_past_end_of_file` FAILs (offset+length>len with valid signs no longer errors → resolve returns Ok). ✓

`cache_bytes` fold:
- `Segment::Inline(b) => b.len() as u64` arm → delete (returns 0): `build_cache_bytes_counts_inline_segments` FAILs (`cache_bytes` becomes 0). ✓
- `.sum::<u64>() + match` → `.sum::<u64>() * match`: for FLAC the match is 0, so `inline_sum * 0 = 0`. FAILs. ✓

- [ ] **Step 4: Record the hard/equivalent guard sub-mutants**

The remaining guard sub-mutants are masked or unreachable for real formats — hand-apply each, confirm the crate stays green, revert, and note for C6:
- `audio_offset < 0` → `<=`/`==`, and the first `||` → `&&`: a negative `audio_offset`/`audio_length` casts to a huge `u64`, so the `> meta.len()` term is *also* true — the sign checks are masked by the overflow term. And the only value distinguishing `< 0` from `<= 0` is `0`, which no real audio format produces (every format's audio starts after a header). Record `missed → equivalent (masked / unreachable boundary)`.
- `audio_length < 0` → `<=`/`==`: distinguished only at `audio_length == 0`; a zero-length-audio FLAC is not produced by `write_flac_local` and `synthesize_layout` is not contracted for it. Record as equivalent (unreachable boundary) unless a zero-length fixture proves otherwise.

> If any "equivalent" above turns out killable with a modest fixture, write the kill instead — equivalence is the fallback, not the goal.

### Task C2-3: `build` Ogg-codec `cache_bytes` arm

**Files:**
- Test: `musefs-core/src/reader.rs` (in `mod cache_bound_tests`)
- Construct: `build` `cache_bytes` match arm `Format::Opus | Format::Vorbis | Format::OggFlac => estimated_ogg_index_bytes(track.audio_length as u64)`.

This needs an Opus backing file. The `resolve_ogg_tests` module already has `build_opus_file`; replicate the minimal builder here, or move the C2-3 test into `resolve_ogg_tests` (which has `build_opus_file` and resolves Opus end-to-end). **Place this test in `mod resolve_ogg_tests`** to reuse `build_opus_file`.

- [ ] **Step 1: Inspect `resolve_ogg_tests::build_opus_file`** to confirm its signature/return (it writes an `.opus` and returns the path or bounds). Then write:

```rust
    #[test]
    fn build_cache_bytes_includes_ogg_index_estimate() {
        use musefs_db::{Format, NewTrack};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.opus");
        // build_opus_file writes a minimal valid opus stream; mirror how the
        // existing resolves_and_reads_opus_with_identical_audio test obtains bounds.
        let (audio_offset, audio_length) = build_opus_file(&path);
        let db = musefs_db::Db::open_in_memory().unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        let id = db
            .upsert_track(&NewTrack {
                backing_path: path.to_string_lossy().to_string(),
                format: Format::Opus,
                audio_offset,
                audio_length,
                backing_size: meta.len() as i64,
                backing_mtime: mtime_secs(&meta),
            })
            .unwrap();
        let cache = HeaderCache::new(Mode::Synthesis);
        let resolved = cache.resolve(&db, id).unwrap();
        let inline_sum: u64 = resolved
            .layout
            .segments()
            .iter()
            .map(|s| match s {
                Segment::Inline(b) => b.len() as u64,
                _ => 0,
            })
            .sum();
        // cache_bytes for an ogg codec == inline bytes + the ogg index estimate.
        assert_eq!(
            resolved.cache_bytes,
            inline_sum + estimated_ogg_index_bytes(audio_length as u64)
        );
        assert!(estimated_ogg_index_bytes(audio_length as u64) > 0);
    }
```

> **Verified:** `build_opus_file` returns `(i64, i64)` — `(audio_offset, audio_length)` — matching the usage above. No adaptation needed.

- [ ] **Step 2: Run → pass**

Run: `cargo test -p musefs-core build_cache_bytes_includes_ogg_index_estimate`
Expected: PASS.

- [ ] **Step 3: Hand-apply, confirm fail, revert**

- Delete the `Opus | Vorbis | OggFlac => estimated_ogg_index_bytes(...)` arm (so the match yields `0`): `cache_bytes` becomes `inline_sum + 0`, assertion FAILs. ✓
- `.sum::<u64>() + match` → `* match`: `inline_sum * estimated(...)` ≠ `inline_sum + estimated(...)`. FAILs. ✓ (This also reinforces the `+→*` kill from C2-2.)

- [ ] **Step 4: Commit C2**

```bash
git add musefs-core/src/reader.rs
git commit -m "test(reader): Phase 4a C2 — build audio-bounds guard, cache_bytes accounting, read_segments early-out

Records masked-equivalent serve-path range mutants with hand-apply evidence.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Component C3 — scan (`MAX_ART_BYTES`, `is_supported_audio`, `collect_audio`, `probe`+#9, `ingest`, `scan_directory`, `revalidate`)

C3 tests go in `musefs-core/src/scan.rs`. Add a new `#[cfg(test)] mod hardening_tests` (sibling to `ogg_probe_tests`/`wav_probe_tests`); finding-#9 probe-fallback cases extend `ogg_probe_tests`/`wav_probe_tests` and the new module. The new module needs a small FLAC builder (with vorbis comments and an optional PICTURE block) since `write_flac_local` lives in `reader.rs`.

### Task C3-1: `MAX_ART_BYTES` constant

**Files:**
- Test: `musefs-core/src/scan.rs` (new `mod hardening_tests`)
- Construct: `const MAX_ART_BYTES: usize = 16 * 1024 * 1024 - 64 * 1024;` (~line 12)

- [ ] **Step 1: Write the module skeleton + test**

```rust
#[cfg(test)]
mod hardening_tests {
    use super::*;

    #[test]
    fn max_art_bytes_is_16_mib_minus_64_kib() {
        // 16*1024*1024 - 64*1024 = 16_711_680. A literal where every *→+/*→/ diverges.
        assert_eq!(MAX_ART_BYTES, 16_711_680);
    }
}
```

- [ ] **Step 2: Run → pass**

Run: `cargo test -p musefs-core max_art_bytes_is_16_mib_minus_64_kib`
Expected: PASS.

- [ ] **Step 3: Hand-apply, confirm fail, revert**

- Each `*` in `16 * 1024 * 1024 - 64 * 1024` → `+` or `/`: value ≠ 16_711_680 → FAIL. ✓ (verify all three `*` sites.)

### Task C3-2: `is_supported_audio` + `collect_audio`

**Files:**
- Test: `musefs-core/src/scan.rs` (in `mod hardening_tests`)
- Constructs: `is_supported_audio` (~line 41) whole-fn `→true` and the chain of `||`; `collect_audio` (~line 52) `ftype.is_file() && is_supported_audio(&path)`.

- [ ] **Step 1: Write the tests**

```rust
    #[test]
    fn is_supported_audio_accepts_known_and_rejects_unknown() {
        for ok in ["a.flac", "a.mp3", "a.m4a", "a.m4b", "a.ogg", "a.oga", "a.opus", "a.wav"] {
            assert!(is_supported_audio(std::path::Path::new(ok)), "{ok} should be supported");
        }
        for bad in ["a.txt", "a.png", "a", "a.flacx"] {
            assert!(!is_supported_audio(std::path::Path::new(bad)), "{bad} must be rejected");
        }
    }

    #[test]
    fn collect_audio_skips_unsupported_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("keep.flac"), b"x").unwrap();
        std::fs::write(dir.path().join("skip.txt"), b"x").unwrap();
        let mut out = Vec::new();
        collect_audio(dir.path(), &mut out).unwrap();
        assert_eq!(out.len(), 1);
        assert!(out[0].ends_with("keep.flac"));
    }
```

- [ ] **Step 2: Run → pass**

Run: `cargo test -p musefs-core -- is_supported_audio_accepts_known_and_rejects_unknown collect_audio_skips_unsupported_files`
Expected: PASS.

- [ ] **Step 3: Hand-apply, confirm fail, revert**

- `is_supported_audio` body → `true`: the `bad` extensions now pass → `is_supported_audio_accepts_known_and_rejects_unknown` FAILs. ✓
- Each `has_ext(path, "x") ||` → `&&`: e.g. `has_ext(path,"flac") && has_ext(path,"mp3") && ...` — a `.flac` file then requires ALL exts → `is_supported_audio("a.flac")` becomes false → FAILs. ✓ (Confirm at least the first and one middle `||`; the `||`-chain is killed by the per-ext accept assertions.)
- `collect_audio`: `ftype.is_file() && is_supported_audio(&path)` → `||`: now any file OR any supported-name dir is collected; `skip.txt` is a file → `is_file() || …` true → collected → `out.len() == 1` FAILs. ✓

### Task C3-3: `probe` per-format fallbacks (finding #9)

**Files:**
- Test: extend `musefs-core/src/scan.rs` `mod ogg_probe_tests` and `mod wav_probe_tests`, plus `mod hardening_tests` for FLAC/MP3/M4A.
- Construct: `probe` (~line 76) the `has_ext(...) || has_ext(...)` ext groups (`m4a||m4b`, `ogg||oga||opus`) and the per-format `.ok()?`/`None` fallbacks.

Finding #9 is about probe's **error/fallback branches**: a file with a supported extension but unparseable contents must return `None` (skipped), and each ext alias must be recognized.

- [ ] **Step 1: Write the tests**

In `mod hardening_tests` (add a minimal FLAC builder first — see Task C3-4 Step 1 for `flac_file`; define it once and reuse):

```rust
    #[test]
    fn probe_returns_none_for_supported_ext_with_garbage_contents() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["bad.flac", "bad.mp3", "bad.m4a", "bad.wav", "bad.opus"] {
            let path = dir.path().join(name);
            std::fs::write(&path, b"not a real audio file").unwrap();
            assert!(probe(&path, b"not a real audio file").is_none(), "{name} must skip");
        }
    }

    #[test]
    fn probe_recognizes_m4b_alias() {
        // A valid m4a body under a .m4b name must probe as M4a (kills m4a||m4b → &&).
        let audio = b"AUDIODATA";
        let bytes = minimal_m4a_local(audio); // see Step 2 note
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("book.m4b");
        std::fs::write(&path, &bytes).unwrap();
        let probed = probe(&path, &bytes).expect("m4b should probe");
        assert_eq!(probed.format, Format::M4a);
    }
```

In `mod ogg_probe_tests`, add an `.oga` alias case mirroring `probe_detects_opus_and_seeds_tags` but writing the bytes under a `song.oga` path and asserting `probe(...).is_some()` with `Format::Opus` (kills `ogg||oga||opus → &&`).

> **Step 2 note:** `minimal_m4a_local` — the `tests/common::minimal_m4a` helper is not visible from `src`. Either (a) add the `.m4b` alias assertion to the existing integration test surface instead, or (b) port a minimal moov-first m4a builder into `hardening_tests`. Prefer (a): add a `reads_m4b_alias` test in `musefs-core/tests/facade.rs` using `common::minimal_m4a` (it is already imported there), scanning a `book.m4b` file and asserting the track ingests as M4a. This keeps the m4a builder DRY.

- [ ] **Step 2: Run → pass**

Run: `cargo test -p musefs-core -- probe_returns_none_for_supported_ext_with_garbage_contents`
plus the ogg `.oga` test and the facade `.m4b` test.
Expected: PASS.

- [ ] **Step 3: Hand-apply, confirm fail, revert**

- `has_ext(path, "m4a") || has_ext(path, "m4b")` → `&&`: the `.m4b` test (a `.m4b` file has `m4a==false && m4b==true` → false) → probe falls through to the next branch and ultimately `None`/wrong → FAILs. ✓
- `has_ext(path,"ogg") || has_ext(path,"oga") || has_ext(path,"opus")` → `&&`: the `.oga` test FAILs. ✓
- Per-format `.ok()?`/final `else { None }`: covered by `probe_returns_none_for_supported_ext_with_garbage_contents` (garbage `.flac`/`.mp3`/etc. must yield `None`). Hand-apply is format-specific; the locate-audio `?` short-circuits are what make garbage return `None`. Confirm at least the FLAC branch by forcing `flac::locate_audio(bytes).ok()?` to `.unwrap()` would panic — instead verify the test stays green and the construct is the `||`/fallback the inventory names.

### Task C3-4: `ingest` ordinal increment + art width/height guards

**Files:**
- Test: `musefs-core/src/scan.rs` (in `mod hardening_tests`)
- Constructs: `ingest` (~line 150) `*ord += 1;`; `(pic.width != 0).then_some(...)` and `(pic.height != 0).then_some(...)`.

- [ ] **Step 1: Add a FLAC builder + write the tests**

```rust
    // Minimal FLAC: fLaC + STREAMINFO + VORBIS_COMMENT(entries) + optional PICTURE + audio.
    fn flac_block(bt: u8, body: &[u8], last: bool) -> Vec<u8> {
        let mut v = vec![(if last { 0x80 } else { 0 }) | (bt & 0x7F)];
        let n = body.len();
        v.extend_from_slice(&[(n >> 16) as u8, (n >> 8) as u8, n as u8]);
        v.extend_from_slice(body);
        v
    }
    fn streaminfo() -> Vec<u8> {
        let mut si = vec![
            0x10, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0A, 0xC4, 0x42, 0xF0,
            0x00, 0x00, 0x00, 0x00,
        ];
        si.extend_from_slice(&[0u8; 16]);
        si
    }
    fn vorbis_comment(entries: &[&str]) -> Vec<u8> {
        let mut vc = Vec::new();
        let vendor = b"x";
        vc.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
        vc.extend_from_slice(vendor);
        vc.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        for e in entries {
            vc.extend_from_slice(&(e.len() as u32).to_le_bytes());
            vc.extend_from_slice(e.as_bytes());
        }
        vc
    }
    fn picture(width: u32, height: u32, data: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&3u32.to_be_bytes()); // type: front cover
        let mime = "image/png";
        b.extend_from_slice(&(mime.len() as u32).to_be_bytes());
        b.extend_from_slice(mime.as_bytes());
        b.extend_from_slice(&0u32.to_be_bytes()); // description len
        b.extend_from_slice(&width.to_be_bytes());
        b.extend_from_slice(&height.to_be_bytes());
        b.extend_from_slice(&0u32.to_be_bytes()); // color depth
        b.extend_from_slice(&0u32.to_be_bytes()); // colors used
        b.extend_from_slice(&(data.len() as u32).to_be_bytes());
        b.extend_from_slice(data);
        b
    }
    // Writes a FLAC and returns its path's bytes-on-disk via the dir.
    fn write_flac(path: &std::path::Path, entries: &[&str], pic: Option<(u32, u32)>) {
        let mut out = b"fLaC".to_vec();
        out.extend(flac_block(0, &streaminfo(), false));
        let last_is_vc = pic.is_none();
        out.extend(flac_block(4, &vorbis_comment(entries), last_is_vc));
        if let Some((w, h)) = pic {
            out.extend(flac_block(6, &picture(w, h, &[0xAB; 64]), true));
        }
        out.extend_from_slice(&[0xCD; 128]); // audio
        std::fs::write(path, &out).unwrap();
    }

    #[test]
    fn ingest_assigns_sequential_ordinals_per_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("multi.flac");
        write_flac(&path, &["ARTIST=A1", "ARTIST=A2"], None);
        let db = musefs_db::Db::open_in_memory().unwrap();
        crate::scan_directory(&db, &path).unwrap();
        let track = db.list_tracks().unwrap().into_iter().next().unwrap();
        let mut artists: Vec<(i64, String)> = db
            .get_tags(track.id)
            .unwrap()
            .into_iter()
            .filter(|t| t.key.eq_ignore_ascii_case("artist"))
            .map(|t| (t.ordinal, t.value))
            .collect();
        artists.sort();
        assert_eq!(artists, vec![(0, "A1".to_string()), (1, "A2".to_string())]);
    }

    #[test]
    fn ingest_stores_nonzero_art_dimensions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("art.flac");
        write_flac(&path, &["ARTIST=A", "TITLE=T"], Some((10, 20)));
        let db = musefs_db::Db::open_in_memory().unwrap();
        crate::scan_directory(&db, &path).unwrap();
        let track = db.list_tracks().unwrap().into_iter().next().unwrap();
        let ta = db.get_track_art(track.id).unwrap();
        assert_eq!(ta.len(), 1);
        let meta = db.get_art_meta(ta[0].art_id).unwrap().unwrap();
        assert_eq!(meta.width, Some(10));
        assert_eq!(meta.height, Some(20));
    }
```

> The tag key may be normalized to lowercase by `flac::read_vorbis_comments`/`mapping`; the `eq_ignore_ascii_case` filter and the assertion compare values/ordinals, not key case. If `probe` fails to parse the minimal FLAC (e.g. the FLAC parser requires blocks beyond STREAMINFO+VORBIS_COMMENT), that is a fixture issue — strengthen the builder, do not weaken the test. If `read_pictures` does not populate width/height from the PICTURE block, that is a real finding — stop and flag it (do not weaken the test).

- [ ] **Step 2: Run → pass**

Run: `cargo test -p musefs-core -- ingest_assigns_sequential_ordinals_per_key ingest_stores_nonzero_art_dimensions`
Expected: PASS.

- [ ] **Step 3: Hand-apply, confirm fail, revert**

- `*ord += 1;` → `-=`: second ARTIST gets ordinal `-1` → assertion `vec![(0,A1),(1,A2)]` FAILs (also a DB UNIQUE/ordering effect). ✓
- `*ord += 1;` → `/=`: `0 /= 1` stays 0 → both ordinals 0 → FAILs (duplicate ordinal). ✓
- `(pic.width != 0).then_some(...)` → `(pic.width == 0).then_some(...)`: width 10 → `10 == 0` false → `None` → `meta.width == Some(10)` FAILs. ✓
- `(pic.height != 0)` → `== 0`: height 20 → `None` → FAILs. ✓

### Task C3-5: `scan_directory` counters

**Files:**
- Test: `musefs-core/src/scan.rs` (in `mod hardening_tests`)
- Construct: `scan_directory` (~line 207) `stats.skipped += 1;` and `stats.scanned += 1;`

- [ ] **Step 1: Write the test**

```rust
    #[test]
    fn scan_directory_counts_scanned_and_skipped() {
        let dir = tempfile::tempdir().unwrap();
        write_flac(&dir.path().join("ok1.flac"), &["ARTIST=A", "TITLE=T1"], None);
        write_flac(&dir.path().join("ok2.flac"), &["ARTIST=A", "TITLE=T2"], None);
        // A .flac name with garbage body → parsed, fails, skipped.
        std::fs::write(dir.path().join("bad.flac"), b"garbage").unwrap();
        let db = musefs_db::Db::open_in_memory().unwrap();
        let stats = crate::scan_directory(&db, dir.path()).unwrap();
        assert_eq!(stats.scanned, 2);
        assert_eq!(stats.skipped, 1);
    }
```

- [ ] **Step 2: Run → pass**

Run: `cargo test -p musefs-core scan_directory_counts_scanned_and_skipped`
Expected: PASS.

- [ ] **Step 3: Hand-apply, confirm fail, revert**

- `stats.scanned += 1;` → `-=` (→ `-2`) or `*=`: `scanned == 2` FAILs. ✓
- `stats.skipped += 1;` → `-=`/`*=`: `skipped == 1` FAILs. ✓

### Task C3-6: `revalidate` — unchanged guard, counters, NotFound prune-guard

**Files:**
- Test: `musefs-core/src/scan.rs` (in `mod hardening_tests`)
- Construct: `revalidate` (~line 219): unchanged guard `existing.backing_size == meta.len() as i64 && existing.backing_mtime == mtime_secs(&meta)`; `stats.{updated,unchanged,pruned} += 1`; prune guard `Err(e) if e.kind() == std::io::ErrorKind::NotFound`.

- [ ] **Step 1: Write the tests**

```rust
    #[test]
    fn revalidate_buckets_unchanged_and_prunes_missing() {
        let dir = tempfile::tempdir().unwrap();
        let keep = dir.path().join("keep.flac");
        write_flac(&keep, &["ARTIST=A", "TITLE=T"], None);
        let db = musefs_db::Db::open_in_memory().unwrap();
        crate::scan_directory(&db, dir.path()).unwrap();

        // First revalidate: file unchanged since scan → unchanged bucket.
        let s1 = crate::revalidate(&db, dir.path()).unwrap();
        assert_eq!(s1.unchanged, 1);
        assert_eq!(s1.updated, 0);
        assert_eq!(s1.pruned, 0);

        // Delete the file, revalidate → pruned bucket.
        std::fs::remove_file(&keep).unwrap();
        let s2 = crate::revalidate(&db, dir.path()).unwrap();
        assert_eq!(s2.pruned, 1);
        assert!(db.list_tracks().unwrap().is_empty());
    }

    #[test]
    fn revalidate_does_not_prune_on_non_notfound_error() {
        // A track whose backing_path traverses a regular FILE as if it were a dir
        // makes fs::metadata return ENOTDIR (NOT NotFound) — the track must NOT be
        // pruned. Kills the prune match-guard `e.kind() == NotFound → true`.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("real.flac");
        write_flac(&file, &["ARTIST=A", "TITLE=T"], None);
        let db = musefs_db::Db::open_in_memory().unwrap();
        crate::scan_directory(&db, dir.path()).unwrap();

        // Point the track at "<dir>/real.flac/ghost.flac": real.flac is a file, so
        // stat'ing a path *under* it yields ENOTDIR. Keep it under canon(root) so
        // the prune scope check passes.
        use musefs_db::{Format, NewTrack};
        let track = db.list_tracks().unwrap().into_iter().next().unwrap();
        db.delete_track(track.id).unwrap();
        let canon = std::fs::canonicalize(dir.path()).unwrap();
        let ghost = canon.join("real.flac").join("ghost.flac");
        db.upsert_track(&NewTrack {
            backing_path: ghost.to_string_lossy().to_string(),
            format: Format::Flac,
            audio_offset: 0,
            audio_length: 0,
            backing_size: 0,
            backing_mtime: 0,
        })
        .unwrap();

        let stats = crate::revalidate(&db, dir.path()).unwrap();
        assert_eq!(stats.pruned, 0, "ENOTDIR is not NotFound → must not prune");
        assert_eq!(db.list_tracks().unwrap().len(), 1);
    }
```

> Note: the second test relies on `collect_audio` not enumerating `real.flac/ghost.flac` (it isn't a real path), so the updated/unchanged buckets are 0 and only the prune loop is exercised against the ghost track. If `std::fs::canonicalize(root)` or `starts_with` behaves unexpectedly on your platform, verify the ghost path is under `canon(root)` before asserting.

- [ ] **Step 2: Run → pass**

Run: `cargo test -p musefs-core -- revalidate_buckets_unchanged_and_prunes_missing revalidate_does_not_prune_on_non_notfound_error`
Expected: PASS.

- [ ] **Step 3: Hand-apply, confirm fail, revert**

- unchanged guard `... == ... && ... == ...` → `||`: a file matching size XOR mtime would re-ingest; here both match, `&&`→`||` keeps `unchanged` correct, so strengthen if needed. To kill `&&→||`, add a third file whose **mtime** matches but **size** differs (touch+rewrite): correct → `updated`, mutant (`||`) → `unchanged`. If the basic test does not move under `&&→||`, add that fixture. (Construct the size-differs/mtime-same case by writing the file, recording mtime, rewriting with different length and restoring mtime via `filetime` if available, else accept this sub-mutant may need the `||` to flip via a mtime-same/size-diff row.)
- `stats.unchanged += 1` / `pruned += 1` → `-=`/`*=`: the bucket assertions FAIL. ✓
- prune guard `e.kind() == std::io::ErrorKind::NotFound` → `true` (guard always matches): `revalidate_does_not_prune_on_non_notfound_error` FAILs (ENOTDIR now prunes → `pruned == 0` and `list_tracks().len() == 1` both FAIL). ✓

> **Verified:** `filetime` is NOT currently a workspace dependency. To kill the `&&→||` sub-mutant, either add `filetime` to `musefs-core`'s `[dev-dependencies]` (small, reversible, ~1 crate) or record the sub-mutant as `missed → needs fixture (filetime dev-dep)`. Decide with the reviewer at commit time; do not silently skip.

- [ ] **Step 4: Commit C3**

```bash
git add musefs-core/src/scan.rs musefs-core/tests/facade.rs
git commit -m "test(scan): Phase 4a C3 — probe fallbacks (#9), ingest accounting, scan/revalidate counters & prune guard

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Component C4 — tree `disambiguate`

C4 tests go in `musefs-core/src/tree.rs` `mod tests`, exercising the private `disambiguate` through the public `VirtualTree::build` (collisions on rendered paths). Names are read back via `lookup`.

### Task C4-1: dotfile/extension split boundary (2 missed mutants)

**Files:**
- Test: `musefs-core/src/tree.rs` (in `mod tests`)
- Construct: `disambiguate` (~line 182) `Some(i) if i > 0 => (&name[..i], Some(&name[i + 1..]))`.

- [ ] **Step 1: Write the test**

```rust
    #[test]
    fn disambiguate_keeps_dotfile_whole_and_splits_normal_ext() {
        let t = VirtualTree::build(&[
            (10, "D/.hidden".into()),
            (20, "D/.hidden".into()),
            (30, "D/a.ext".into()),
            (40, "D/a.ext".into()),
        ]);
        let d = t.lookup(VirtualTree::ROOT, "D").unwrap();
        // ".hidden": the dot is at index 0 → whole name is the stem, no ext split.
        assert!(t.lookup(d, ".hidden").is_some());
        assert!(t.lookup(d, ".hidden (2)").is_some());
        assert!(t.lookup(d, " (2).hidden").is_none(), "must not split at the index-0 dot");
        // "a.ext": dot at index 1 → split into stem "a" + ext "ext".
        assert!(t.lookup(d, "a.ext").is_some());
        assert!(t.lookup(d, "a (2).ext").is_some());
    }
```

- [ ] **Step 2: Run → pass**

Run: `cargo test -p musefs-core disambiguate_keeps_dotfile_whole_and_splits_normal_ext`
Expected: PASS.

- [ ] **Step 3: Hand-apply, confirm fail, revert**

- `Some(i) if i > 0` → `Some(i) if i >= 0`: for `.hidden`, `0 >= 0` matches → stem `""`, ext `"hidden"` → candidate `" (2).hidden"`. `.hidden (2)` is no longer produced → `t.lookup(d, ".hidden (2)").is_some()` FAILs (and `" (2).hidden".is_none()` FAILs). ✓
- match guard `Some(i) if i > 0` → `Some(i) if true`: same wrong split for the index-0 dot → FAILs. ✓

### Task C4-2: suffix-loop termination (2 timeout mutants — record, never hand-apply)

**Files:**
- Test: `musefs-core/src/tree.rs` (in `mod tests`)
- Construct: `disambiguate` loop (~line 194) `if !existing.contains_key(&candidate)` and (~line 197) `k += 1;`

- [ ] **Step 1: Write the covering test**

```rust
    #[test]
    fn disambiguate_resolves_three_way_collision() {
        // Three tracks render to the same path → the loop must reach (2) then (3).
        let t = VirtualTree::build(&[
            (10, "D/song.flac".into()),
            (20, "D/song.flac".into()),
            (30, "D/song.flac".into()),
        ]);
        let d = t.lookup(VirtualTree::ROOT, "D").unwrap();
        assert!(t.lookup(d, "song.flac").is_some());
        assert!(t.lookup(d, "song (2).flac").is_some());
        assert!(t.lookup(d, "song (3).flac").is_some());
    }
```

- [ ] **Step 2: Run → pass**

Run: `cargo test -p musefs-core disambiguate_resolves_three_way_collision`
Expected: PASS.

- [ ] **Step 3: Record as timeout-detected (DO NOT hand-apply)**

Both mutants make the suffix loop non-terminating, so hand-applying would hang the suite:
- `k += 1;` → `k *= 1;` pins `k` at 2 → never reaches `(3)` → infinite loop.
- `if !existing.contains_key(&candidate)` → `if existing.contains_key(&candidate)` returns only when a candidate *already* exists → loops forever on a fresh candidate.

cargo-mutants' per-mutant timeout kills both in CI. Confirm by reasoning + this covering test (which forces ≥2 iterations on correct code). Note for C6: annotate both inventory rows `timeout → timeout-detected`.

- [ ] **Step 4: Commit C4**

```bash
git add musefs-core/src/tree.rs
git commit -m "test(tree): Phase 4a C4 — disambiguate dotfile boundary kills + 3-way collision (timeout coverage)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Component C5 — facade glue (`refresh`, `getattr`, `read`, `open_handle`, `release_handle`, `poll_refresh_notify`)

C5 tests go in `musefs-core/tests/facade.rs`, reusing `config()`, `scanned_db(dir)`, and `common::{make_flac, streaminfo_body, vorbis_comment_body}`. The `read`/`release_handle` kills use the **append trick**: appending bytes to the backing file after `open_handle` changes the file's size/mtime so the *fallback* (`resolve`) path errors with `BackingChanged`, while the cached-handle fast path still serves the original ranges — making the two code paths observably different.

### Task C5-1: `refresh` whole-fn

**Files:**
- Test: `musefs-core/tests/facade.rs`
- Construct: `Musefs::refresh` (~line 196) whole-fn `→Ok(())`.

- [ ] **Step 1: Write the test** (on-disk DB so a second connection can add a track)

```rust
#[test]
fn refresh_picks_up_externally_added_track() {
    use musefs_db::{Format, NewTrack, Tag};
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("m.db");
    {
        let db = musefs_db::Db::open(&db_path).unwrap();
        let id = db
            .upsert_track(&NewTrack {
                backing_path: "/x/a.flac".into(),
                format: Format::Flac,
                audio_offset: 0,
                audio_length: 0,
                backing_size: 0,
                backing_mtime: 0,
            })
            .unwrap();
        db.replace_tags(id, &[Tag::new("artist", "Alice", 0), Tag::new("title", "A", 0)]).unwrap();
    }
    let fs = Musefs::open(musefs_db::Db::open(&db_path).unwrap(), config()).unwrap();
    assert!(fs.lookup(VirtualTree::ROOT, "Bob").is_none());
    {
        let db2 = musefs_db::Db::open(&db_path).unwrap();
        let id = db2
            .upsert_track(&NewTrack {
                backing_path: "/x/b.flac".into(),
                format: Format::Flac,
                audio_offset: 0,
                audio_length: 0,
                backing_size: 0,
                backing_mtime: 0,
            })
            .unwrap();
        db2.replace_tags(id, &[Tag::new("artist", "Bob", 0), Tag::new("title", "B", 0)]).unwrap();
    }
    fs.refresh().unwrap();
    assert!(fs.lookup(VirtualTree::ROOT, "Bob").is_some(), "refresh must rebuild the tree");
}
```

- [ ] **Step 2: Run → pass**

Run: `cargo test -p musefs-core --test facade refresh_picks_up_externally_added_track`
Expected: PASS.

- [ ] **Step 3: Hand-apply, confirm fail, revert**

- `refresh` body → `Ok(())`: rebuild skipped → `Bob` never appears → FAILs. ✓

### Task C5-2: `open_handle` whole-fn

**Files:**
- Test: `musefs-core/tests/facade.rs`
- Construct: `Musefs::open_handle` (~line 485) whole-fn `→Ok(1)`.

- [ ] **Step 1: Write the test**

```rust
#[test]
fn open_handle_returns_distinct_ids_and_rejects_dirs() {
    let dir = tempfile::tempdir().unwrap();
    let db = scanned_db(dir.path());
    let fs = Musefs::open(db, config()).unwrap();
    let artist = fs.lookup(VirtualTree::ROOT, "Alice").unwrap();
    let (_, file_inode, _) = fs.readdir(artist).unwrap().into_iter().next().unwrap();

    let fh1 = fs.open_handle(file_inode).unwrap();
    let fh2 = fs.open_handle(file_inode).unwrap();
    assert_ne!(fh1, fh2, "each open must yield a fresh handle id");
    assert!(fh1 != 0 && fh2 != 0);

    // Opening a directory inode must error, not return a bogus handle.
    assert!(matches!(fs.open_handle(artist), Err(CoreError::IsDir(_))));
}
```

- [ ] **Step 2: Run → pass**

Run: `cargo test -p musefs-core --test facade open_handle_returns_distinct_ids_and_rejects_dirs`
Expected: PASS.

- [ ] **Step 3: Hand-apply, confirm fail, revert**

- `open_handle` body → `Ok(1)`: `fh1 == fh2 == 1` → `assert_ne!` FAILs; also the dir case returns `Ok(1)` instead of `Err(IsDir)` → FAILs. ✓

### Task C5-3: `read` handle fast-path guard (`!=→==`)

**Files:**
- Test: `musefs-core/tests/facade.rs`
- Construct: `Musefs::read` (~line 457) `if fh != 0 { ... }`.

- [ ] **Step 1: Write the test** (append trick)

```rust
#[test]
fn read_uses_cached_handle_after_backing_grows() {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let db = scanned_db(dir.path()); // writes a.flac into dir
    let fs = Musefs::open(db, config()).unwrap();
    let artist = fs.lookup(VirtualTree::ROOT, "Alice").unwrap();
    let (_, file_inode, _) = fs.readdir(artist).unwrap().into_iter().next().unwrap();
    let size = fs.getattr(file_inode).unwrap().size;

    let fh = fs.open_handle(file_inode).unwrap();
    // Append bytes to the backing file: size/mtime now mismatch the DB row, so the
    // fallback (resolve) path would error with BackingChanged — but the cached
    // handle keeps serving the original byte ranges.
    {
        let mut f = std::fs::OpenOptions::new().append(true).open(dir.path().join("a.flac")).unwrap();
        f.write_all(&[0u8; 64]).unwrap();
    }
    let via_handle = fs.read(file_inode, fh, 0, size).unwrap();
    assert_eq!(via_handle.len() as u64, size);
}
```

- [ ] **Step 2: Run → pass**

Run: `cargo test -p musefs-core --test facade read_uses_cached_handle_after_backing_grows`
Expected: PASS.

- [ ] **Step 3: Hand-apply, confirm fail, revert**

- `if fh != 0` → `if fh == 0`: for `fh != 0` the fast path is skipped → fallback `resolve` sees the grown file → `Err(BackingChanged)` → `.unwrap()` panics → FAILs. ✓

### Task C5-4: `release_handle` whole-fn

**Files:**
- Test: `musefs-core/tests/facade.rs`
- Construct: `Musefs::release_handle` (~line 507) whole-fn `→()`.

- [ ] **Step 1: Write the test**

```rust
#[test]
fn release_handle_forces_fallback_on_next_read() {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let db = scanned_db(dir.path());
    let fs = Musefs::open(db, config()).unwrap();
    let artist = fs.lookup(VirtualTree::ROOT, "Alice").unwrap();
    let (_, file_inode, _) = fs.readdir(artist).unwrap().into_iter().next().unwrap();
    let size = fs.getattr(file_inode).unwrap().size;

    let fh = fs.open_handle(file_inode).unwrap();
    fs.release_handle(fh);
    // Grow the backing file: with the handle truly gone, read(fh) falls back to
    // resolve → BackingChanged. If release was a no-op, the stale handle would
    // still serve bytes successfully.
    {
        let mut f = std::fs::OpenOptions::new().append(true).open(dir.path().join("a.flac")).unwrap();
        f.write_all(&[0u8; 64]).unwrap();
    }
    assert!(matches!(fs.read(file_inode, fh, 0, size), Err(CoreError::BackingChanged(_))));
}
```

- [ ] **Step 2: Run → pass**

Run: `cargo test -p musefs-core --test facade release_handle_forces_fallback_on_next_read`
Expected: PASS.

- [ ] **Step 3: Hand-apply, confirm fail, revert**

- `release_handle` body → `()` (no-op): the handle survives → `read(fh)` uses the fast path → `Ok(bytes)` instead of `Err(BackingChanged)` → `assert!(matches!(...))` FAILs. ✓

### Task C5-5: `getattr` size-cache version check (`==→!=`)

**Files:**
- Test: `musefs-core/tests/facade.rs`
- Construct: `Musefs::getattr` (~line 410) `if e.content_version == track.content_version`.

This needs a content change that (a) bumps `content_version` and (b) changes the synthesized size, without invalidating the size cache (i.e. no `poll_refresh` between the two `getattr` calls). Use an on-disk DB scanned from a real FLAC, then retag via a second connection (adds an `album` tag → larger VORBIS_COMMENT → larger synthesized header).

- [ ] **Step 1: Write the test**

```rust
#[test]
fn getattr_reresolves_size_after_content_version_bump() {
    use common::{make_flac, streaminfo_body, vorbis_comment_body};
    use musefs_db::Tag;
    let dir = tempfile::tempdir().unwrap();
    let backing = dir.path().join("a.flac");
    let bytes = make_flac(
        &[
            (0, streaminfo_body()),
            (4, vorbis_comment_body("v", &["ARTIST=Alice", "TITLE=Song"])),
        ],
        &[0xAB; 64],
    );
    std::fs::write(&backing, &bytes).unwrap();
    let db_path = dir.path().join("m.db");
    {
        let db = musefs_db::Db::open(&db_path).unwrap();
        scan_directory(&db, dir.path()).unwrap();
    }
    let fs = Musefs::open(musefs_db::Db::open(&db_path).unwrap(), config()).unwrap();
    let alice = fs.lookup(VirtualTree::ROOT, "Alice").unwrap();
    let inode = fs.lookup(alice, "Song.flac").unwrap();
    let size_before = fs.getattr(inode).unwrap().size; // populates size cache @ v0

    // External retag: same artist/title (path/inode stable) + a large album tag so
    // the synthesized header — and thus total size — grows; content_version bumps.
    let track_id = musefs_db::Db::open(&db_path).unwrap().list_tracks().unwrap()[0].id;
    {
        let db2 = musefs_db::Db::open(&db_path).unwrap();
        db2.replace_tags(
            track_id,
            &[
                Tag::new("artist", "Alice", 0),
                Tag::new("title", "Song", 0),
                Tag::new("album", &"X".repeat(500), 0),
            ],
        )
        .unwrap();
    }
    // No poll_refresh: the size cache still holds (v0, size_before).
    let size_after = fs.getattr(inode).unwrap().size;
    assert!(size_after > size_before, "size must reflect the larger retagged header");
}
```

- [ ] **Step 2: Run → pass**

Run: `cargo test -p musefs-core --test facade getattr_reresolves_size_after_content_version_bump`
Expected: PASS.

- [ ] **Step 3: Hand-apply, confirm fail, revert**

- `if e.content_version == track.content_version` → `!=`: after the bump, the cached `v0` `!= v1` is true → returns the **stale** `size_before` → `size_after > size_before` FAILs. ✓

> **Verified:** FLAC `synthesize_layout` passes ALL tags (via `tags_to_inputs`) to `vorbiscomment::build`, and `album` maps to the `ALBUM` vorbis field in the tagmap. The 500-byte album tag WILL enlarge the synthesized VORBIS_COMMENT block. If `size_after` still does not exceed `size_before`, the size cache is not being invalidated as expected — debug the cache path, do not weaken the assertion.

### Task C5-6: `poll_refresh_notify` timing guards (flagged candidate-equivalents)

**Files:**
- Test: `musefs-core/tests/facade.rs` (existing `poll_refresh_debounces_within_interval`, `unchanged_refresh_poll_consumes_debounce_window`, `failed_refresh_retries_after_backoff_not_every_call` already cover the coarse behavior).
- Construct: `Musefs::poll_refresh_notify` (~line 259, ~line 274) `last_poll.elapsed() < poll_interval` and `last_failed.elapsed() < refresh_retry_backoff`.

- [ ] **Step 1: Confirm existing coverage passes**

Run: `cargo test -p musefs-core --test facade -- poll_refresh_debounces_within_interval unchanged_refresh_poll_consumes_debounce_window failed_refresh_retries_after_backoff_not_every_call`
Expected: PASS.

- [ ] **Step 2: Hand-apply `<→<=`, confirm SURVIVAL, record equivalent**

For each guard, change `<` → `<=`, run the three tests above, and confirm they **still pass** (the mutation only changes behavior at exact `elapsed() == Duration` equality, which wall-clock sleeps cannot hit; `poll_interval == 0` short-circuits via `is_zero()` and cannot probe the first guard). Revert.

Record both for C6: `missed → equivalent (timing boundary unreachable without injecting Instant::now())`. Do **not** add a flaky timing test.

- [ ] **Step 3: Commit C5**

```bash
git add musefs-core/tests/facade.rs
git commit -m "test(facade): Phase 4a C5 — refresh/getattr/read/open_handle/release_handle kills; poll_refresh_notify timing recorded equivalent

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Component C6 — docs, inventory annotations, finding #15

### Task C6-1: finding #15 (ESTALE) doc note

**Files:**
- Modify: `musefs-core/src/reader.rs` near the `read_exact_at` call / `BackingChanged` logic in `read_segments`/`resolve`.

- [ ] **Step 1: Add a short doc comment**

Add (adjust wording to fit the surrounding style) a comment near the positioned `f.read_exact_at(...)` in `read_segments` and/or the `BackingChanged` return in `resolve`:

```rust
// Finding #15 (ESTALE, untested by design): on an NFS-backed mount a stale file
// handle surfaces here as a raw io::Error from the positioned read (or as
// BackingChanged from the size/mtime re-validation) and is propagated verbatim
// through the FUSE layer. There is no test-framework support to inject NFS ESTALE,
// so this path is documented rather than covered.
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo build -p musefs-core`
Expected: success (comment-only change).

### Task C6-2: annotate the survivor inventory

**Files:**
- Modify: `docs/audits/2026-05-29-test-audit.md`

- [ ] **Step 1: Annotate each `musefs-core` row**

For every survivor addressed above, change its row marker:
- Killed mutants → `missed → killed (phase 4a)`.
- Masked/unreachable mutants (C2 read_at/read_segments range guards; C2-2 build sign guards; C5-6 poll_refresh_notify timing) → `missed → equivalent (phase 4a)` with the one-line rationale from this plan.
- The two `disambiguate` loop mutants → `timeout → timeout-detected (phase 4a)`.
- Finding #9 (`probe` fallback `||`) → covered by C3-3; mark resolved.
- Finding #15 → document-only, discharged by C6-1.

- [ ] **Step 2: Mark 4a complete** in the remediation tracking doc (the same audit doc's status section, or the tracking doc it references).

### Task C6-3: final verification gate

- [ ] **Step 1: Run the full gate**

```bash
cargo test --workspace
cargo test -p musefs-format --features fuzzing
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```
Expected: all green.

- [ ] **Step 2: Commit C6**

```bash
git add musefs-core/src/reader.rs docs/audits/2026-05-29-test-audit.md
git commit -m "docs: Phase 4a C6 — finding #15 ESTALE note, inventory annotations, 4a complete

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

## Self-review checklist (run before declaring the plan done)

- **Spec coverage:** C1 (Shard insert/retain, cache math) ✓; C2 (build guard + cache_bytes + read_segments) ✓; C3 (MAX_ART_BYTES, is_supported_audio, collect_audio, probe/#9, ingest, scan_directory, revalidate) ✓; C4 (disambiguate missed + timeout) ✓; C5 (refresh, getattr, read, open_handle, release_handle, poll_refresh_notify) ✓; C6 (finding #15, inventory, completion) ✓.
- **Known fixture risks flagged inline (not placeholders):** `build_opus_file` signature (C2-3 — **verified: returns `(i64, i64)`**); minimal FLAC parse + `read_pictures` width/height (C3-4); `&&→||` unchanged-guard needing `filetime` dev-dep (C3-6 — **verified: not currently a dependency**); album-tag size growth (C5-5 — **verified: FLAC synthesis includes all tags**). Each says "verify/flag", never "TODO".
- **Equivalence is the fallback, not the goal:** every recorded equivalent carries hand-apply evidence and a rationale.
- **No production logic changes** except the C6-1 comment; any off-by-one revealed by a kill is a scoped, flagged fix within the owning function — never the positioned backing reads.
