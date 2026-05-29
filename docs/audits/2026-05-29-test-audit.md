# musefs Test Suite Audit — 2026-05-29

**Status:** in progress
**Spec:** docs/superpowers/specs/2026-05-29-test-audit-design.md
**Deliverable type:** _full audit_ | _red-test halt report_ (set at the Phase A gate)

## 1. Executive summary

_pending — filled last._

## 2. Environment & tooling (Phase 0)

| Tool | Present? | Version | Notes |
|------|----------|---------|-------|
| cargo-llvm-cov | yes | 0.8.7 | |
| cargo-fuzz | yes | 0.13.1 | |
| cargo-mutants | yes | 27.0.0 | installed during this phase (`cargo install --locked`) |
| beets venv (pytest/pytest-cov/mutagen) | yes | beets 2.11.0 / pytest 9.0.3 / pytest-cov 7.1.0 / mutagen 1.47.0 | built at `/tmp/musefs-audit/venv`; see CI-divergence note below |

**CI-divergence note:** The beets venv here uses `pip install -r contrib/beets/requirements.txt pytest-cov mutagen==1.47.0` rather than the CI path `pip install -e "contrib/beets[test]"`. This is intentional — it gives explicit version pinning for pytest-cov and mutagen. The venv is not a byte-for-byte CI reproduction.

Blocked surfaces (network/tooling): _none._

## 3. Test inventory & counts (Phase A)

Counting methodology: per-category, unique tests only (never summed raw invocation totals).

| Category | Command | Count | Pass/Fail/Skip |
|----------|---------|-------|----------------|
| (a) workspace (incl. fuzzing proptests) | `cargo test --workspace` | 259 | 259/0/0 (10 ignored, not run) |
| (b) FUSE e2e (`--ignored`) | `cargo test -p musefs-fuse -- --ignored` | 9 | 9/0/0 |
| (b) FUSE concurrency (`--features metrics`) | `cargo test -p musefs-fuse --features metrics -- --ignored --test-threads=1` | 1 | 1/0/0 (unique: `slow_read_does_not_block_stat` in `concurrency.rs`) |
| (b) core metrics (`--features metrics`) | `cargo test -p musefs-core --features metrics --test metrics -- --test-threads=1` | 4 | BLOCKED — compile error: `NewTrack` has no field `backing_mtime_secs` (`musefs-core/tests/metrics.rs:177`); all 4 tests blocked |
| (b) interop emitter + mutagen | `cargo test --test interop_emit -- --ignored` + `pytest test_mutagen_roundtrip.py` | 3 | 3/0/0 (1 Rust emitter + 2 Python) |
| (c) beets pytest (default) | `pytest --cov=beetsplug` | 52 | 52/0/0 (14 deselected, non-matching markers) |
| (c) beets pytest (musefs_bin) | `pytest -m musefs_bin --cov=beetsplug` | 8 | 8/0/0 (58 deselected) |
| (c) beets pytest (e2e) | `pytest -m e2e --cov=beetsplug` | 6 | 6/0/0 (60 deselected) |
| (d) cargo-fuzz targets | `cargo +nightly fuzz build` + smoke 15s each | 8 | 8/0/0 (0 crashes, all targets smoke-clean) |

## 4. Tier-1 test set & flakiness (Phase A)

Tier-1 tests are those on the byte-identical synthesis/splice critical path — any failure here means served bytes differ from originals. Grouped into three categories for the 3× flakiness runs.

### 4.1 Byte-identical / read path

#### musefs-core

| # | Test |
|---|------|
| 1 | `read_at::read_at_preserves_backing_audio` |
| 2 | `reader::random_offset_and_size_match_the_whole_read` |
| 3 | `reader::read_at_streams_art_image_segments` |
| 4 | `reader::reading_past_eof_returns_empty` |
| 5 | `reader::reading_whole_file_matches_total_len_and_splices_audio` |
| 6 | `facade::reads_a_synthesized_m4a_through_the_facade` |
| 7 | `facade::reads_a_synthesized_mp3_through_the_facade` |
| 8 | `facade::serves_flac_with_embedded_art_through_the_facade` |
| 9 | `facade::serves_mp3_with_embedded_art_through_the_facade` |
| 10 | `facade::lookup_getattr_readdir_and_read_through_the_facade` |
| 11 | `facade::open_handle_read_and_release_roundtrip` |
| 12 | `facade::readdir_distinguishes_a_file_from_an_unknown_inode` |
| 13 | `facade::parent_exposes_the_tree_hierarchy` |

