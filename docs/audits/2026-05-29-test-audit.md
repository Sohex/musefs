# musefs Test Suite Audit — 2026-05-29

**Status:** complete (full audit)
**Spec:** docs/superpowers/specs/2026-05-29-test-audit-design.md
**Deliverable type:** _full audit_ | _red-test halt report_ (set at the Phase A gate)

## 1. Executive summary

**Overall health:** Strong. 259 workspace tests, 9 FUSE e2e, 66 beets tests, 8 fuzz targets — all green. 94.1% line coverage across the workspace (excluding FUSE e2e instrumentation). The byte-identical synthesis invariant is well-backed for FLAC/MP3/MP4/WAV (oracle-validated, proptest-backed, 3× flakiness-stable) but weak for Ogg (no independent Ogg audio oracle at the unit-test level).

### Headline counts (per category)

| Category | Tests | Status |
|----------|------:|--------|
| Workspace (incl. fuzzing proptests) | 259 | all pass |
| FUSE e2e (`--ignored`) | 9 | all pass |
| FUSE concurrency (`--features metrics`) | 1 | pass |
| Core metrics (`--features metrics`) | 4 | BLOCKED — compile error (`backing_mtime_secs` → `backing_mtime` in `musefs-core/tests/metrics.rs:177`) |
| Interop emitter + mutagen | 3 | all pass |
| beets pytest (all subsets) | 66 | all pass |
| cargo-fuzz targets | 8 | all smoke-clean, 0 crashes |

**Tier-1 flakiness:** PASS — 79 tests, 3× stable across unit/integration and e2e (both thread settings). No flaky tests detected.

**Coverage:** 94.1% line | 93.4% region | 96.5% function. Notable gaps: `ogg/crc.rs` (53.1% — partly const-table inflation), `musefs-db/tracks.rs` (85.5% line, 80.6% region), `musefs-db/tags.rs` (90.5% line, 80.2% region), `musefs-core/scan.rs` (87.7% line).

**Schema parity:** CLEAN — no drift between `contrib/beets/tests/schema_v1.sql` and production migration.

### Top risks

1. **Ogg serve() untested** — `ogg_index.rs:83` serves every Ogg read but has zero unit tests (P1).
2. **No independent Ogg oracle** — `resolve_layout()` marks `Segment::OggAudio` as `unreachable!()`, so Ogg audio segments are never unit-tested for byte-identity (P1).
3. **Mutation testing partial** — `musefs-format` mutation score is 77% but only 16% of mutants were tested (195/1237); `musefs-db` mutation testing is vacuous (all 19 mutants structurally unviable due to missing `Default` impl).

### Byte-identical invariant test backing

- **FLAC:** Well-backed — 8 integration tests + proptest + metaflac oracle + 3× stable.
- **MP3:** Well-backed — 8 integration tests + proptest + id3 oracle + 3× stable.
- **MP4:** Well-backed — 2 integration tests + proptest + mp4-crate oracle + 3× stable.
- **WAV:** Well-backed — 5 integration tests + proptest + hound oracle + 3× stable.
- **Ogg:** Adequate at the e2e level (4 FUSE read-through tests) but weak at the unit-test level — no independent oracle materializes Ogg audio segments to verify page CRCs.

### Remediation backlog

| Priority | Count |
|----------|------:|
| P0 | 0 |
| P1 | 3 |
| P2 | 13 |

Full backlog in §12.

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

**Tool:** cargo-mutants 27.0.0. **Gate status:** §5 = PASS, so Phase B runs.

**Flakiness caveat:** No flaky tests found in Task 5 (§4.6), so flakiness is N/A for mutation results.

**Environment note:** `--test-workspace=true` exhausts `/tmp` (3.9 GB tmpfs) for `musefs-core` and `musefs-format` due to criterion/proptest scratch builds; `TMPDIR` was redirected to `/home` and `--test-workspace=false` used for core and format. `musefs-db` used the specified `--test-workspace=true` (completed within quota).

### 9.1 musefs-db

| Metric | Value |
|--------|-------|
| Command | `cargo mutants -p musefs-db --test-workspace=true --file musefs-db/src/schema.rs --file musefs-db/src/lib.rs` |
| Mutants generated | 19 |
| Tested | 0 |
| Caught | 0 |
| Missed | 0 |
| Unviable | 19 |
| Timeouts | 0 |
| Mutation score | N/A — no viable mutants |

