# Ogg cover-art duplication fix — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans or subagent-driven-development. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Stop served Ogg files from carrying duplicated cover art by excluding `METADATA_BLOCK_PICTURE` from `ogg::read_tags`, so the cover is stored (and synthesized) only via the art channel.

**Architecture:** One format-layer change. `ogg::read_tags` (`musefs-format/src/ogg/mod.rs`) currently returns every vorbis comment, including the base64 `METADATA_BLOCK_PICTURE` that carries cover art. The scanner stores that as a text tag *and* stores the art from `read_pictures`, so synthesis emits both → two identical pictures. Filtering the picture comment out of `read_tags` restores parity with FLAC (whose art is a separate block, never in its comments).

**Tech Stack:** Rust (`musefs-format`), existing `base64`/`vorbiscomment` test helpers.

**Reference spec:** `docs/superpowers/specs/2026-06-10-ogg-art-duplication-fix-design.md`

---

## Preconditions

- The e2e fixture fix in `musefs-fuse/tests/ogg_read_through.rs` (cover via
  `METADATA_BLOCK_PICTURE`, un-skipping `opus_read_through_preserves_embedded_art`)
  is already in the working tree, currently failing at `out_pics.len() == 2`. This
  plan's fix makes it pass at `== 1`; commit the two together.
- `tagmap` does not map `METADATA_BLOCK_PICTURE`, so the parsed key equals the
  field name (any case); `eq_ignore_ascii_case` matches it reliably.

## File structure

- **Modify:** `musefs-format/src/ogg/mod.rs` — `read_tags` (add the filter) and its
  `#[cfg(test)]` module (add one regression test).
- **Already-staged:** `musefs-fuse/tests/ogg_read_through.rs` — the un-skipping
  fixture fix (committed alongside, not edited here).

---

## Task 1: Failing unit test — read_tags excludes the picture comment

**Files:**
- Modify: `musefs-format/src/ogg/mod.rs` (test module, near `read_tags_opus`)

- [ ] **Step 1: Add the regression test**

Add this test next to the existing `read_tags_opus` in the `#[cfg(test)] mod`:

```rust
#[test]
fn read_tags_excludes_metadata_block_picture() {
    // A METADATA_BLOCK_PICTURE comment whose value is a base64 FLAC picture block
    // carrying a 1-byte image, plus one ordinary text tag.
    let mut block = Vec::new();
    block.extend_from_slice(&3u32.to_be_bytes()); // picture type: front cover
    block.extend_from_slice(&9u32.to_be_bytes());
    block.extend_from_slice(b"image/png");
    block.extend_from_slice(&0u32.to_be_bytes()); // description length
    block.extend_from_slice(&1u32.to_be_bytes()); // width
    block.extend_from_slice(&1u32.to_be_bytes()); // height
    block.extend_from_slice(&8u32.to_be_bytes()); // depth
    block.extend_from_slice(&0u32.to_be_bytes()); // colors used
    block.extend_from_slice(&1u32.to_be_bytes()); // image length
    block.push(0xAB);
    let pic_value = base64::engine::general_purpose::STANDARD.encode(&block);

    let body = crate::vorbiscomment::build(&[
        crate::input::TagInput::new("title", "Sun"),
        crate::input::TagInput::new("METADATA_BLOCK_PICTURE", &pic_value),
    ])
    .unwrap();
    let mut tags_pkt = b"OpusTags".to_vec();
    tags_pkt.extend_from_slice(&body);
    let head = b"OpusHead\x01\x02\x38\x01\x80\xbb\x00\x00\x00\x00\x00".to_vec();
    let (mut data, _) = crate::ogg::page::build_header(7, &[&head, &tags_pkt]);
    let (audio, _) = crate::ogg::page::lace_packet(7, 2, false, 960, &[0u8; 50]);
    data.extend_from_slice(&audio);

    // read_tags returns only the text tag — the picture comment is excluded...
    let tags = read_tags(&data).unwrap();
    assert_eq!(tags, vec![("title".to_string(), "Sun".to_string())]);
    // ...while read_pictures still finds the embedded art.
    let pics = read_pictures(&data).unwrap();
    assert_eq!(pics.len(), 1);
    assert_eq!(pics[0].data, vec![0xAB]);
}
```

The test module already imports the crate internals used here (`read_tags`,
`read_pictures`, `crate::vorbiscomment`, `crate::input::TagInput`,
`crate::ogg::page::*`); `base64::engine` is reachable because `base64` is a
`musefs-format` dependency (used by `read_pictures`).