#### musefs-format

| # | Test | Rationale |
|---|------|-----------|
| 1 | `layout::empty_backing_segment_passes_validation` | Validates layout segment invariants used by synthesis |
| 2 | `layout::empty_single_segment_layout_fails_validation` | Same — boundary guard |
| 3 | `layout::lengths_sum_segments_and_exclude_audio_from_header` | Core length accounting for byte-identical output |
| 4 | `layout::segment_len_reports_each_variant` | Segment length correctness |
| 5 | `layout::total_overflow_detected` | Overflow guard on total_len |
| 6 | `layout::valid_layout_passes_validation` | Happy-path layout validation |
| 7 | `roundtrip::full_roundtrip_preserved_blocks_multivalue_tags_and_two_pictures` | End-to-end FLAC roundtrip asserting byte identity |
| 8 | `synthesize_tags::measured_lengths_match_assembled_bytes` | Generate-and-measure invariant |
| 9 | `synthesize_tags::metaflac_reads_synthesized_vorbis_comments_and_preserves_streaminfo` | Independent oracle validates synthesized FLAC metadata |
| 10 | `synthesize_tags::vorbis_comment_block_is_the_last_metadata_block_when_no_art` | Structural correctness of synthesized FLAC |
| 11 | `synthesize_art::art_becomes_an_artimage_segment_and_lengths_are_exact` | Art segment synthesis |
| 12 | `synthesize_art::metaflac_reads_synthesized_picture` | Independent oracle validates synthesized picture |
| 13 | `synthesize_art::synthesize_errors_on_oversized_picture` | Error-path guard |
| 14 | `mp3_synthesize::synthesizes_id3v24_text_frames_and_preserves_audio` | MP3 synthesis with id3 oracle — byte-identical audio tail |
| 15 | `mp3_synthesize::synthesizes_apic_with_streamed_image_bytes` | MP3 art synthesis |
| 16 | `mp3_synthesize::embedded_size_field_matches_the_frame_region` | Syncsafe size correctness for byte-identical output |
| 17 | `mp3_synthesize::empty_tag_when_no_tags_or_art` | Empty-tag synthesis boundary |
| 18 | `mp3_synthesize::unknown_key_becomes_txxx` | TXXX frame synthesis |
| 19 | `mp3_synthesize::multi_value_text_frame_round_trips` | Multi-value TPE1 synthesis |
| 20 | `mp3_synthesize::multiple_art_frames_keep_order` | Art ordering in synthesized output |
| 21 | `mp3_synthesize::synthesize_errors_on_oversized_frame` | Error-path guard |
| 22 | `mp3_synthesize::synthesize_errors_when_frames_sum_past_the_tag_limit` | Error-path guard |
| 23 | `wav_synthesize::synthesizes_valid_riff_and_preserves_audio` | WAV synthesis with hound oracle — byte-identical PCM |
| 24 | `wav_synthesize::embeds_full_fidelity_id3_tag_with_art` | WAV ID3 tag + art synthesis |
| 25 | `wav_synthesize::emits_native_info_chunk_for_mapped_tags` | WAV LIST/INFO synthesis |
| 26 | `wav_synthesize::pads_odd_data_payload_to_word_boundary` | WAV word-alignment correctness |
| 27 | `wav_synthesize::rejects_audio_over_32bit` | Error-path guard |
| 28 | `mp4_oracle::synthesized_m4a_decodes_via_independent_parser` | MP4 synthesis with mp4-crate oracle — byte-identical sample offsets |
| 29 | `mp4_oracle::m4a_synthesis_uses_only_first_cover_art` | MP4 art-input dedup |
| 30 | `proptest_flac::flac_synthesis_preserves_audio` | Property test: FLAC audio byte identity |
| 31 | `proptest_flac::flac_tag_roundtrip_is_stable` | Property test: FLAC tag stability |
| 32 | `proptest_mp3::mp3_synthesis_preserves_audio` | Property test: MP3 audio byte identity |
| 33 | `proptest_mp4::mp4_synthesis_preserves_audio` | Property test: MP4 audio byte identity |
| 34 | `proptest_ogg::ogg_synthesis_preserves_audio` | Property test: OGG audio byte identity |
| 35 | `proptest_wav::wav_synthesis_preserves_audio` | Property test: WAV audio byte identity |

### 4.2 Resolution / freshness

#### musefs-core