**Root cause of unviable mutants:** All 19 generated mutants replace function bodies with `Ok(Default::default())` or `Ok(0)` / `Ok(1)` / `Ok(-1)`. Since `Db` does not implement `Default`, every replacement fails to compile (`E0277: the trait bound Db: Default is not satisfied`). cargo-mutants cannot generate type-correct replacements for these return types. This is a tool limitation, not a test-quality signal.

**Out of scope:** `musefs-db/src/db_pool.rs` — deferred to Phase C (connection-pool assessment).

### 9.2 musefs-core

| Metric | Value |
|--------|-------|
| Command | `cargo mutants -p musefs-core --test-workspace=false --file musefs-core/src/reader.rs --file musefs-core/src/tree.rs --file musefs-core/src/scan.rs --file musefs-core/src/facade.rs --file musefs-core/src/ogg_index.rs` |
| Mutants generated | 353 |
| Tested | 353 |
| Caught | 237 |
| Missed | 57 |
| Unviable | 57 |
| Timeouts | 2 |
| Mutation score (caught / (caught + missed)) | **80.1%** (237 / 296) |

#### Surviving mutants (missed + timeout)

**facade.rs** (7 missed):

| Line | Mutation |
|------|----------|
| 198:9 | `replace Musefs::refresh -> Result<()> with Ok(())` |
| 267:17 | `replace < with <= in poll_refresh_notify` |
| 276:38 | `replace < with <= in poll_refresh_notify` |
| 412:38 | `replace == with != in getattr` |
| 458:15 | `replace != with == in read` |
| 487:9 | `replace Musefs::open_handle -> Result<u64> with Ok(1)` |
| 509:9 | `replace Musefs::release_handle with ()` |

**ogg_index.rs** (3 missed):

| Line | Mutation |
|------|----------|
| 105:15 | `replace < with <= in serve` |
| 113:15 | `replace < with <= in serve` |
| 117:83 | `replace + with - in serve` |

**reader.rs** (30 missed + 0 timeout):

| Line | Mutation |
|------|----------|
| 114:24 | `replace -= with += in Shard::insert` |
| 114:24 | `replace -= with /= in Shard::insert` |
| 128:40 | `replace && with \|\| in Shard::insert` |
| 128:26 | `replace > with >= in Shard::insert` |
| 128:58 | `replace > with >= in Shard::insert` |
| 132:24 | `replace -= with += in Shard::insert` |
| 132:24 | `replace -= with /= in Shard::insert` |
| 136:9 | `replace Shard::retain_keys with ()` |
| 140:25 | `delete ! in Shard::retain_keys` |
| 145:28 | `replace -= with += in Shard::retain_keys` |
| 145:28 | `replace -= with /= in Shard::retain_keys` |
| 159:49 | `replace * with +` |
| 159:49 | `replace * with /` |
| 159:42 | `replace * with +` |
| 159:42 | `replace * with /` |
| 182:33 | `replace / with % in HeaderCache::with_budget` |
| 182:33 | `replace / with * in HeaderCache::with_budget` |
| 189:36 | `replace % with / in HeaderCache::shard` |
| 196:9 | `replace HeaderCache::retain with ()` |
| 251:21 | `replace \|\| with && in HeaderCache::build` |
| 250:39 | `replace < with == in HeaderCache::build` |
| 250:39 | `replace < with <= in HeaderCache::build` |
| 251:43 | `replace < with == in HeaderCache::build` |
| 251:43 | `replace < with <= in HeaderCache::build` |
| 350:13 | `replace + with * in HeaderCache::build` |
| 346:17 | `delete match arm Segment::Inline(b) in HeaderCache::build` |
| 351:17 | `delete match arm Format::Opus \| Format::Vorbis \| Format::OggFlac in HeaderCache::build` |
| 374:37 | `replace \|\| with && in read_at` |
| 403:37 | `replace \|\| with && in read_segments` |
| 415:21 | `replace < with <= in read_segments` |

**scan.rs** (15 missed):

