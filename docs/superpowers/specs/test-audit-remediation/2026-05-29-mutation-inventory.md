# Mutation Survivor Inventory

**Source:** `mutants.yml` `full` job (CI). Supersedes the audit's partial §9
(which only reached `flac.rs`).
**Scope:** `musefs-db`, `musefs-core`, `musefs-format`. `musefs-cli` /
`musefs-fuse` out of scope by decision (see remediation tracking doc).
**Run:** [26632110192](https://github.com/Sohex/musefs/actions/runs/26632110192)
(`workflow_dispatch`, sha `81d6d845d`, 2026-05-29). Durations: db 5m, core 16m,
format 1h56m.

## Totals

| Crate | Tested | Caught | Missed | Unviable | Timeout |
|-------|-------:|-------:|-------:|---------:|--------:|
| `musefs-db` | 62 | 40 | 2 | 20 | 0 |
| `musefs-core` | 353 | 237 | 57 | 57 | 2 |
| `musefs-format` | 1237 | 962 | 227 | 39 | 9 |

Survivors = **missed + timeout** (286 missed, 11 timeout = **297** to kill across
phases 2–4). Unviable mutants don't compile (mostly the `Ok(Default::default())`
pattern, below) and are not survivors.

## How to (re)generate

1. Trigger the campaign: GitHub → Actions → **Mutants** → **Run workflow**
   (`workflow_dispatch`), or wait for the Monday cron.
2. Download the artifacts: `mutants-musefs-db`, `mutants-musefs-core`, and the
   four format shards `mutants-musefs-format-0..3` (format is split across four
   `--shard k/4` matrix legs; each shard tests a disjoint subset).
3. In each artifact the per-result lists live under `<crate>/mutants.out/`
   (cargo-mutants writes `caught.txt` / `missed.txt` / `unviable.txt` /
   `timeout.txt`). Concatenate the four format shards' files before grouping.
   The tables below were produced from those files.

## Tool limitations to revisit (phase 4)

- `musefs-db` mutation is **not** vacuous (40/62 caught), but 20 mutants are
  unviable because they replace a body with `Ok(Default::default())` and `Db` has
  no `Default` — concentrated in `tags.rs` (8), `tracks.rs` (5), `lib.rs` (4),
  `art.rs` (3). Implementing `Default for Db` (phase 4) would make those viable
  and likely surface more survivors.
- A few `musefs-format` / `musefs-core` mutants share the same
  `Ok(Default::default())` unviable pattern.

## Phase routing

`ogg/*` + `ogg_index.rs` → **phase 2**; `flac/mp3/mp4/wav` → **phase 3**;
`reader/tree/scan/facade` + all `musefs-db` → **phase 4**. Per-mutant phase is in
the rightmost column of each survivor table.

## musefs-db

| File | Caught | Missed | Unviable | Timeout |
|------|-------:|-------:|---------:|--------:|
| `art.rs` | 14 | 0 | 3 | 0 |
| `lib.rs` | 7 | 1 | 4 | 0 |
| `schema.rs` | 6 | 1 | 0 | 0 |
| `tags.rs` | 2 | 0 | 8 | 0 |
| `tracks.rs` | 11 | 0 | 5 | 0 |
| **total** | **40** | **2** | **20** | **0** |

### Surviving mutants → phase

| File:line | Mutation | Kind | Phase |
|-----------|----------|------|------:|
| `lib.rs:55` | replace Db::user_version -> Result<i64> with Ok(1) | missed | 4 |
| `schema.rs:93` | replace < with <= in migrate | missed | 4 |

## musefs-core

| File | Caught | Missed | Unviable | Timeout |
|------|-------:|-------:|---------:|--------:|
| `facade.rs` | 60 | 7 | 35 | 0 |
| `ogg_index.rs` | 32 | 3 | 1 | 0 |
| `reader.rs` | 63 | 30 | 8 | 0 |
| `scan.rs` | 34 | 15 | 6 | 0 |
| `tree.rs` | 48 | 2 | 7 | 2 |
| **total** | **237** | **57** | **57** | **2** |