| # | Test |
|---|------|
| 1 | `external_contract::resolve_builds_layout_and_total_len` |
| 2 | `external_contract::resolve_caches_until_content_version_changes` |
| 3 | `external_contract::resolve_errors_when_audio_bounds_overrun_the_file` |
| 4 | `external_contract::resolve_errors_when_backing_file_changes` |
| 5 | `external_contract::resolve_includes_art_image_segments` |
| 6 | `external_contract::structure_only_resolves_to_whole_backing_file` |
| 7 | `tree::root_node_is_a_directory` |
| 8 | `tree::builds_directories_and_files_with_lookup` |
| 9 | `tree::parent_of_root_is_root_and_children_point_back` |
| 10 | `tree::disambiguates_colliding_file_names` |
| 11 | `facade::inode_is_stable_across_refresh` |
| 12 | `facade::failed_refresh_retries_after_backoff_not_every_call` |
| 13 | `facade::poll_refresh_debounces_within_interval` |
| 14 | `facade::poll_refresh_keeps_unchanged_entries_and_prunes_vanished` |
| 15 | `facade::poll_refresh_notify_invalidates_old_inode_for_removed_track` |
| 16 | `facade::poll_refresh_notify_reports_changed_track_inode` |
| 17 | `facade::poll_refresh_notify_reports_old_inode_for_path_changing_retag` |
| 18 | `facade::poll_refresh_picks_up_external_db_edits` |
| 19 | `facade::poll_refresh_single_flights_concurrent_callers` |
| 20 | `facade::refresh_rebuilds_tree_after_new_tracks` |
| 21 | `facade::unchanged_refresh_poll_consumes_debounce_window` |
| 22 | `ogg_index::build_index_renumbers_and_preserves_payload_length` |

### 4.3 Tier-1 e2e (ignored)

#### musefs-fuse

| # | Test |
|---|------|
| 1 | `mount::concurrent_spawns_do_not_race` |
| 2 | `mount::end_to_end_read_through_mount` |
| 3 | `mount::end_to_end_read_through_mount_wav` |
| 4 | `ogg_read_through::oggflac_read_through_validates_pages_and_audio` |
| 5 | `ogg_read_through::opus_read_through_preserves_embedded_art` |
| 6 | `ogg_read_through::opus_read_through_validates_pages_and_audio` |
| 7 | `ogg_read_through::vorbis_read_through_validates_pages_and_audio` |
| 8 | `keep_cache::keep_cache_mount_reflects_retag_after_refresh` |
| 9 | `playback_pcm::all_supported_formats_decode_to_same_pcm_sha_as_source` |

### 4.4 Format tests classified as Tier-2 (not in flakiness set)

These `musefs-format/tests/` files were triaged out of Tier-1 because they test tag/metadata read or locate helpers — not byte-identical synthesis/splice. They belong in the Tier-2 count (§3) but not the 3× flakiness runs.

| File | Rationale |
|------|-----------|
| `flac_pictures.rs` | Tests `read_pictures` (FLAC picture extraction) — metadata read, not synthesis |
| `mp3_pictures.rs` | Tests `read_pictures` (MP3 APIC extraction) — metadata read, not synthesis |
| `mp3_read_tags.rs` | Tests `read_tags` (ID3 frame → canonical key) — tag read |
| `wav_read_tags.rs` | Tests `read_tags` + `read_pictures` (WAV INFO/id3 merge) — tag read |
| `read_metadata.rs` | Tests `read_metadata` + `locate_audio` for FLAC — metadata/locate |
| `read_comments.rs` | Tests `read_vorbis_comments` — tag read |
| `locate.rs` | Tests `locate_audio` for FLAC — locate helper |
| `mp3_locate.rs` | Tests `locate_audio` for MP3 — locate helper |
| `wav_locate.rs` | Tests `locate_audio` + `read_structure` for WAV — locate helper |

### 4.5 Task 5 `cargo test` invocations (updated)

The 3× flakiness runs in Task 5 must cover every test above. The updated invocations:

