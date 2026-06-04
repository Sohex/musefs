# Phase 4a — Core Hardening (Design)

**Source audit:** `docs/audits/2026-05-29-test-audit.md` (the `musefs-core`
mutation survivors from the phase-1 inventory + findings #9, #15)
**Created:** 2026-05-29
**Status:** design — awaiting plan

## Goal

Drive the **`musefs-core` mutation survivors** toward zero with additive tests:
**54 missed + 2 timeout** across `reader.rs` (30), `scan.rs` (15), `facade.rs`
(7), and `tree.rs` (2 missed + 2 timeout). Kill the killable ones, document
genuine equivalents, and record the 2 `disambiguate` infinite-loop survivors as
**timeout-detected**. Close finding **#9** (scan probe/fallback coverage) along
the way, and discharge the document-only finding **#15** (ESTALE / backing-read
`io::Error` propagation).

`ogg_index.rs` is **out of scope** — its 3 survivors are Ogg and were resolved in
Phase 2 (two recorded equivalent, one killed).

**All changes are additive tests; no production logic change is expected** (these
are coverage gaps, not bugs). The one contingency: if a bound/accounting survivor
in the LRU shard (`reader.rs::Shard`), a scan counter, or `disambiguate` turns out
to mark a real off-by-one, it gets a small scoped fix — flagged, not assumed
(mirrors 3a's #16 and 3c's `patch_chunk_offsets` contingency). The byte-identity
invariant is untouched either way: nothing here changes positioned backing reads.

This is the **core** slice of Phase 4 (Core & DB). Phase 4b (`musefs-db`:
lib/schema mutants, findings #10/#11/#12, and the test-only `Default for Db`
decision) is a separate spec → plan → PR cycle.

## Decomposition decisions (carried from brainstorming)

- **Phase 4 splits into 4a (core, this doc) and 4b (db).** Core alone is larger
  than 3a+3b+3c combined; splitting keeps PRs reviewable and matches the phase-3
  sub-phase rhythm.
- **`Default for Db`** (the 20 unviable db mutants) is a **4b** concern; not
  touched here.
- **Finding #15 (ESTALE)** is document-only and concerns the *core* backing-read
  path (`reader.rs` `BackingChanged`, the positioned `read_exact_at` calls), so it
  is discharged **here in 4a**, not in 4b.

## Guiding principle: verify, don't trust

The 2026-05-29 audit was executed by a weaker model; its findings are **leads, not
facts.** Every survivor below is re-read against the actual source on the 4a base
before any kill is written. Consequences carried from 3a–3c:

1. **Line numbers in the inventory are approximate** (captured at CI sha
   `81d6d845d`). **Locate every target by its code construct, never by the raw
   line number.** Re-confirm before each kill.
2. The `||→&&` / `&&→||` / `<→<=` / `>→>=` survivors are predominantly **boolean
   short-circuit and boundary gaps** in control flow that the current tests pass
   through but never pin at the edge — most are killed by one carefully-placed
   boundary fixture each, and several share a base fixture mutated minimally.

## The hand-apply verification method (use for every kill)

cargo-mutants is not available locally. For each targeted
`function: construct: mutation`:

1. Run the new test → it passes (production code is correct).
2. Locate the construct by pattern, apply the exact mutation, rerun **just that
   test** → it must **fail** (a failed assertion *or* a panic both count).
3. Revert (`git checkout -- <file>`), rerun → passes again.

If step 2 still passes, strengthen the test, or — if the mutation provably yields
identical behavior — record it as an **equivalent mutant** instead of contriving a
test. Never leave a mutation applied.

**Timeout survivors are the exception** (see the timeout section): the mutation
makes a loop non-terminating, so it would hang rather than fail. They are
confirmed by reasoning + a covering test and recorded as caught-by-timeout.

## Test placement