| Line | Mutation |
|------|----------|
| 13:33 | `replace * with +` |
| 43:5 | `replace is_supported_audio -> bool with true` |
| 48:9 | `replace \|\| with && in is_supported_audio` |
| 47:9 | `replace \|\| with && in is_supported_audio` |
| 60:35 | `replace && with \|\| in collect_audio` |
| 107:36 | `replace \|\| with && in probe` |
| 152:14 | `replace += with -= in ingest` |
| 166:31 | `replace != with == in ingest` |
| 167:33 | `replace != with == in ingest` |
| 209:27 | `replace += with -= in scan_directory` |
| 209:27 | `replace += with *= in scan_directory` |
| 242:17 | `replace && with \|\| in revalidate` |
| 252:27 | `replace += with -= in revalidate` |
| 252:27 | `replace += with *= in revalidate` |
| 266:23 | `replace match guard e.kind() == NotFound with true in revalidate` |

**tree.rs** (2 missed + 2 timeout):

| Line | Mutation | Status |
|------|----------|--------|
| 185:24 | `replace match guard i > 0 with true in disambiguate` | MISSED |
| 185:26 | `replace > with >= in disambiguate` | MISSED |
| 194:16 | `delete ! in disambiguate` | TIMEOUT |
| 197:15 | `replace += with *= in disambiguate` | TIMEOUT |

### 9.3 musefs-format (crate-local-only)

| Metric | Value |
|--------|-------|
| Command | `cargo mutants -p musefs-format --test-workspace=false --features fuzzing --file musefs-format/src/ogg/mod.rs --file musefs-format/src/ogg/page.rs --file musefs-format/src/ogg/crc.rs --file musefs-format/src/ogg/b64.rs --file musefs-format/src/mp4.rs --file musefs-format/src/flac.rs --file musefs-format/src/wav.rs --file musefs-format/src/mp3.rs` |
| Mutants generated | 1237 |
| Tested (before 30-min cap) | 195 |
| Caught | 148 |
| Missed | 44 |
| Unviable | 5 |
| Timeouts | 0 |
| Mutation score (partial, flac.rs only) | **77.0%** (148 / 192) |
| Files reached | `flac.rs` only |
| Files NOT reached | `ogg/mod.rs`, `ogg/page.rs`, `ogg/crc.rs`, `ogg/b64.rs`, `mp4.rs`, `wav.rs`, `mp3.rs` |

**Partial run:** The 30-minute cap killed the run while processing `flac.rs`. Only `flac.rs` mutants were tested. The remaining 7 files (1042 mutants) were never reached. This result is **crate-local-only**.

**Unviable mutants (flac.rs):** 5 mutants replaced `Result<T>` returns with `Ok(Default::default())` — same structural limitation as `musefs-db`.

#### Surviving mutants (flac.rs — partial)

All 44 missed mutants are in `flac.rs`. Dominant patterns:
- **Bitwise operator mutations** (`|` → `^`, `<<` → `>>`): 14 mutants in `parse_blocks`, `push_block_header`, `read_vorbis_comments`, `read_pictures` — tests exercise only the happy path; boundary-bit and endianness mutations pass undetected.
- **Comparison boundary mutations** (`<` → `<=`, `>` → `>=`, `>` → `==`): 18 mutants across `parse_blocks`, `read_vorbis_comments`, `read_u32_be`, `parse_picture_block`, `read_pictures` — off-by-one in block-length checks is untested.
- **Arithmetic mutations** (`+` → `-`, `+` → `*`, `<<` → `>>`): 7 mutants in `parse_blocks`, `read_vorbis_comments`, `read_u32_be`, `read_pictures`.
- **Logical operator mutations** (`||` → `&&`): 3 mutants in `read_vorbis_comments`, `read_pictures` — loop-guard inversion passes undetected.

### 9.4 Summary

| Crate | Mutants | Tested | Caught | Missed | Unviable | Timeout | Score |
|-------|--------:|-------:|-------:|-------:|---------:|--------:|------:|
| musefs-db | 19 | 0 | 0 | 0 | 19 | 0 | N/A |
| musefs-core | 353 | 353 | 237 | 57 | 57 | 2 | 80.1% |
| musefs-format | 1237 | 195 | 148 | 44 | 5 | 0 | 77.0% (partial) |