```bash
# Byte-identical / read path — musefs-core
cargo test -p musefs-core --test read_at --test reader --test facade -- \
  read_at_preserves_backing_audio \
  random_offset_and_size_match_the_whole_read \
  read_at_streams_art_image_segments \
  reading_past_eof_returns_empty \
  reading_whole_file_matches_total_len_and_splices_audio \
  reads_a_synthesized_m4a_through_the_facade \
  reads_a_synthesized_mp3_through_the_facade \
  serves_flac_with_embedded_art_through_the_facade \
  serves_mp3_with_embedded_art_through_the_facade \
  lookup_getattr_readdir_and_read_through_the_facade \
  open_handle_read_and_release_roundtrip \
  readdir_distinguishes_a_file_from_an_unknown_inode \
  parent_exposes_the_tree_hierarchy

# Resolution / freshness — musefs-core
cargo test -p musefs-core --test external_contract --test tree --test facade -- \
  resolve_builds_layout_and_total_len \
  resolve_caches_until_content_version_changes \
  resolve_errors_when_audio_bounds_overrun_the_file \
  resolve_errors_when_backing_file_changes \
  resolve_includes_art_image_segments \
  structure_only_resolves_to_whole_backing_file \
  root_node_is_a_directory \
  builds_directories_and_files_with_lookup \
  parent_of_root_is_root_and_children_point_back \
  disambiguates_colliding_file_names \
  inode_is_stable_across_refresh \
  failed_refresh_retries_after_backoff_not_every_call \
  poll_refresh_debounces_within_interval \
  poll_refresh_keeps_unchanged_entries_and_prunes_vanished \
  poll_refresh_notify_invalidates_old_inode_for_removed_track \
  poll_refresh_notify_reports_changed_track_inode \
  poll_refresh_notify_reports_old_inode_for_path_changing_retag \
  poll_refresh_picks_up_external_db_edits \
  poll_refresh_single_flights_concurrent_callers \
  refresh_rebuilds_tree_after_new_tracks \
  unchanged_refresh_poll_consumes_debounce_window

# Resolution / freshness — ogg_index lib test
cargo test -p musefs-core --lib -- build_index_renumbers_and_preserves_payload_length

# Byte-identical / read path — musefs-format (synthesis)
cargo test -p musefs-format --features fuzzing \
  --test layout --test roundtrip --test synthesize_tags --test synthesize_art \
  --test mp3_synthesize --test wav_synthesize --test mp4_oracle

# Byte-identical / read path — musefs-format (property tests)
cargo test -p musefs-format --features fuzzing \
  --test proptest_flac --test proptest_mp3 --test proptest_mp4 \
  --test proptest_ogg --test proptest_wav

# Tier-1 e2e (ignored)
cargo test -p musefs-fuse --test mount --test ogg_read_through \
  --test keep_cache --test playback_pcm -- --ignored
```

### 4.6 Triage summary

| Metric | Count |
|--------|-------|
| Tier-1 tests (§4.1 + §4.2 + §4.3) | 79 |
| — byte-identical / read path | 48 (13 core + 35 format) |
| — resolution / freshness | 22 (core) |
| — Tier-1 e2e | 9 |
| Format files reclassified Tier-1 (added beyond Step 1) | 3 (`mp3_synthesize`, `wav_synthesize`, `mp4_oracle`) |
| Format files classified Tier-2 | 9 |

Enumerated Tier-1 set: **complete** (79 tests).
Flaky tests (file:line): **none** — all 79 Tier-1 tests passed identically across 3× unit/integration and 3× e2e (both thread settings).
Metrics-gated stability (Tier-2): core metrics blocked — compile error (`NewTrack::backing_mtime_secs` in `musefs-core/tests/metrics.rs:177`; field renamed to `backing_mtime`). FUSE metrics stable — `slow_read_does_not_block_stat` passes consistently. No flakiness in either Tier-2 category; compile failure is a known bug, not a timing issue.

## 5. Red-test gate decision

**PASS** — all 79 Tier-1 tests (48 byte-identical + 22 resolution/freshness + 9 e2e) passed identically across 3× runs. No flaky tests detected.

| Check | Result |
|-------|--------|
| Tier-1 unit/integration 3× | **PASS** — 34 core + 35 format tests, zero flakes |
| Tier-1 e2e 3× (threads=1 + default) | **PASS** — 9 tests, zero flakes |
| Flaky tests (file:line) | none |
| Metrics-gated (Tier-2, non-halting) | core metrics: compile error (`musefs-core/tests/metrics.rs:177` — `backing_mtime_secs` → `backing_mtime`); FUSE metrics: stable |
| Gate verdict | **PASS** — continue to Phase B/C |

Evidence: `/tmp/musefs-audit/flaky-core.log`, `flaky-format.log`, `flaky-e2e.log`, `flaky-metrics.log`

## 6. Coverage (Phase A)