### Surviving mutants → phase

| File:line | Mutation | Kind | Phase |
|-----------|----------|------|------:|
| `facade.rs:198` | replace Musefs::refresh -> Result<()> with Ok(()) | missed | 4 |
| `facade.rs:267` | replace < with <= in Musefs::poll_refresh_notify | missed | 4 |
| `facade.rs:276` | replace < with <= in Musefs::poll_refresh_notify | missed | 4 |
| `facade.rs:412` | replace == with != in Musefs::getattr | missed | 4 |
| `facade.rs:458` | replace != with == in Musefs::read | missed | 4 |
| `facade.rs:487` | replace Musefs::open_handle -> Result<u64> with Ok(1) | missed | 4 |
| `facade.rs:509` | replace Musefs::release_handle with () | missed | 4 |
| `ogg_index.rs:105` | replace < with <= in serve | missed | 2 |
| `ogg_index.rs:113` | replace < with <= in serve | missed | 2 |
| `ogg_index.rs:117` | replace + with - in serve | missed | 2 |
| `reader.rs:114` | replace -= with += in Shard::insert | missed | 4 |
| `reader.rs:114` | replace -= with /= in Shard::insert | missed | 4 |
| `reader.rs:128` | replace > with >= in Shard::insert | missed | 4 |
| `reader.rs:128` | replace && with \|\| in Shard::insert | missed | 4 |
| `reader.rs:128` | replace > with >= in Shard::insert | missed | 4 |
| `reader.rs:132` | replace -= with += in Shard::insert | missed | 4 |
| `reader.rs:132` | replace -= with /= in Shard::insert | missed | 4 |
| `reader.rs:136` | replace Shard::retain_keys with () | missed | 4 |
| `reader.rs:140` | delete ! in Shard::retain_keys | missed | 4 |
| `reader.rs:145` | replace -= with += in Shard::retain_keys | missed | 4 |
| `reader.rs:145` | replace -= with /= in Shard::retain_keys | missed | 4 |
| `reader.rs:159` | replace * with + | missed | 4 |
| `reader.rs:159` | replace * with / | missed | 4 |
| `reader.rs:159` | replace * with + | missed | 4 |
| `reader.rs:159` | replace * with / | missed | 4 |
| `reader.rs:182` | replace / with % in HeaderCache::with_budget | missed | 4 |
| `reader.rs:182` | replace / with * in HeaderCache::with_budget | missed | 4 |
| `reader.rs:189` | replace % with / in HeaderCache::shard | missed | 4 |
| `reader.rs:196` | replace HeaderCache::retain with () | missed | 4 |
| `reader.rs:250` | replace < with == in HeaderCache::build | missed | 4 |
| `reader.rs:250` | replace < with <= in HeaderCache::build | missed | 4 |
| `reader.rs:251` | replace \|\| with && in HeaderCache::build | missed | 4 |
| `reader.rs:251` | replace < with == in HeaderCache::build | missed | 4 |
| `reader.rs:251` | replace < with <= in HeaderCache::build | missed | 4 |
| `reader.rs:346` | delete match arm Segment::Inline(b) in HeaderCache::build | missed | 4 |
| `reader.rs:350` | replace + with * in HeaderCache::build | missed | 4 |
| `reader.rs:351` | delete match arm Format::Opus \| Format::Vorbis \| Format::OggFlac in HeaderCache::build | missed | 4 |
| `reader.rs:374` | replace \|\| with && in read_at | missed | 4 |
| `reader.rs:403` | replace \|\| with && in read_segments | missed | 4 |
| `reader.rs:415` | replace < with <= in read_segments | missed | 4 |
| `scan.rs:13` | replace * with + | missed | 4 |
| `scan.rs:43` | replace is_supported_audio -> bool with true | missed | 4 |
| `scan.rs:47` | replace \|\| with && in is_supported_audio | missed | 4 |
| `scan.rs:48` | replace \|\| with && in is_supported_audio | missed | 4 |
| `scan.rs:60` | replace && with \|\| in collect_audio | missed | 4 |
| `scan.rs:107` | replace \|\| with && in probe | missed | 4 |
| `scan.rs:152` | replace += with -= in ingest | missed | 4 |
| `scan.rs:166` | replace != with == in ingest | missed | 4 |
| `scan.rs:167` | replace != with == in ingest | missed | 4 |
| `scan.rs:209` | replace += with -= in scan_directory | missed | 4 |
| `scan.rs:209` | replace += with *= in scan_directory | missed | 4 |
| `scan.rs:242` | replace && with \|\| in revalidate | missed | 4 |
| `scan.rs:252` | replace += with -= in revalidate | missed | 4 |
| `scan.rs:252` | replace += with *= in revalidate | missed | 4 |
| `scan.rs:266` | replace match guard e.kind() == std::io::ErrorKind::NotFound with true in revalidate | missed | 4 |
| `tree.rs:185` | replace match guard i > 0 with true in VirtualTree::disambiguate | missed | 4 |
| `tree.rs:185` | replace > with >= in VirtualTree::disambiguate | missed | 4 |
| `tree.rs:194` | delete ! in VirtualTree::disambiguate | timeout | 4 |
| `tree.rs:197` | replace += with *= in VirtualTree::disambiguate | timeout | 4 |

