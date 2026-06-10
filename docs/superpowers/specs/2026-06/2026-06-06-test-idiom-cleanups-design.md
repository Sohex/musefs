# Test-suite idiom cleanups (issue #139)

**Date:** 2026-06-06
**Issue:** [#139](https://github.com/Sohex/musefs/issues/139)
**Status:** Approved

## Problem

Four test-suite idiom problems, all flagged in issue #139:

1. **Env-var coupled scan tests.** `musefs-core/src/scan.rs` reads
   `MUSEFS_SCAN_WINDOW` (`scan_window()`) and `MUSEFS_BATCH_BYTES`
   (`batch_bytes_cap()`) from the process environment. Tests configure the
   scanner by mutating process-global env under a shared `ENV_LOCK` mutex,
   which serializes them and risks cross-test leakage. Both env vars exist
   *only* so tests can exercise these paths — no CLI, doc, or CI consumer
   exists.
2. **Triplicated FLAC fixture helpers.** `flac_block`, `streaminfo_body`,
   `vorbis_comment_body`, and `make_flac` are duplicated between
   `musefs-format/tests/common/mod.rs` and `musefs-core/tests/common/mod.rs`,
   and `musefs_format::fuzz_check` carries a third private `flac_block` under
   its `fixtures::flac()` builder.
3. **Leaked bench TempDir.** The criterion fixture in
   `musefs-core/benches/read_throughput.rs` keeps its `TempDir` alive via
   `std::mem::forget`, leaking a directory handle per call.
4. **Order-fragile fuzz bounds.** `fuzz/fuzz_targets/b64.rs` computes
   `int_in_range` bounds with bare subtractions (`total - 1`,
   `total - out_off`) whose non-underflow depends on a guard and a prior
   range several statements away; a reorder would underflow silently.

## Design

### 1. Inject scan knobs via `ScanOptions`; delete the env vars

- `ScanOptions` gains `window: usize` and `batch_bytes: u64`. A manual
  `Default` impl supplies `WINDOW` (1<<20), `BATCH_BYTES` (64<<20), and the
  current `jobs` default.
- Delete `scan_window()` and `batch_bytes_cap()` (env read + zero-filter
  parsing). Threading chain: `scan_directory_with` / `revalidate_with` →
  `run_pipeline` (already takes `&ScanOptions`; reads `opts.batch_bytes`
  where it called `batch_bytes_cap()`) → `probe_file`, whose signature gains
  the `window` parameter (it is the sole `scan_window()` consumer).
- The `MUSEFS_SCAN_WINDOW` and `MUSEFS_BATCH_BYTES` env vars are removed
  entirely (decision: no fallback layer — they had no non-test consumers).
- No validation on the new fields: they are test-only injection points and
  the `Default` values are the only production path.

Call-site blast radius — adding required fields breaks **every**
`ScanOptions` struct literal workspace-wide, not just the env-mutating tests:

- **Production:** `musefs-cli/src/lib.rs` (`run_scan`) constructs a literal;
  it gains `..Default::default()` (behavior unchanged — defaults are what
  the env-free production path always got).
- Tests with literals: `pipeline_backpressure.rs`,
  `scan_counters.rs` (several), `bench_ingest.rs` (several), and the
  in-file `scan.rs` test `jobs1_and_jobs_n_produce_equivalent_state`. Both
  spellings occur: `ScanOptions { jobs: 4 }` → add `..Default::default()`,
  and field-shorthand `ScanOptions { jobs }` →
  `ScanOptions { jobs, ..Default::default() }`. `bench_ingest.rs` is in
  scope only for these mechanical edits (it must compile under
  `cargo test --workspace`).
- Tests that today combine the **no-options** API with env mutation switch
  APIs instead: `probe_equivalence.rs` and two `scan_counters.rs` tests call
  `scan_directory(&db, dir)` under `MUSEFS_SCAN_WINDOW`; they become
  `scan_directory_with(&db, dir, &ScanOptions { window: 64, ..Default::default() })`
  (likewise for `MUSEFS_BATCH_BYTES` → `batch_bytes`).

Test updates:

- `scan.rs` unit tests drop their `ENV_LOCK`. `scan_window_default_and_env`
  and `batch_bytes_cap_default_and_env` are replaced by one test asserting
  `ScanOptions::default()` field values **against decimal literals**
  (`1_048_576`, `67_108_864`), not against `WINDOW`/`BATCH_BYTES` — the
  in-diff mutation gate mutates const and `Default` initializers, and a
  `== WINDOW` assertion lets a `<<`→`>>` const mutant flow to both sides and
  survive.
- Env-lock reality check: only `scan_counters.rs` has a test-file `ENV_LOCK`
  (deleted); `probe_equivalence.rs` and `pipeline_backpressure.rs` call
  `set_var`/`remove_var` **unguarded** today — a latent cross-test race this
  change fixes as a side benefit.
- Stale references scrubbed: the lock-registry comment in
  `musefs-core/src/lock.rs` naming "scan.rs ENV_LOCK", plus comments
  mentioning the deleted env vars in `tests/common/corpus.rs`,
  `tests/metrics.rs`, and `tests/scan_counters.rs`.
- Out of scope: `common_corpus_smoke.rs` keeps its `ENV_LOCK` — those tests
  deliberately exercise `MUSEFS_BENCH_*` env *parsing*, which is a real
  user-facing bench-config surface, not test plumbing.

### 2. Consolidate FLAC fixtures into `fuzz_check::fixtures`

- Consolidate into `musefs_format::fuzz_check::fixtures`: its existing
  private `flac_block` / `streaminfo_body` / `vorbis_comment_body` are
  overwritten with the better-commented bodies (the musefs-format
  `tests/common` copy) and made `pub`; `make_flac` is **added** (it has no
  `fuzz_check` counterpart today — `fixtures::flac()` inlines its assembly).
- Rewrite `fixtures::flac()` on top of `make_flac` — three copies become one.
- Add a self-dev-dependency to musefs-format
  (`musefs-format = { path = ".", features = ["fuzzing"] }`) so the
  feature-gated `fuzz_check` module is visible to musefs-format's own
  `tests/` directory. musefs-core's dev-dep already enables the feature.
- Both `tests/common/mod.rs` files replace their local copies with
  `pub use musefs_format::fuzz_check::fixtures::{flac_block, streaminfo_body,
  vorbis_comment_body, make_flac};` — call sites in dependent test files do
  not change, and the `pub use` keeps in-module callers (`write_flac`,
  `write_oggflac_with_art` in musefs-core's `common/mod.rs`) resolving
  unchanged.
- Side effect (documented in CLAUDE.md): feature unification from the
  self-dev-dependency means plain `cargo test -p musefs-format` now runs the
  format-layer proptests; the explicit `--features fuzzing` flag is no longer
  required. Update CLAUDE.md's test-commands section accordingly.

Rejected alternatives: a cross-crate `#[path]` include (brittle, leaves the
`fuzz_check` copy untouched) and a new `musefs-test-fixtures` workspace crate
(a new workspace member for ~40 lines, doesn't absorb the existing
`fuzz_check` fixtures).

### 3. Bench fixture returns its `TempDir`

`fixture()` in `musefs-core/benches/read_throughput.rs` returns
`(Arc<Musefs>, Vec<u64>, TempDir)`; its two callers (`bench_sequential_read`,
the concurrent bench) bind the dir (`_dir`) so it lives for the bench's
scope. `std::mem::forget(dir)` and its justifying comment are removed. The
sibling `cold_fixture()` already returns its `TempDir` and is the model.

### 4. Locally-evident bounds in the `b64` fuzz target

Replace the bare subtractions with `checked_sub` + early return:

```rust
let Some(max_off) = total.checked_sub(1) else { return };
let out_off = match u.int_in_range(0..=max_off) { ... };
let Some(max_take) = total.checked_sub(out_off) else { return };
let take = match u.int_in_range(1..=max_take) { ... };
```

The standalone `total == 0` guard is dead code — `img` is guaranteed
non-empty, so `total = b64_len(img.len()) >= 4` always — and is dropped; the
first `checked_sub` covers the case anyway. `let full = encode_b64_slice(…)`
stays before the bounds block. Behavior is identical; underflow-safety no
longer depends on statement order.

## Verification

One branch, one commit per item. Gates:

- `cargo test --workspace`
- `cargo clippy --all-targets` (compiles the bench)
- `cargo fmt --all --check`
- In-diff mutation gate: `-j2`, output under `/tmp/mutants-out/in-diff`,
  default TMPDIR, `mutants.diff` sanity-checked non-empty first
- `cargo +nightly fuzz build b64` — the fuzz crate is outside the workspace,
  so workspace builds do not compile it