**Key observations:**
1. `musefs-db` mutation testing is vacuous — all generated mutants are structurally unviable due to `Db` lacking `Default`. The crate's test suite should be assessed via manual code review or by implementing `Default` for `Db`.
2. `musefs-core` at 80.1% is reasonable but the missed mutants cluster in `reader.rs` (header-cache internals), `scan.rs` (file-system operations), and `facade.rs` (FUSE-layer glue) — these are areas where tests use mock/stub paths that don't exercise all branches.
3. `musefs-format` is heavily partial (16% of mutants tested). The `flac.rs` parser shows a systematic gap: bitwise and boundary mutations in block-length parsing are undetected. The remaining 7 files (ogg, mp4, wav, mp3) could not be reached within the cap.
4. The two `tree.rs` timeouts indicate `VirtualTree::disambiguate` mutations cause hangs (likely infinite loops in name-collision resolution), suggesting the test suite lacks a timeout guard for this path.

**Out of scope by decision:**
- `musefs-db/src/db_pool.rs` — connection pool assessed in Phase C.
- `contrib/beets/` Python code — Python mutation deferred.

## 10. Per-area scorecard

### Tier-1 areas

| Area | Coverage | Quality | Edge cases | Basis / notes |
|------|----------|---------|------------|---------------|
| **FLAC read/synthesis** (musefs-format) | 95.1% line (`flac.rs`) | High — 8 integration tests + proptest, byte-identical oracle via `metaflac` | Partial: no explicit 0-byte audio test; multi-value and Unicode tags covered | Coverage from §6; quality from §4.1 format tests |
| **MP3 read/synthesis** (musefs-format) | 92.9% line (`mp3.rs`) | High — 8 integration tests + proptest, `id3` oracle, tag-limit and oversized-frame guards | Good: empty tag, TXXX, multi-value, ordering, oversize all tested | Coverage from §6; quality from §4.1 |
| **MP4 read/synthesis** (musefs-format) | 97.2% line (`mp4.rs`) | High — 2 integration tests + proptest, `mp4` crate oracle | Good: art dedup tested | Coverage from §6 |
| **WAV read/synthesis** (musefs-format) | 95.0% line (`wav.rs`) | High — 5 integration tests + proptest, `hound` oracle | Good: word-alignment, 32-bit limit, LIST/INFO | Coverage from §6 |
| **Ogg page/CRC** (musefs-format) | 53.1% line (`ogg/crc.rs`); 99.0% line (`ogg/page.rs`) | Moderate — CRC has 1 test (reference comparison); page.rs has 10 tests covering parse, lace, patch, multi-page | Weak: CRC const-table lines inflate gap; no known-bad-CRC rejection test; no EOS flag test | Coverage from §6; crc.rs gap is mostly const-table build |
| **Ogg synthesis** (musefs-format) | 96.6% line (`ogg/mod.rs`) | High — 12 integration tests for Opus/Vorbis/OggFLAC synthesis + art round-trips | Good: multiplexed rejection tested; art multi-page tested; oversized art rejected | Coverage from §6 |
| **ogg_index** (musefs-core) | 95.9% line (`ogg_index.rs`) | Moderate — 1 unit test (`build_index_renumbers_and_preserves_payload_length`) | Weak: `serve()` has zero unit tests; `consume != audio_length` error path untested; CRC revalidation untested; EOS untested | Coverage from §6; quality concern is serve() on every Ogg read |
| **reader.rs** (musefs-core) | 94.2% line | High — 4 integration tests (`reader::*`), proptest for FLAC audio identity | Partial: partial reads tested indirectly via facade; art streaming tested; EOF tested | Coverage from §6 |
| **facade.rs** (musefs-core) | 92.8% line | High — 17 integration tests covering refresh, inode stability, concurrent callers, tree hierarchy | Good: BackingChanged, debounce, notify, pruning, retries | Coverage from §6 |
| **tree.rs** (musefs-core) | 97.3% line | High — 4 integration tests | Good: root, children, disambiguation | Coverage from §6 |
| **external_contract** (musefs-core) | N/A (part of facade) | High — 6 integration tests | Good: structure-only mode, caching, art segments, error paths | §4.2 |
| **db_pool.rs** (musefs-core) | 98.9% line | High — 3 unit tests | Good: shared, per-thread, cross-thread | Coverage from §6 |
| **scan.rs** (musefs-core) | 87.7% line | Moderate — covered by facade tests (scan via `refresh_rebuilds_tree_after_new_tracks`) | Partial: probe fallback for malformed inputs untested at unit level | Coverage from §6 |