## musefs-format

| File | Caught | Missed | Unviable | Timeout |
|------|-------:|-------:|---------:|--------:|
| `flac.rs` | 150 | 45 | 5 | 0 |
| `mp3.rs` | 180 | 70 | 4 | 0 |
| `mp4.rs` | 204 | 40 | 9 | 4 |
| `ogg/b64.rs` | 24 | 3 | 1 | 0 |
| `ogg/crc.rs` | 23 | 0 | 5 | 0 |
| `ogg/mod.rs` | 102 | 28 | 8 | 0 |
| `ogg/page.rs` | 162 | 13 | 4 | 5 |
| `wav.rs` | 117 | 28 | 3 | 0 |
| **total** | **962** | **227** | **39** | **9** |

### Surviving mutants → phase

| File:line | Mutation | Kind | Phase |
|-----------|----------|------|------:|
| `flac.rs:37` | replace < with == in parse_blocks | missed | 3 |
| `flac.rs:37` | replace < with <= in parse_blocks | missed | 3 |
| `flac.rs:43` | replace + with - in parse_blocks | missed | 3 |
| `flac.rs:43` | replace > with == in parse_blocks | missed | 3 |
| `flac.rs:43` | replace > with >= in parse_blocks | missed | 3 |
| `flac.rs:49` | replace << with >> in parse_blocks | missed | 3 |
| `flac.rs:50` | replace \| with ^ in parse_blocks | missed | 3 |
| `flac.rs:51` | replace \| with ^ in parse_blocks | missed | 3 |
| `flac.rs:99` | replace \| with ^ in push_block_header | missed | 3 |
| `flac.rs:101` | replace >> with << in push_block_header | missed | 3 |
| `flac.rs:155` | replace > with >= in synthesize_layout | missed | 3 |
| `flac.rs:188` | replace < with == in read_vorbis_comments | missed | 3 |
| `flac.rs:188` | replace < with <= in read_vorbis_comments | missed | 3 |
| `flac.rs:188` | replace \|\| with && in read_vorbis_comments | missed | 3 |
| `flac.rs:193` | replace + with - in read_vorbis_comments | missed | 3 |
| `flac.rs:193` | replace > with == in read_vorbis_comments | missed | 3 |
| `flac.rs:193` | replace > with >= in read_vorbis_comments | missed | 3 |
| `flac.rs:199` | replace << with >> in read_vorbis_comments | missed | 3 |
| `flac.rs:200` | replace \| with & in read_vorbis_comments | missed | 3 |
| `flac.rs:200` | replace \| with ^ in read_vorbis_comments | missed | 3 |
| `flac.rs:200` | replace << with >> in read_vorbis_comments | missed | 3 |
| `flac.rs:201` | replace \| with ^ in read_vorbis_comments | missed | 3 |
| `flac.rs:204` | replace > with == in read_vorbis_comments | missed | 3 |
| `flac.rs:204` | replace > with >= in read_vorbis_comments | missed | 3 |
| `flac.rs:219` | replace > with == in read_u32_be | missed | 3 |
| `flac.rs:219` | replace > with >= in read_u32_be | missed | 3 |
| `flac.rs:224` | replace + with * in read_u32_be | missed | 3 |
| `flac.rs:237` | replace > with == in parse_picture_block | missed | 3 |
| `flac.rs:237` | replace > with >= in parse_picture_block | missed | 3 |
| `flac.rs:245` | replace > with == in parse_picture_block | missed | 3 |
| `flac.rs:245` | replace > with >= in parse_picture_block | missed | 3 |
| `flac.rs:261` | replace > with < in parse_picture_block | missed | 3 |
| `flac.rs:277` | replace < with == in read_pictures | missed | 3 |
| `flac.rs:277` | replace < with <= in read_pictures | missed | 3 |
| `flac.rs:277` | replace \|\| with && in read_pictures | missed | 3 |
| `flac.rs:283` | replace + with - in read_pictures | missed | 3 |
| `flac.rs:283` | replace > with == in read_pictures | missed | 3 |
| `flac.rs:283` | replace > with >= in read_pictures | missed | 3 |
| `flac.rs:289` | replace << with >> in read_pictures | missed | 3 |
| `flac.rs:290` | replace \| with & in read_pictures | missed | 3 |
| `flac.rs:290` | replace \| with ^ in read_pictures | missed | 3 |
| `flac.rs:290` | replace << with >> in read_pictures | missed | 3 |
| `flac.rs:291` | replace \| with ^ in read_pictures | missed | 3 |
| `flac.rs:294` | replace > with == in read_pictures | missed | 3 |
| `flac.rs:294` | replace > with >= in read_pictures | missed | 3 |
| `mp3.rs:16` | replace << with >> in synchsafe_decode | missed | 3 |
| `mp3.rs:17` | replace \| with & in synchsafe_decode | missed | 3 |
| `mp3.rs:17` | replace \| with ^ in synchsafe_decode | missed | 3 |
| `mp3.rs:17` | replace << with >> in synchsafe_decode | missed | 3 |
| `mp3.rs:18` | replace \| with ^ in synchsafe_decode | missed | 3 |
| `mp3.rs:19` | replace \| with ^ in synchsafe_decode | missed | 3 |
| `mp3.rs:30` | replace && with \|\| in locate_audio | missed | 3 |
| `mp3.rs:35` | replace += with -= in locate_audio | missed | 3 |
| `mp3.rs:35` | replace += with *= in locate_audio | missed | 3 |
| `mp3.rs:49` | replace + with * in locate_audio | missed | 3 |
| `mp3.rs:51` | replace \|\| with && in locate_audio | missed | 3 |
| `mp3.rs:51` | replace + with * in locate_audio | missed | 3 |
| `mp3.rs:66` | replace >> with << in syncsafe | missed | 3 |
| `mp3.rs:67` | replace >> with << in syncsafe | missed | 3 |
| `mp3.rs:76` | replace > with == in push_frame_header | missed | 3 |
| `mp3.rs:76` | replace > with >= in push_frame_header | missed | 3 |
| `mp3.rs:113` | replace is_id3_text_frame_id -> bool with false | missed | 3 |
| `mp3.rs:114` | replace != with == in is_id3_text_frame_id | missed | 3 |
| `mp3.rs:118` | replace \|\| with && in is_id3_text_frame_id | missed | 3 |
| `mp3.rs:187` | replace match guard is_id3_text_frame_id(key) with false in build_id3v2_segments | missed | 3 |
| `mp3.rs:232` | replace > with >= in build_id3v2_segments | missed | 3 |
| `mp3.rs:275` | replace < with == in id3v2_alloc_safe | missed | 3 |
| `mp3.rs:275` | replace < with <= in id3v2_alloc_safe | missed | 3 |
| `mp3.rs:275` | replace \|\| with && in id3v2_alloc_safe | missed | 3 |
| `mp3.rs:293` | replace \| with & in id3v2_alloc_safe | missed | 3 |
| `mp3.rs:293` | replace \| with ^ in id3v2_alloc_safe | missed | 3 |
| `mp3.rs:293` | replace \| with & in id3v2_alloc_safe | missed | 3 |
| `mp3.rs:293` | replace \| with ^ in id3v2_alloc_safe | missed | 3 |
| `mp3.rs:293` | replace \| with & in id3v2_alloc_safe | missed | 3 |
| `mp3.rs:293` | replace \| with ^ in id3v2_alloc_safe | missed | 3 |
| `mp3.rs:311` | replace + with - in id3v2_alloc_safe | missed | 3 |
| `mp3.rs:320` | replace != with == in id3v2_alloc_safe | missed | 3 |
| `mp3.rs:320` | replace \|\| with && in id3v2_alloc_safe | missed | 3 |
| `mp3.rs:324` | replace + with - in id3v2_alloc_safe | missed | 3 |
| `mp3.rs:324` | replace + with * in id3v2_alloc_safe | missed | 3 |
| `mp3.rs:324` | replace << with >> in id3v2_alloc_safe | missed | 3 |
| `mp3.rs:325` | replace \| with & in id3v2_alloc_safe | missed | 3 |
| `mp3.rs:325` | replace \| with ^ in id3v2_alloc_safe | missed | 3 |
| `mp3.rs:325` | replace + with - in id3v2_alloc_safe | missed | 3 |
| `mp3.rs:325` | replace + with * in id3v2_alloc_safe | missed | 3 |
| `mp3.rs:325` | replace << with >> in id3v2_alloc_safe | missed | 3 |
| `mp3.rs:326` | replace \| with & in id3v2_alloc_safe | missed | 3 |
| `mp3.rs:326` | replace \| with ^ in id3v2_alloc_safe | missed | 3 |
| `mp3.rs:326` | replace + with - in id3v2_alloc_safe | missed | 3 |
| `mp3.rs:326` | replace + with * in id3v2_alloc_safe | missed | 3 |
| `mp3.rs:327` | replace == with != in id3v2_alloc_safe | missed | 3 |
| `mp3.rs:334` | replace + with - in id3v2_alloc_safe | missed | 3 |
| `mp3.rs:334` | replace != with == in id3v2_alloc_safe | missed | 3 |
| `mp3.rs:334` | replace \|\| with && in id3v2_alloc_safe | missed | 3 |
| `mp3.rs:334` | replace + with - in id3v2_alloc_safe | missed | 3 |
| `mp3.rs:334` | replace != with == in id3v2_alloc_safe | missed | 3 |
| `mp3.rs:337` | replace + with - in id3v2_alloc_safe | missed | 3 |
| `mp3.rs:337` | replace + with - in id3v2_alloc_safe | missed | 3 |
| `mp3.rs:337` | replace + with - in id3v2_alloc_safe | missed | 3 |
| `mp3.rs:337` | replace + with - in id3v2_alloc_safe | missed | 3 |
| `mp3.rs:343` | replace + with - in id3v2_alloc_safe | missed | 3 |
| `mp3.rs:343` | replace \| with & in id3v2_alloc_safe | missed | 3 |
| `mp3.rs:343` | replace \| with ^ in id3v2_alloc_safe | missed | 3 |
| `mp3.rs:343` | replace + with - in id3v2_alloc_safe | missed | 3 |
| `mp3.rs:343` | replace \| with & in id3v2_alloc_safe | missed | 3 |
| `mp3.rs:343` | replace \| with ^ in id3v2_alloc_safe | missed | 3 |
| `mp3.rs:343` | replace + with - in id3v2_alloc_safe | missed | 3 |
| `mp3.rs:343` | replace \| with & in id3v2_alloc_safe | missed | 3 |
| `mp3.rs:343` | replace \| with ^ in id3v2_alloc_safe | missed | 3 |
| `mp3.rs:343` | replace + with - in id3v2_alloc_safe | missed | 3 |
| `mp3.rs:346` | replace \|\| with && in id3v2_alloc_safe | missed | 3 |
| `mp3.rs:356` | replace > with == in id3v2_alloc_safe | missed | 3 |
| `mp3.rs:356` | replace > with >= in id3v2_alloc_safe | missed | 3 |
| `mp3.rs:356` | replace - with + in id3v2_alloc_safe | missed | 3 |
| `mp3.rs:362` | replace >= with < in id3v2_alloc_safe | missed | 3 |
| `mp4.rs:35` | replace BoxRef::end -> usize with 0 | timeout | 3 |
| `mp4.rs:35` | replace BoxRef::end -> usize with 1 | timeout | 3 |
| `mp4.rs:35` | replace + with * in BoxRef::end | timeout | 3 |
| `mp4.rs:75` | replace < with <= in box_header | missed | 3 |
| `mp4.rs:105` | replace - with + in read_box | missed | 3 |
| `mp4.rs:105` | replace - with / in read_box | missed | 3 |
| `mp4.rs:276` | replace - with + in read_structure_from | missed | 3 |
| `mp4.rs:279` | delete match arm b"moof" in read_structure_from | missed | 3 |
| `mp4.rs:280` | replace \|= with &= in read_structure_from | missed | 3 |
| `mp4.rs:281` | replace \|= with &= in read_structure_from | missed | 3 |
| `mp4.rs:282` | replace \|= with &= in read_structure_from | missed | 3 |
| `mp4.rs:285` | replace += with *= in read_structure_from | timeout | 3 |
| `mp4.rs:337` | replace < with == in read_freeform | missed | 3 |
| `mp4.rs:337` | replace < with <= in read_freeform | missed | 3 |
| `mp4.rs:337` | replace \|\| with && in read_freeform | missed | 3 |
| `mp4.rs:337` | replace < with == in read_freeform | missed | 3 |
| `mp4.rs:337` | replace < with <= in read_freeform | missed | 3 |
| `mp4.rs:354` | replace >= with < in read_freeform | missed | 3 |
| `mp4.rs:388` | replace < with == in read_tags | missed | 3 |
| `mp4.rs:388` | replace < with <= in read_tags | missed | 3 |
| `mp4.rs:396` | replace && with \|\| in read_tags | missed | 3 |
| `mp4.rs:401` | replace == with != in read_tags | missed | 3 |
| `mp4.rs:401` | replace && with \|\| in read_tags | missed | 3 |
| `mp4.rs:401` | replace >= with < in read_tags | missed | 3 |
| `mp4.rs:428` | replace < with == in read_pictures | missed | 3 |
| `mp4.rs:428` | replace < with <= in read_pictures | missed | 3 |
| `mp4.rs:433` | delete match arm 14 in read_pictures | missed | 3 |
| `mp4.rs:539` | replace == with != in build_udta | missed | 3 |
| `mp4.rs:540` | replace + with - in build_udta | missed | 3 |
| `mp4.rs:540` | replace + with * in build_udta | missed | 3 |
| `mp4.rs:540` | replace + with * in build_udta | missed | 3 |
| `mp4.rs:541` | replace + with * in build_udta | missed | 3 |
| `mp4.rs:566` | replace > with >= in build_udta | missed | 3 |
| `mp4.rs:590` | replace + with - in patch_chunk_offsets | missed | 3 |
| `mp4.rs:590` | replace + with * in patch_chunk_offsets | missed | 3 |
| `mp4.rs:595` | replace < with == in patch_chunk_offsets | missed | 3 |
| `mp4.rs:595` | replace < with <= in patch_chunk_offsets | missed | 3 |
| `mp4.rs:595` | replace \|\| with && in patch_chunk_offsets | missed | 3 |
| `mp4.rs:595` | replace > with == in patch_chunk_offsets | missed | 3 |
| `mp4.rs:595` | replace > with >= in patch_chunk_offsets | missed | 3 |
| `mp4.rs:601` | replace < with == in patch_chunk_offsets | missed | 3 |
| `mp4.rs:601` | replace < with <= in patch_chunk_offsets | missed | 3 |
| `mp4.rs:638` | replace > with == in synthesize_layout | missed | 3 |
| `mp4.rs:638` | replace > with >= in synthesize_layout | missed | 3 |
| `ogg/b64.rs:26` | replace - with + in b64_window | missed | 2 |
| `ogg/b64.rs:26` | replace - with / in b64_window | missed | 2 |
| `ogg/b64.rs:26` | replace / with * in b64_window | missed | 2 |
| `ogg/mod.rs:25` | replace && with \|\| in detect_codec | missed | 2 |
| `ogg/mod.rs:36` | replace < with == in oggflac_following_packets | missed | 2 |
| `ogg/mod.rs:36` | replace < with <= in oggflac_following_packets | missed | 2 |
| `ogg/mod.rs:113` | replace < with == in comment_body | missed | 2 |
| `ogg/mod.rs:113` | replace < with <= in comment_body | missed | 2 |
| `ogg/mod.rs:121` | replace comment_packet_index -> usize with 1 | missed | 2 |
| `ogg/mod.rs:130` | delete ! in comment_packet_index | missed | 2 |
| `ogg/mod.rs:130` | replace && with \|\| in comment_packet_index | missed | 2 |
| `ogg/mod.rs:130` | replace & with \| in comment_packet_index | missed | 2 |
| `ogg/mod.rs:130` | replace & with ^ in comment_packet_index | missed | 2 |
| `ogg/mod.rs:130` | replace == with != in comment_packet_index | missed | 2 |
| `ogg/mod.rs:196` | replace > with == in locate_audio | missed | 2 |
| `ogg/mod.rs:196` | replace > with >= in locate_audio | missed | 2 |
| `ogg/mod.rs:233` | replace += with *= in synthesize_layout | missed | 2 |
| `ogg/mod.rs:235` | replace - with + in synthesize_layout | missed | 2 |
| `ogg/mod.rs:235` | replace - with / in synthesize_layout | missed | 2 |
| `ogg/mod.rs:254` | replace % with + in picture_prefix | missed | 2 |
| `ogg/mod.rs:304` | replace + with * in build_packets_with_art | missed | 2 |
| `ogg/mod.rs:305` | replace + with * in build_packets_with_art | missed | 2 |
| `ogg/mod.rs:306` | replace > with >= in build_packets_with_art | missed | 2 |
| `ogg/mod.rs:409` | replace + with * in oggflac_packets_with_art | missed | 2 |
| `ogg/mod.rs:410` | replace > with == in oggflac_packets_with_art | missed | 2 |
| `ogg/mod.rs:410` | replace > with >= in oggflac_packets_with_art | missed | 2 |
| `ogg/mod.rs:439` | replace < with == in oggflac_packets_with_art | missed | 2 |
| `ogg/mod.rs:439` | replace < with <= in oggflac_packets_with_art | missed | 2 |
| `ogg/mod.rs:455` | replace page_test_support::vorbis_body_empty -> Vec<u8> with vec![] | missed | 2 |
| `ogg/mod.rs:455` | replace page_test_support::vorbis_body_empty -> Vec<u8> with vec![0] | missed | 2 |
| `ogg/mod.rs:455` | replace page_test_support::vorbis_body_empty -> Vec<u8> with vec![1] | missed | 2 |
| `ogg/page.rs:33` | replace > with == in parse_page | missed | 2 |
| `ogg/page.rs:33` | replace > with >= in parse_page | missed | 2 |
| `ogg/page.rs:47` | replace > with == in parse_page | missed | 2 |
| `ogg/page.rs:47` | replace > with >= in parse_page | missed | 2 |
| `ogg/page.rs:93` | replace < with == in lace_packet | timeout | 2 |
| `ogg/page.rs:93` | replace < with <= in lace_packet | timeout | 2 |
| `ogg/page.rs:122` | replace += with *= in lace_packet | missed | 2 |
| `ogg/page.rs:181` | replace == with != in read_packets | missed | 2 |
| `ogg/page.rs:197` | replace < with > in patch_page_header | missed | 2 |
| `ogg/page.rs:256` | replace < with == in lace_chunks_to_segments | timeout | 2 |
| `ogg/page.rs:256` | replace < with <= in lace_chunks_to_segments | timeout | 2 |
| `ogg/page.rs:263` | replace \|= with &= in lace_chunks_to_segments | missed | 2 |
| `ogg/page.rs:265` | delete ! in lace_chunks_to_segments | missed | 2 |
| `ogg/page.rs:266` | replace \|= with &= in lace_chunks_to_segments | missed | 2 |
| `ogg/page.rs:294` | replace += with *= in lace_chunks_to_segments | timeout | 2 |
| `ogg/page.rs:298` | replace - with + in lace_chunks_to_segments | missed | 2 |
| `ogg/page.rs:310` | replace < with <= in copy_payload | missed | 2 |
| `ogg/page.rs:337` | replace < with <= in emit_segments | missed | 2 |
| `wav.rs:24` | replace < with == in riff_wave_start | missed | 3 |
| `wav.rs:24` | replace < with <= in riff_wave_start | missed | 3 |
| `wav.rs:47` | replace + with - in walk_chunks | missed | 3 |
| `wav.rs:49` | replace match guard next <= buf.len() as u64 with true in walk_chunks | missed | 3 |
| `wav.rs:67` | replace == with != in locate_audio | missed | 3 |
| `wav.rs:71` | replace > with < in locate_audio | missed | 3 |
| `wav.rs:119` | delete match arm "artist" in info_fourcc | missed | 3 |
| `wav.rs:120` | delete match arm "album" in info_fourcc | missed | 3 |
| `wav.rs:121` | delete match arm "date" in info_fourcc | missed | 3 |
| `wav.rs:122` | delete match arm "genre" in info_fourcc | missed | 3 |
| `wav.rs:123` | delete match arm "comment" in info_fourcc | missed | 3 |
| `wav.rs:124` | delete match arm "tracknumber" in info_fourcc | missed | 3 |
| `wav.rs:155` | replace % with / in build_info_payload | missed | 3 |
| `wav.rs:155` | replace % with + in build_info_payload | missed | 3 |
| `wav.rs:155` | replace == with != in build_info_payload | missed | 3 |
| `wav.rs:168` | replace % with / in push_inline_chunk | missed | 3 |
| `wav.rs:168` | replace % with + in push_inline_chunk | missed | 3 |
| `wav.rs:186` | replace > with == in synthesize_layout | missed | 3 |
| `wav.rs:186` | replace > with >= in synthesize_layout | missed | 3 |
| `wav.rs:207` | replace % with / in synthesize_layout | missed | 3 |
| `wav.rs:207` | replace % with + in synthesize_layout | missed | 3 |
| `wav.rs:227` | replace > with == in synthesize_layout | missed | 3 |
| `wav.rs:227` | replace > with >= in synthesize_layout | missed | 3 |
| `wav.rs:245` | delete match arm b"IPRD" in info_to_key | missed | 3 |
| `wav.rs:246` | delete match arm b"ICRD" in info_to_key | missed | 3 |
| `wav.rs:248` | delete match arm b"ICMT" in info_to_key | missed | 3 |
| `wav.rs:249` | delete match arm b"ITRK" in info_to_key | missed | 3 |
| `wav.rs:300` | replace && with \|\| in read_tags | missed | 3 |