All four core files **already have in-module `#[cfg(test)] mod tests`** (reader:
`cache_bound_tests`, `ogg_serve_tests`, `resolve_ogg_tests`, `ogg_art_serve_tests`;
scan: `ogg_probe_tests`, `wav_probe_tests`; facade: `tests`; tree: `tests`). 4a
**extends those modules** so each kill can call the private survivor fn directly
(`Shard::insert`/`retain_keys`, `HeaderCache::with_budget`/`shard`/`retain`/`build`,
`is_supported_audio`/`collect_audio`/`probe`/`ingest`/`revalidate`,
`VirtualTree::disambiguate`) with byte-precise fixtures and private-field
assertions (`Shard::bytes`/`budget`).

**Facade is the exception — its kills go in the integration file, not the
in-module module.** The in-module `facade.rs` `tests` module has a *single* unit
test (`validate_opened_backing_rejects_mismatched_descriptor_metadata`) that builds
a tempdir + a hand-constructed `ResolvedFile` to exercise the free fn
`validate_opened_backing`; it does **not** construct a `Db` or a `Musefs`, so there
is no in-module `Musefs` fixture to extend. The reusable `Musefs` harness lives in
`musefs-core/tests/facade.rs` (`common`, `config()`, `scanned_db()`), which
**already has behavioral tests for nearly every C5 survivor**
(`refresh_rebuilds_tree_after_new_tracks`, `open_handle_read_and_release_roundtrip`,
`poll_refresh_debounces_within_interval`,
`poll_refresh_notify_reports_changed_track_inode`, …). C5 therefore **strengthens
those integration tests** to pin the survivor boundaries, rather than adding
in-module tests against private fields. The other four files' kills stay in-module,
beside the construct, so their kill→test mapping is unambiguous.

## Verified findings (survivors cluster by file → function)

Line numbers approximate — locate by construct.

### `reader.rs` (30)

| Function | Constructs with survivors | Kill approach |
|----------|---------------------------|---------------|
| `Shard::insert` | re-insert byte accounting `bytes -= old_bytes` (`-=→+=`/`/=`); evict guard `bytes > budget && map.len() > 1` (`>→>=`, `&&→\|\|`); evict subtract `bytes -= n.value.cache_bytes` (`-=→+=`/`/=`) | re-insert same key with a *different* `cache_bytes` and assert `Shard::bytes`; drive total over `budget` to force eviction and assert which key/bytes survive; budget-equal boundary pins `>→>=` |
| `Shard::retain_keys` | whole-fn (`→()`), `filter(\|k\| !live.contains(k))` (`delete !`), `bytes -= n.value.cache_bytes` (`-=→+=`/`/=`) | retain with a `live` set that drops some keys; assert dropped keys gone, kept keys present, and `bytes` decremented by exactly the dropped sizes |
| `DEFAULT_CACHE_BUDGET` | `64 * 1024 * 1024` (`* → +`/`/`, ×4) | assert the const == 64 MiB directly (a value where `*`/`+`/`/` all diverge) |
| `HeaderCache::with_budget` | `budget / CACHE_SHARDS` per-shard (`/→%`/`*`) | construct with a known budget; assert per-shard budget == `budget / CACHE_SHARDS` (a value where `/`, `%`, `*` all diverge) |
| `HeaderCache::shard` | `track_id % CACHE_SHARDS` index (`%→/`) | two track ids that map to the same shard under `%` but different under `/` (or vice-versa); assert routing |
| `HeaderCache::retain` | whole-fn (`→()`) | resolve some tracks into the cache, `retain` a subset, assert the dropped ones miss and the kept ones hit |
| `HeaderCache::build` | **synthesis audio-bounds guard** `audio_offset < 0 \|\| audio_length < 0 \|\| (audio_offset+audio_length) > meta.len()` — `< → <=`/`==` on the two sign checks, `> → >=` on the size check, `\|\| → &&` (×2); plus the `cache_bytes` fold: `Inline(b)` arm delete, `.sum::<u64>() + match{…}` (`+ → *`), `Opus\|Vorbis\|OggFlac` arm delete. (No "segment-length" guard exists here — these are the row-vs-file bound checks.) | drive each bound to its exact edge (`audio_offset == 0`, `audio_length == 0`, `audio_offset+audio_length == meta.len()`) to pin the sign/size operators and the two `\|\|`; an `Inline` segment of known length pins `cache_bytes`/`+→*`; an Ogg-codec track exercises the codec arm |
| `read_at` / `read_segments` | early-out `offset >= total_len \|\| size == 0` in both fns (`\|\| → &&`, and `>= → >` at `offset == total_len` — note the operator is `>=`, not `<`); `read_segments` per-segment overlap test `ov_start < ov_end` (`< → <=`) | a read at exactly `offset == total_len` and one with `size == 0` pin the early-out `\|\|`/`>=`; a read landing exactly on a segment boundary (`ov_start == ov_end`) pins `ov_start < ov_end` |