### Tier-1 e2e (FUSE)

| Area | Coverage | Quality | Edge cases | Basis |
|------|----------|---------|------------|-------|
| **FLAC/MP3/WAV read-through** | N/A (e2e) | High — 4 mount tests pass consistently across 3× runs | Good: byte-identical via PCM SHA | §4.3, §3 |
| **Ogg read-through** | N/A (e2e) | High — 4 tests (OggFLAC, Opus×2, Vorbis) validate pages + audio + art | Good: page validation + art preservation | §4.3 |
| **Concurrency** | N/A (e2e) | High — `concurrent_spawns_do_not_race` | Good | §4.3 |
| **Keep-cache** | N/A (e2e) | High — `keep_cache_mount_reflects_retag_after_refresh` | Good | §4.3 |
| **Playback PCM** | N/A (e2e) | High — all formats decode to same PCM SHA | Good: cross-format invariant | §4.3 |

### Tier-2 areas

| Area | Coverage | Quality | Edge cases | Basis |
|------|----------|---------|------------|-------|
| **beets plugin** | 52/8/6 tests (default/musefs_bin/e2e) | High — all pass; FK parity gap in `db_path` fixture | Moderate: no FK parity in raw `db_path` connection | §3, conftest.py:16 |
| **musefs-cli** | 30.9% line | Low — thin dispatch glue, tested via integration | Expected for binary crate | §6 |
| **musefs-db tracks.rs** | 85.5% line, 80.6% region | Moderate — delete cascade and upsert conflict branches partially covered | Weak: several SQL error branches untested | §6 |
| **musefs-db art.rs** | 92.8% line, 81.1% region | Moderate — gc_orphan_art race paths partially covered | Weak: concurrent-deletion edge cases | §6 |
| **musefs-db tags.rs** | 90.5% line, 80.2% region | Moderate — multi-value tag grouping has region gaps | Weak: GROUP BY assembly edge cases | §6 |
| **musefs-core/metrics.rs** | 50.0% line | Blocked — compile error (`backing_mtime_secs` field renamed); cannot run | N/A | §3 |

### Quality summary

- **Strong:** FLAC/MP3/MP4/WAV synthesis (oracle-validated, proptest-backed), facade refresh logic, tree construction, FUSE e2e read-through
- **Adequate:** ogg_index build (happy path), scan via facade, db_pool
- **Weak:** ogg_index `serve()` (no unit test), CRC edge cases, db SQL error branches, beets FK parity

## 11. Findings