- [ ] **Step 2: Run it — expect FAIL**

Run: `cargo test -p musefs-format ogg::mod::tests::read_tags_excludes_metadata_block_picture -- --exact` (or `cargo test -p musefs-format read_tags_excludes_metadata_block_picture`)
Expected: FAIL on the first assert — `read_tags` currently returns
`[("title","Sun"), ("METADATA_BLOCK_PICTURE", "<base64>")]`, so `tags != [("title","Sun")]`.

---

## Task 2: Implement the filter in read_tags

**Files:**
- Modify: `musefs-format/src/ogg/mod.rs` (`read_tags`)

- [ ] **Step 1: Add the exclusion**

Replace the body of `read_tags`:

```rust
pub fn read_tags(data: &[u8]) -> Result<Vec<(String, String)>> {
    let header = read_header(data)?;
    let idx = comment_packet_index(&header);
    if idx == 0 {
        return Ok(Vec::new()); // no comment packet present
    }
    let body = comment_body(header.codec, &header.packets[idx])?;
    let mut tags = crate::vorbiscomment::parse(body)?;
    // Cover art rides in the comment as a base64 METADATA_BLOCK_PICTURE entry, but
    // it has its own channel (read_pictures). Excluding it keeps read_tags
    // text-only and prevents the art being stored — and re-synthesized — twice.
    tags.retain(|(field, _)| !field.eq_ignore_ascii_case("METADATA_BLOCK_PICTURE"));
    Ok(tags)
}
```

- [ ] **Step 2: Run the unit test — expect PASS**

Run: `cargo test -p musefs-format read_tags_excludes_metadata_block_picture`
Expected: PASS. Also run `cargo test -p musefs-format read_tags_opus` — still PASS
(no picture comment, unaffected).

- [ ] **Step 3: Lint/format the crate**

Run: `cargo clippy -p musefs-format --all-targets -- -D warnings && cargo fmt -p musefs-format -- --check`
Expected: clean.

---

## Task 3: Confirm the e2e art test now passes, then commit

**Files:** none new (commits the staged fixture fix + this fix together)

- [ ] **Step 1: Run the un-skipped opus art e2e test**

Run: `cargo test -p musefs-fuse --test ogg_read_through opus_read_through_preserves_embedded_art -- --ignored`
Expected: PASS — the synthesized file now carries exactly one picture matching the
source (`out_pics.len() == 1`).

- [ ] **Step 2: Run the whole ogg_read_through module**

Run: `cargo test -p musefs-fuse --test ogg_read_through -- --ignored`
Expected: all pass, no skips for the opus art test.

- [ ] **Step 3: Commit**

```bash
git add musefs-format/src/ogg/mod.rs musefs-fuse/tests/ogg_read_through.rs \
        docs/superpowers/specs/2026-06-10-ogg-art-duplication-fix-design.md \
        docs/superpowers/plans/2026-06-10-ogg-art-duplication-fix.md
git commit -m "fix(ogg): stop duplicating cover art on synthesis"
```

(The pre-commit hook runs fmt + clippy `-D warnings` + the full workspace suite +
ruff. The new unit test runs there; the `#[ignore]` e2e ones do not, but were
verified above.)

---

## Task 4: Full verification

**Files:** none

- [ ] **Step 1: Workspace gates (pre-commit parity)**

Run: `cargo fmt --all --check && cargo clippy --all-targets -- -D warnings && cargo test --workspace`
Expected: clean fmt, no clippy warnings, all tests pass.

- [ ] **Step 2: Full FUSE e2e (the two affected modules at minimum)**

Run: `cargo test -p musefs-fuse -- --ignored`
Expected: all pass — `read_consistency` (4) and `ogg_read_through` (incl. the now-real opus art test) green.

- [ ] **Step 3: Fuzz crate untouched**

The format-layer change is internal to `read_tags` (no signature change), so the
out-of-workspace `fuzz/` crate is unaffected; no action required.

---

## Self-review (coverage)

- **Root cause (METADATA_BLOCK_PICTURE in read_tags)** → Task 2 filter.
- **Regression test (read_tags excludes it; read_pictures still finds it)** → Task 1.
- **e2e duplication gone (one served picture)** → Task 3.
- **No other format / scanner / schema change** → only `read_tags` touched.
- **Scope: COVERART untouched, vorbiscomment::parse untouched** → honored (filter is
  in `read_tags` only).