### `scan.rs` (15) + finding #9

| Function | Constructs with survivors | Kill approach |
|----------|---------------------------|---------------|
| `MAX_ART_BYTES` (`scan.rs`, `16 * 1024 * 1024 - 64 * 1024`) | `* → +` (3 `*` sites) | assert `MAX_ART_BYTES == 16 * 1024 * 1024 - 64 * 1024` directly (a value where every `*→+`/`*→/` diverges) |
| `is_supported_audio` | whole-fn (`→true`), ext `\|\| → &&` (×2) | each supported ext recognized **and** an unsupported ext (e.g. `.txt`) rejected — kills `→true`; one-supported-one-not fixture kills each `\|\|→&&` |
| `collect_audio` | `&& → \|\|` | a dir tree where one predicate side is false; assert the file is/ isn't collected |
| `probe` (#9) | `\|\| → &&` | **finding #9**: truncated header, invalid magic, and per-format fallback inputs that exercise probe's error/fallback branches (extend `ogg_probe_tests`/`wav_probe_tests` with flac/mp3/mp4 + malformed cases) |
| `ingest` | `bytes_in += ... ` (`+=→-=`), tag/art `!= → ==` (×2) | ingest a fixture and assert the stat counters and the `!=` branch outcomes |
| `scan_directory` | counter `+= ` (`+=→-=`/`*=`) | scan a multi-file dir; assert the exact processed/added counts |
| `revalidate` | unchanged guard `backing_size == … && backing_mtime == …` (`&& → \|\|`), counters `+=` (`+=→-=`/`*=`), prune match-guard `e.kind() == NotFound` (`→true`) | revalidate over changed/unchanged/missing files; assert per-bucket counts. For the guard `→true`: insert a track whose `backing_path` traverses a **regular file** (e.g. `…/file.flac/ghost`) so `fs::metadata` returns `ENOTDIR` (non-`NotFound`) deterministically, then assert the track is **not** pruned (the `→true` mutant would prune it). A chmod-0 parent dir (`EACCES`) is the portable alternative |

### `tree.rs` (2 missed + 2 timeout)

`VirtualTree::disambiguate` — the only survivor site.

- **Missed** `:185` stem/ext split `rfind('.')` guard `i > 0` (`> → >=`, match
  guard `→ true`): a dotfile-style name like `.hidden` (dot at index 0) must keep
  its whole name as the stem (no spurious empty stem). A fixture colliding on
  `.hidden` and on `a.ext` pins both the `>` boundary and the guard.
- **Timeout** `:194` `if !existing.contains_key(&candidate)` (`delete !`) and
  `:197` `k += 1` (`+= → *=`): both make the suffix loop non-terminating
  (`*=` pins `k` at 2; deleting `!` returns only when a candidate *already*
  exists, looping forever otherwise). **Timeout-detected** — confirmed by a test
  that forces ≥2 collisions on one name (so the loop must reach ` (2)` then
  ` (3)`), which a correct impl satisfies and the `*=`/`delete !` mutants hang on.
  Recorded, never hand-applied locally.

### `facade.rs` (7)

| Function | Construct | Kill approach |
|----------|-----------|---------------|
| `Musefs::refresh` | whole-fn (`→Ok(())`) | mutate the DB out-of-band, call `refresh`, assert the virtual tree actually rebuilt (a node that should appear/vanish) |
| `Musefs::poll_refresh_notify` | two timing guards `last_poll.elapsed() < poll_interval` and `last_failed.elapsed() < refresh_retry_backoff` (`< → <=`) | drive debounced-vs-fires behavior via `last_poll`/`last_failed_refresh` + `poll_interval` (existing `tests/facade.rs::poll_refresh_debounces_within_interval` is the base). **See the equivalent-mutant flag below** — the exact `<→<=` edge may be unreachable without injecting `Instant::now()`, and `poll_interval == 0` short-circuits via `is_zero()` |
| `Musefs::getattr` | `== → !=` | getattr on an inode that exists vs one that doesn't; assert attr vs ENOENT |
| `Musefs::read` | `!= → ==` | a read whose guard distinguishes hit/miss; assert served bytes vs error |
| `Musefs::open_handle` | whole-fn (`→Ok(1)`) | open two handles; assert distinct/sequential ids (a constant `1` collides) |
| `Musefs::release_handle` | whole-fn (`→()`) | open then release then read via the released handle; assert the handle is gone |

## Equivalent mutants

**One flagged candidate, none otherwise assumed up front.** Unlike the format
layer (3a/3b's disjoint-bitfield `|→^`), the core survivors are
control-flow/accounting and are expected to be killable. Any mutant that proves
genuinely equivalent during implementation is recorded **then**, with hand-apply
evidence (the inventory row gets `missed → **equivalent**` with a one-line
rationale).

**Flagged candidate — `poll_refresh_notify`'s two timing `< → <=` guards.** Both
compare a monotonic `Instant::elapsed()` against a `Duration`; `<` and `<=` diverge
only when `elapsed()` equals the bound *exactly*, which a wall-clock test cannot
hit reliably, and the first guard is gated behind `is_zero()` (so `poll_interval ==
0` skips it rather than probing it). The plan should attempt a coarse
debounced-vs-fires kill first; if the equality edge is unreachable without
injecting a clock, record both as **equivalent** with that rationale rather than
contriving a flaky timing test.

## Timeout survivors → timeout-detected

The two `disambiguate` infinite-loop mutants (`:194 delete !`, `:197 += → *=`)
are recorded as **timeout-detected** per the Phase 2/3 convention (cargo-mutants'
per-mutant timeout kills a non-terminating mutant in CI). No production change;
the plan's verification step is "confirm a covering test forces ≥2 suffix
iterations," never an apply-and-rerun (which would hang the suite).

## Finding #15 — ESTALE (document-only)

No test framework support for NFS ESTALE injection. Discharge by **documenting the
known gap** where the backing-read path lives: a short note in `reader.rs` near
the positioned `read_exact_at` / `BackingChanged` logic (and a line in the
remediation tracking doc) stating that a stale backing handle surfaces as a raw
`io::Error` propagated through the FUSE layer, untested by design. No code change.

## Components

- **C1 — reader LRU cache** (`Shard::insert`/`retain_keys`,
  `HeaderCache::with_budget`/`shard`/`retain`): extend `cache_bound_tests` with
  byte-accounting, eviction-boundary, shard-routing, and retain assertions over
  private fields. ~8–10 survivors.
- **C2 — reader layout/build & serve** (`HeaderCache::build`, `read_at`,
  `read_segments`): segment-length/`cache_bytes`, the `Inline` and Ogg-codec match
  arms, and range-boundary fixtures. ~10–12 survivors.
- **C3 — scan** (`is_supported_audio`, `collect_audio`, `probe`+#9, `ingest`,
  `scan_directory`, `revalidate`): supported/unsupported ext, malformed-probe
  fallbacks, counter and `!=`/`&&` branch assertions over a tempdir fixture tree.
  ~15 survivors + finding #9.
- **C4 — tree disambiguate** (`VirtualTree::disambiguate`): dotfile/collision
  boundary tests (kill the 2 missed) + a ≥2-collision test covering the 2 timeouts;
  record both timeouts.
- **C5 — facade glue** (`refresh`, `poll_refresh_notify`, `getattr`, `read`,
  `open_handle`, `release_handle`): behavioral tests over a real `Musefs` built from
  the `tests/facade.rs` harness (`config()`/`scanned_db()`), strengthening the
  existing integration tests there to pin survivor boundaries — **not** the
  in-module `tests` module (which only covers `validate_opened_backing`). ~7
  survivors; `poll_refresh_notify`'s two timing `<→<=` guards are the flagged
  candidate-equivalents.
- **C6 — docs**: finding #15 note (reader.rs + tracking doc); annotate the
  `musefs-core` inventory rows (`missed → **killed** (phase 4a)` /
  `timeout → **timeout-detected**`); mark 4a complete in the tracking doc.

## Test budget (for chunking the plan)

Boundary/accounting tests each kill multiple mutants (e.g. one re-insert test pins
all three `Shard::insert` byte-accounting mutants; one eviction test pins the
guard `>→>=` and `&&→||` together). Rough counts:

- C1 reader cache: ~6–8 tests.
- C2 reader build/serve: ~8–10 tests.
- C3 scan + #9: ~10–14 tests (the densest; finding #9 adds per-format malformed
  fixtures — the plan should split probe-fallbacks from the counter/branch kills).
- C4 tree: ~3 tests (+ record 2 timeouts).
- C5 facade: ~6–7 tests.

Total ≈ 35–45 new/strengthened tests, plus 2 timeout records and 1 doc note.

## Implementation ordering

C1 → C2 → C3 → C4 → C5 → C6. C1–C5 are independent; C3 is the largest (do it after
the smaller reader components warm up the fixture idioms). C6 closes the loop.

## Error handling

No new error paths. Tests assert existing mappings: `CoreError::{TrackNotFound,
BackingChanged, …}`, scan's skip/prune contracts, and the FUSE-facing
attr/read/ENOENT behavior. If a bound/accounting survivor reveals a real
off-by-one, the scoped fix stays within the owning function (never the positioned
backing reads).

## Acceptance

| Component | Check |
|-----------|-------|
| C1 | re-insert byte accounting red under `-=→+=`/`/=`; eviction red under `>→>=`/`&&→\|\|`; `retain_keys` red under `→()`/`delete !`/`-=`; `with_budget`/`shard` math red under `/→%/*` / `%→/` |
| C2 | `build` `Inline`/Ogg-codec arm deletes and size guards red; `read_at`/`read_segments` range boundaries red |
| C3 | `is_supported_audio` red under `→true`/`\|\|→&&`; probe-fallback (#9) tests cover truncated/invalid/per-format inputs; `ingest`/`scan_directory`/`revalidate` counters + `!=`/`&&` + NotFound-guard red |
| C4 | dotfile/collision boundary tests red under `>→>=` + guard; ≥2-collision test covers both timeouts; recorded |
| C5 | `refresh`/`open_handle`/`release_handle` whole-fn mutants red; `poll_refresh_notify` boundary, `getattr` `==`, `read` `!=` red |
| C6 | finding #15 documented; inventory rows annotated; 4a marked complete |
| Whole | `cargo test --workspace` + `--features fuzzing` + `clippy --all-targets -D warnings` + `fmt --check` green; next full mutants campaign shows `musefs-core` survivors dropped (excluding the 2 documented timeouts) |