| # | Location | Description | Severity |
|---|----------|-------------|----------|
| 1 | `musefs-core/src/ogg_index.rs:83` | `serve()` function (header/payload splitting + backing read) has zero unit tests; only exercised indirectly via FUSE e2e | P1 |
| 2 | `musefs-format/tests/common/mod.rs:80` | `resolve_layout()` oracle marks `Segment::OggAudio` as `unreachable!()` — no independent oracle materializes Ogg audio segments to verify page CRCs at the unit-test level | P1 |
| 3 | `musefs-core/src/ogg_index.rs:72` | `consume != audio_length` error path in `build_index` is untested (the one test always produces exact consumption) | P1 |
| 4 | `musefs-core/src/ogg_index.rs:131` | `build_index_renumbers_and_preserves_payload_length` only verifies seq on page[0]; does not verify FLAG_CONTINUED on page[1], CRC revalidation, or payload_len consistency for all pages | P2 |
| 5 | `musefs-core/tests/proptest_read_fidelity.rs:36` | Property test only reads from offset 0; does not test partial reads, header/audio boundary spanning, art segment serving, or non-FLAC formats | P2 |
| 6 | `contrib/beets/tests/conftest.py:16` | `db_path` fixture creates SQLite connection without `PRAGMA foreign_keys = ON`; production (`musefs-db/src/lib.rs:44`) sets it. The `make_track` path uses `musefs_connect()` (which sets FK), but raw `conn` usage from `db_path` lacks FK enforcement | P2 |
| 7 | `musefs-format/src/ogg/crc.rs:53` | CRC unit test has only 1 test (`matches_independent_reference`); no test for single-byte input, max-byte (0xFF) input, or specific patterns that exercise different polynomial tap paths; 53.1% line coverage is partly const-table inflation | P2 |
| 8 | `musefs-core/src/ogg_index.rs:83` | No test for `serve()` boundary conditions: header-only read, payload-only read, read spanning header+payload, empty result, read past end of audio region | P2 |
| 9 | `musefs-core/src/scan.rs:79` | `probe()` fallback paths for malformed inputs (truncated headers, invalid magic) not tested at unit level; 87.7% line coverage leaves error branches uncovered | P2 |
| 10 | `musefs-db/src/tracks.rs:31` | `delete_track` cascade paths and `upsert` conflict-resolution branches have region gaps (85.5% line, 80.6% region); SQL error branches untested | P2 |
| 11 | `musefs-db/src/art.rs:125` | `gc_orphan_art` concurrent-deletion race paths and `linking_art` edge cases partially covered (81.1% region) | P2 |
| 12 | `musefs-db/src/tags.rs:38` | Multi-value tag grouping and empty-set edge cases leave region gaps in `GROUP BY` assembly paths (80.2% region) | P2 |
| 13 | `musefs-core/tests/metrics.rs:177` | Compile error: `NewTrack` has no field `backing_mtime_secs` (renamed to `backing_mtime`); all 4 metrics tests blocked | P2 |
| 14 | `musefs-format/src/ogg/page.rs:6` | No test for EOS (end-of-stream) flag behavior in Ogg pages; `FLAG_EOS` is not defined or handled in the parser | P2 |
| 15 | (no file) | No test for NFS-style `ESTALE` error on backing file read; FUSE mount would propagate raw io::Error | P2 |
| 16 | (no file) | No test for zero-byte embedded art (empty image); code handles it but no explicit boundary test | P2 |

### Reconciliation against Step-1 candidates

My Step-1 independent list identified 10 candidates. The named callouts (Step 2) identified 2 areas. Cross-check:

- **Candidate 1 (crc.rs):** Confirmed — Finding #7. Severity P2 (partly const-table inflation).
- **Candidate 2 (serve()):** Confirmed — Finding #1, #8. Severity P1.
- **Candidate 3 (proptest_read_fidelity):** Confirmed — Finding #5. Severity P2.
- **Candidate 4 (synthesize_layout edge cases):** Partially addressed by existing Ogg synthesis tests; not escalated.
- **Candidate 5 (tracks.rs):** Confirmed — Finding #10. Severity P2.
- **Candidate 6 (art.rs):** Confirmed — Finding #11. Severity P2.
- **Candidate 7 (scan.rs):** Confirmed — Finding #9. Severity P2.
- **Candidate 8 (resolve_layout oracle gap):** Confirmed — Finding #2. Severity P1.
- **Candidate 9 (beets FK parity):** Confirmed — Finding #6. Severity P2.
- **Candidate 10 (serve boundary conditions):** Confirmed — Finding #8. Severity P2.

The callouts did not miss anything from my Step-1 list. The callouts added focus on the `build_index` test's CRC/continued-page gaps (Finding #4) and the `consume != audio_length` error path (Finding #3), which were not in my Step-1 list but are valid P1/P2 findings.

## 12. Prioritized remediation backlog

Ordered P0 → P2. Each item names the target test file and the exact addition or fix.

### P0 — No items

All Tier-1 tests pass; byte-identical invariant is protected for FLAC/MP3/MP4/WAV. No P0 found.

### P1 (3 items)