Basis: `cargo llvm-cov --workspace --exclude musefs-fuse` (includes fuzzing proptests, excludes `#[ignore]`d e2e and the `musefs-fuse` crate). Coverage data captured to `/tmp/musefs-audit/coverage.json` and `/tmp/musefs-audit/coverage-summary.log`.

**Overall: 94.1% line | 93.4% region | 96.5% function (5844 lines, 11281 regions, 522 functions)**

### Per-crate

| Crate | Lines | Line % | Regions | Region % | Functions | Fn % |
|-------|------:|-------:|--------:|---------:|----------:|-----:|
| musefs-format | 3547 | 96.4% | 7131 | 96.2% | 293 | 99.0% |
| musefs-core | 1817 | 93.7% | 3333 | 91.8% | 165 | 97.0% |
| musefs-db | 370 | 93.2% | 689 | 85.5% | 53 | 98.1% |
| musefs-cli | 110 | 30.9% | 128 | 22.7% | 11 | 18.2% |
| musefs-fuse | — | — e2e | — | — e2e | — | — e2e |

`musefs-fuse` coverage is scored by FUSE e2e evidence (see §3/§7), not llvm-cov instrumentation (FUSE coverage strategy).

### Per-module (notable modules)

**musefs-core**

| Module | Lines | Line % | Regions | Region % |
|--------|------:|-------:|--------:|---------:|
| `reader.rs` | 708 | 94.2% | 1275 | 92.2% |
| `facade.rs` | 334 | 92.8% | 527 | 90.7% |
| `scan.rs` | 268 | 87.7% | 549 | 85.1% |
| `tree.rs` | 184 | 97.3% | 359 | 98.6% |
| `ogg_index.rs` | 97 | 95.9% | 190 | 93.2% |
| `db_pool.rs` | 87 | 98.9% | 179 | 96.1% |
| `template.rs` | 63 | 98.4% | 97 | 97.9% |
| `mapping.rs` | 68 | 98.5% | 144 | 92.4% |
| `metrics.rs` | 8 | 50.0% | 13 | 61.5% |

**musefs-db**

| Module | Lines | Line % | Regions | Region % |
|--------|------:|-------:|--------:|---------:|
| `lib.rs` | 92 | 100.0% | 176 | 93.2% |
| `schema.rs` | 18 | 94.4% | 49 | 85.7% |
| `art.rs` | 97 | 92.8% | 180 | 81.1% |
| `models.rs` | 45 | 95.6% | 64 | 93.8% |
| `tracks.rs` | 76 | 85.5% | 129 | 80.6% |
| `tags.rs` | 42 | 90.5% | 91 | 80.2% |

**musefs-format**

| Module | Lines | Line % | Regions | Region % |
|--------|------:|-------:|--------:|---------:|
| `fuzz_check.rs` | 228 | 100.0% | 429 | 100.0% |
| `tagmap.rs` | 99 | 100.0% | 183 | 99.5% |
| `b64.rs` | 41 | 100.0% | 81 | 100.0% |
| `page.rs` | 411 | 99.0% | 803 | 99.0% |
| `vorbiscomment.rs` | 74 | 98.6% | 159 | 96.9% |
| `mod.rs` (ogg) | 762 | 96.6% | 1414 | 96.7% |
| `mp4.rs` | 1009 | 97.2% | 2409 | 96.3% |
| `flac.rs` | 226 | 95.1% | 363 | 94.5% |
| `wav.rs` | 238 | 95.0% | 453 | 94.0% |
| `layout.rs` | 43 | 93.0% | 63 | 93.7% |
| `mp3.rs` | 378 | 92.9% | 708 | 92.5% |
| `crc.rs` | 32 | 53.1% | 61 | 68.9% |

**musefs-cli**

| Module | Lines | Line % | Regions | Region % |
|--------|------:|-------:|--------:|---------:|
| `lib.rs` | 104 | 32.7% | 121 | 24.0% |
| `main.rs` | 6 | 0.0% | 7 | 0.0% |

### Notable low-coverage areas