| # | Finding | Target test file | What to add/fix |
|---|---------|------------------|-----------------|
| P1-1 | `serve()` has zero unit tests (#1) | Add `musefs-core/tests/ogg_index_serve.rs` (or extend existing `ogg_index` test module) | Add unit tests for `serve()`: header-only read, payload-only read, read spanning header+payload, read past end of audio region, empty backing file. Assert returned bytes match expected header/payload split. |
| P1-2 | No independent Ogg oracle (#2) | Add `musefs-format/tests/ogg_oracle.rs` (or extend `musefs-format/tests/common/mod.rs`) | Add an independent Ogg audio oracle that materializes `Segment::OggAudio` to bytes, re-parses the Ogg pages, and asserts CRC validity and byte-identity. Must exercise Opus, Vorbis, and OggFLAC paths independently of `resolve_layout`. |
| P1-3 | `consume != audio_length` error path untested (#3) | Add `musefs-core/tests/ogg_index.rs` (extend existing lib test) | Add a test that produces a mismatched consumption count (e.g. truncated backing file) and asserts the correct error is returned from `build_index`. |

### P2 (13 items)

| # | Finding | Target test file | What to add/fix |
|---|---------|------------------|-----------------|
| P2-1 | `build_index` CRC/continued-page gaps (#4) | Extend `musefs-core/src/ogg_index.rs` (lib test `build_index_renumbers_and_preserves_payload_length`) | Assert `FLAG_CONTINUED` on page[1], verify CRC revalidation across all pages, and confirm payload_len consistency for every page — not just seq on page[0]. |
| P2-2 | `proptest_read_fidelity` offset-0-only (#5) | `musefs-core/tests/proptest_read_fidelity.rs` | Add proptest strategies for random offsets and sizes; test partial reads, header/audio boundary spanning, and art segment serving. Extend beyond offset 0. |
| P2-3 | beets FK parity (#6) | `contrib/beets/tests/conftest.py` | Add `PRAGMA foreign_keys = ON` after connection creation in the `db_path` fixture, matching production `musefs-db/src/lib.rs:44`. |
| P2-4 | CRC edge cases (#7) | Add `musefs-format/tests/ogg_crc.rs` (or extend existing CRC test module) | Add tests for single-byte input, max-byte (0xFF) input, and patterns that exercise different polynomial tap paths. Cover the `crc_update` inner loop variants. |
| P2-5 | `serve()` boundary conditions (#8) | Add `musefs-core/tests/ogg_index_serve.rs` (same file as P1-1) | Test header-only read, payload-only read, read spanning header+payload, empty result, and read past end of audio region. (Subsumed by P1-1; track here if P1-1 is deferred.) |
| P2-6 | `probe()` fallback paths (#9) | Add `musefs-core/tests/scan_probe.rs` (or unit tests in `scan.rs`) | Add unit tests for truncated headers, invalid magic bytes, and other malformed inputs that exercise the `probe()` fallback and error branches. |
| P2-7 | db tracks.rs SQL branches (#10) | Add `musefs-db/tests/tracks_cascade.rs` (or extend existing tracks tests) | Test `delete_track` cascade paths for tracks with tags, art, and multi-value tags. Test `upsert` conflict-resolution branches. |
| P2-8 | db art.rs race paths (#11) | Add `musefs-db/tests/art_gc.rs` (or extend existing art tests) | Test `gc_orphan_art` concurrent-deletion paths. Test `linking_art` edge cases (art linked to multiple tracks, art unlinked then re-linked). |
| P2-9 | db tags.rs GROUP BY gaps (#12) | Add `musefs-db/tests/tags_grouping.rs` (or extend existing tags tests) | Test multi-value tag grouping with empty sets, single-value, and multi-value tags. Verify `GROUP BY` assembly correctness. |
| P2-10 | metrics compile error (#13) | `musefs-core/tests/metrics.rs:177` | Rename `backing_mtime_secs` to `backing_mtime` in the `NewTrack` literal at line 177. Re-run all 4 metrics tests. |
| P2-11 | EOS flag untested (#14) | Add `musefs-format/tests/ogg_eos.rs` (or extend Ogg page tests) | Add a test that constructs an Ogg page with the EOS flag set and verifies it is correctly parsed and propagated. |
| P2-12 | NFS ESTALE not tested (#15) | Deferred — no standard test framework support | Document as a known gap. Consider adding an integration test with a mock filesystem or a conditional NFS-mounted backing directory. Low priority — FUSE mount would propagate raw `io::Error`. |
| P2-13 | Zero-byte art boundary (#16) | Add `musefs-format/tests/synthesize_art.rs` (extend existing art tests) | Add a test that synthesizes zero-byte embedded art (empty image data) and verifies the error or boundary behavior. |