- **`ogg/crc.rs`** (53.1% line): CRC path-mismatch and invalid-tap branches are dead in unit tests; only the happy path and one reference comparison are exercised. Region coverage gaps are in the `crc_update` inner loop variants.
- **`musefs-db/tracks.rs`** (85.5% line, 80.6% region): `delete_track` cascade paths and `upsert` conflict-resolution branches have partial coverage; several error branches in the SQL layer are untested.
- **`musefs-db/tags.rs`** (90.5% line, 80.2% region): multi-value tag grouping and empty-set edge cases leave region gaps in the `GROUP BY` assembly paths.
- **`musefs-db/art.rs`** (92.8% line, 81.1% region): `gc_orphan_art` concurrent-deletion race paths and `linking_art` edge cases are partially covered.
- **`musefs-core/scan.rs`** (87.7% line, 85.1% region): probe fallback paths for malformed inputs and art-ingestion edge branches remain uncovered.
- **`musefs-cli`** (30.9% line): mostly thin CLI dispatch glue; unit-testable logic is minimal. Low coverage is expected for a binary crate with integration tests driving it end-to-end.
- **`musefs-core/metrics.rs`** (50.0% line): metrics feature-gated; only basic construction is tested without the `metrics` feature enabled in coverage run.

## 7. Fuzz smoke & corpus health (Phase A)

**Toolchain:** nightly-2026-05-29, cargo-fuzz 0.13.1. All 8 targets built cleanly.

**Smoke results (15s, `max_len=131072`, `rss_limit_mb=2048`):** zero crashes across all targets.

| Target | Runs | Edges (cov) | Features (ft) | Corpus entries | Corpus size |
|--------|------|-------------|---------------|----------------|-------------|
| flac | 195k | 663 | 1956 | 310 | 1065Kb |
| mp3 | 310k | 2068 | 4586 | 758 | 43Kb |
| mp4 | 906k | 601 | 1096 | 187 | 42Kb |
| ogg | 181k | 723 | 1702 | 268 | 909Kb |
| wav | 803k | 2128 | 4114 | 572 | 56Kb |
| ogg_page | 7.7M | 50 | 68 | 17 | 672b |
| b64 | 757k | 198 | 925 | 57 | 10Kb |
| vorbiscomment | 803k | 134 | 550 | 141 | 74Kb |

**Corpus sizes (on disk):**

| Target | Files |
|--------|-------|
| flac | 935 |
| mp3 | 3506 |
| mp4 | 597 |
| ogg | 852 |
| wav | 1744 |
| ogg_page | 51 |
| b64 | 110 |
| vorbiscomment | 1108 |

**Coverage merge:** `cargo fuzz coverage` runs for ogg/mp4/flac completed fuzzing (759/627/692 edges respectively) but the `llvm-profdata merge` step failed — `llvm-profdata` is absent from the nightly toolchain. The fuzz engine's own edge counts (`cov:`) remain valid for relative comparison.

**Observations:**
- `ogg_page` has very shallow coverage (50 edges, 68 features) — the target is a thin page-header parser with minimal logic; corpus is tiny (51 files) and saturated quickly.
- `b64` and `vorbiscomment` also show shallow reach (198/134 edges) — these are narrow utility parsers.
- `mp3` and `wav` are the deepest parsers (2068/2128 edges) with healthy feature diversity.
- Corpus sizes for `mp3` (3506) and `wav` (1744) are relatively large; `cargo fuzz cmin` could reduce redundant entries (backlog recommendation — do not run `cmin` during audit).
- No broken/unreachable targets found.

**Backlog recommendation:** Run `cargo fuzz cmin` on mp3 and wav corpora to prune redundant entries. Install `llvm-tools` component (`rustup component add llvm-tools-preview`) for full coverage merge in future audits.

## 8. Schema-parity check (Phase A)

**Result: CLEAN** — no drift detected.

After stripping SQL comments, whitespace-normalizing, dropping `PRAGMA user_version` from the fixture, and extracting only the raw-string body of `MIGRATION_V1` from `musefs-db/src/schema.rs`, the sorted line diff between `contrib/beets/tests/schema_v1.sql` and the production Rust migration is empty.

Fixture: `contrib/beets/tests/schema_v1.sql` (52 non-comment, non-blank lines after normalization).
Production: `musefs-db/src/schema.rs` `MIGRATION_V1` const (52 lines after normalization).

No missing/renamed columns, tables, triggers, or indexes. Schema parity confirmed.

## 9. Mutation testing (Phase B)

_pending — or "blocked — suite not green" if the gate tripped._

## 10. Per-area scorecard

_pending. Columns: Coverage | Quality | Edge cases, per Tier-1/Tier-2 area._

## 11. Findings

_pending. Format: `file:line` — description — severity (P0/P1/P2)._

## 12. Prioritized remediation backlog

_pending. P0/P1/P2; each item names the target test file and exactly what to add/fix._
