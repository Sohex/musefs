# Skip Oversized M4A `ilst` Payloads Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop the M4A scan path from cloning oversized `covr` (cover art) and `----` (binary freeform) `ilst` payloads before the scanner's per-payload caps are applied.

**Architecture:** Thread an explicit `usize` byte budget into `mp4::read_pictures` and `mp4::read_binary_tags`. Each function skips any `data` payload whose body exceeds the budget *before* the `dp[8..].to_vec()` copy. `musefs-core` passes its existing `MAX_ART_BYTES` / `MAX_BINARY_TAG_BYTES` constants at both probe call sites; the format layer stays policy-free. `read_tags` (text) is untouched — deferred to #267.

**Tech Stack:** Rust workspace (`musefs-db` → `musefs-format` → `musefs-core`), `cargo test`, `cargo clippy --all-targets`, `cargo +nightly fuzz` (out-of-workspace fuzz crate).

**Reference spec:** `docs/superpowers/specs/2026-06-12-mp4-oversize-ilst-skip-design.md`

---

## Critical constraints (read before starting)

- **Every commit must leave the whole workspace green.** The pre-commit hook runs the full workspace test suite, clippy `-D warnings`, and fmt; a red commit is rejected. Because changing a function signature breaks all of its callers' compilation at once, each task changes the signature, the guard, **and every caller** in a single commit. Do not split a signature change from its caller updates across commits.
- **Use Serena symbolic tools** (`find_symbol`, `replace_symbol_body`, `replace_content`) for code reads/edits per project convention; built-in Read/Grep are for discovery only.
- **Locate by symbol, not line number.** Line numbers in this plan are approximate and drift.
- **The `fuzz/` crate is outside the workspace.** A signature change to `read_pictures` breaks `fuzz/fuzz_targets/mp4.rs` silently — the workspace build will not catch it. Verify with `cargo +nightly fuzz build mp4`.

## File map

| File | Change |
| ---- | ------ |
| `musefs-format/src/mp4.rs` | Add `max_art_bytes` to `read_pictures`, `max_binary_tag_bytes` to `read_binary_tags`; add skip guards; new unit tests; update inline test callers + `fuzz_check.rs` is separate (below) |
| `musefs-format/src/fuzz_check.rs` | Update the one `read_pictures` call site (pass `usize::MAX`) |
| `musefs-format/tests/proptest_mp4.rs` | Update the one `read_binary_tags` call site (pass `usize::MAX`) |
| `musefs-core/src/scan.rs` | Pass `MAX_ART_BYTES` / `MAX_BINARY_TAG_BYTES` at both probe sites; add `mp4_with_covr` test helper + two core tests |
| `fuzz/fuzz_targets/mp4.rs` | Update the one `read_pictures` call site (pass `usize::MAX`); out-of-workspace |

---

## Task 1: `read_pictures` byte budget (art)

**Files:**
- Modify: `musefs-format/src/mp4.rs` — `read_pictures` (~line 431) + inline tests
- Modify: `musefs-format/src/fuzz_check.rs` (~line 448)
- Modify: `musefs-core/src/scan.rs` — probe sites (~lines 227, 310) + new test helper/tests
- Modify: `fuzz/fuzz_targets/mp4.rs` (~line 13)

- [ ] **Step 1: Write the failing format-layer tests**

Add these two tests to the `#[cfg(test)] mod tests` block in `musefs-format/src/mp4.rs` (near the other `read_pictures_*` tests, ~line 1889). They use the existing `bx`, `data_atom`, and `mp4_with_ilst` helpers and a tiny budget so no large buffers are built:

```rust
#[test]
fn read_pictures_skips_art_over_budget() {
    // covr image body of 5 bytes with a budget of 4: skipped before any copy.
    let over = vec![0xFFu8; 5];
    let buf = mp4_with_ilst(&bx(b"covr", &data_atom(13, &over)), true);
    assert!(read_pictures(&buf, 4).is_empty());
}

#[test]
fn read_pictures_accepts_art_exactly_at_budget() {
    // Boundary: image body length == budget is still extracted.
    let exact = vec![0xFFu8; 4];
    let buf = mp4_with_ilst(&bx(b"covr", &data_atom(13, &exact)), true);
    let pics = read_pictures(&buf, 4);
    assert_eq!(pics.len(), 1);
    assert_eq!(pics[0].data, exact);
}
```

- [ ] **Step 2: Run the tests to verify they fail to compile**

Run: `cargo test -p musefs-format read_pictures_skips_art_over_budget 2>&1 | head -20`
Expected: FAIL — compile error, "this function takes 1 argument but 2 arguments were supplied" (the budget parameter does not exist yet). This is the red state; the signature change in Step 3 plus caller fixes in Step 4 make it green.

- [ ] **Step 3: Add the budget parameter and skip guard to `read_pictures`**

Use `replace_symbol_body` on `read_pictures` in `musefs-format/src/mp4.rs`. The signature gains `max_art_bytes: usize` and a guard is added immediately after the `if dp.len() < 8 { continue; }` check (so `dp.len() - 8` cannot underflow):

```rust
/// Lenient: returns empty / skips any malformed atom and never errors — this only
/// seeds cover art from existing files, so a missing or garbled picture must simply be absent.
/// Every `data` child of every `covr` atom yields one picture (the iTunes
/// multiple-artwork convention); non-`data` children are skipped.
///
/// `max_art_bytes` caps each image body: a `data` payload whose image bytes
/// (after the 8-byte `[type][locale]` header) exceed it is skipped before any
/// copy, so an oversized `covr` in a large `moov` is never materialized.
pub fn read_pictures(buf: &[u8], max_art_bytes: usize) -> Vec<EmbeddedPicture> {
    let Some((start, len)) = ilst_region(buf) else {
        return Vec::new();
    };
    let ilst = &buf[start..start + len];
    let mut out = Vec::new();
    for atom in child_boxes(ilst).unwrap_or_default() {
        if &atom.kind != b"covr" {
            continue;
        }
        let inner = atom.payload(ilst);
        for data in child_boxes(inner).unwrap_or_default() {
            if &data.kind != b"data" {
                continue;
            }
            let dp = data.payload(inner);
            if dp.len() < 8 {
                continue;
            }
            if dp.len() - 8 > max_art_bytes {
                continue;
            }
            let mime = match u32::from_be_bytes([dp[0], dp[1], dp[2], dp[3]]) {
                13 => "image/jpeg",
                14 => "image/png",
                _ => continue,
            };
            out.push(EmbeddedPicture {
                mime: mime.to_string(),
                picture_type: PictureType::new(3).expect("3 is in range"),
                description: String::new(),
                width: 0,
                height: 0,
                data: dp[8..].to_vec(),
            });
        }
    }
    out
}
```

- [ ] **Step 4: Update every `read_pictures` caller to pass a budget**

The crate (and downstream) will not compile until all callers are fixed. Find them with: `rg -n 'read_pictures\(' musefs-format/src/mp4.rs musefs-format/src/fuzz_check.rs musefs-core/src/scan.rs fuzz/fuzz_targets/mp4.rs`.

Production sites — pass the real cap (`musefs-core/src/scan.rs`):
- `probe_full` arm (~line 227): `pictures: mp4::read_pictures(bytes),` → `pictures: mp4::read_pictures(bytes, MAX_ART_BYTES),`
- `probe_file` arm (~line 310): `pictures: mp4::read_pictures(&scan.moov),` → `pictures: mp4::read_pictures(&scan.moov, MAX_ART_BYTES),`

Extract-everything sites — pass `usize::MAX`:
- `musefs-format/src/fuzz_check.rs` (~line 448): `crate::mp4::read_pictures(&f)` → `crate::mp4::read_pictures(&f, usize::MAX)`
- `fuzz/fuzz_targets/mp4.rs` (~line 13): `mp4::read_pictures(data)` → `mp4::read_pictures(data, usize::MAX)`
- Inline `#[cfg(test)]` callers in `musefs-format/src/mp4.rs`: there are ten existing `read_pictures(...)` calls (e.g. `read_pictures(&buf)`, `read_pictures(&[])`, `read_pictures(garbage)`). Add `, usize::MAX` to each — `read_pictures(&buf, usize::MAX)`, `read_pictures(&[], usize::MAX)`, etc. **Do not** touch the two new tests from Step 1 (they already pass a budget). Grep `read_pictures\(` within `mp4.rs` and update every call that still passes a single argument.

- [ ] **Step 5: Add the core-level seek-path test (and its `mp4_with_covr` helper)**

This proves the real scan path (`probe_file`, which loads the up-to-256 MiB `moov` via `read_structure_from`) passes `MAX_ART_BYTES` — a format-layer test alone would not catch a `usize::MAX` mistakenly wired at the scan site.

First add a test helper to the `#[cfg(test)]` module in `musefs-core/src/scan.rs`, next to `mp4_with_binary_freeform` (~line 1080). It mirrors that helper but emits a `covr` atom; the single `soun` track satisfies `validate_moov`:

```rust
fn mp4_with_covr(type_code: u32, value: &[u8]) -> Vec<u8> {
    fn bx(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = u32::try_from(8 + body.len())
            .unwrap()
            .to_be_bytes()
            .to_vec();
        v.extend_from_slice(kind);
        v.extend_from_slice(body);
        v
    }
    // mdia/hdlr with handler type `soun` (validate_moov requires one audio track).
    let mut hdlr_body = vec![0u8; 8];
    hdlr_body.extend_from_slice(b"soun");
    hdlr_body.extend_from_slice(&[0u8; 12]);
    let trak = bx(b"trak", &bx(b"mdia", &bx(b"hdlr", &hdlr_body)));

    // udta/meta/ilst with one covr/data atom: data body is [type][locale][value].
    let mut data_body = type_code.to_be_bytes().to_vec();
    data_body.extend_from_slice(&0u32.to_be_bytes()); // locale
    data_body.extend_from_slice(value);
    let ilst = bx(b"ilst", &bx(b"covr", &bx(b"data", &data_body)));
    let mut meta = 0u32.to_be_bytes().to_vec();
    meta.extend(bx(b"hdlr", &[0u8; 25]));
    meta.extend(ilst);
    let udta = bx(b"udta", &bx(b"meta", &meta));

    let moov = bx(b"moov", &[trak, udta].concat());
    [bx(b"ftyp", b"M4A "), moov, bx(b"mdat", b"AUDIODATA")].concat()
}
```

Then add the test to the same module (near `probe_full_surfaces_mp4_binary_freeform`, ~line 1117):

```rust
#[test]
fn probe_file_skips_oversized_mp4_covr() {
    // A covr image body larger than MAX_ART_BYTES must be skipped at extraction
    // (never copied) by the real seek-path scanner, so it is absent from Probed.
    let oversized = vec![0xFFu8; MAX_ART_BYTES + 1];
    let bytes = mp4_with_covr(13, &oversized); // type 13 = JPEG
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("oversized_art.m4a");
    std::fs::write(&path, &bytes).unwrap();
    let probed = probe_file(&path, bytes.len() as u64, 0)
        .unwrap()
        .expect("m4a should probe");
    assert_eq!(probed.format, Format::M4a);
    assert!(
        probed.pictures.is_empty(),
        "oversized covr must be skipped at extraction, not materialized"
    );
}
```

Note: `probe_file`'s `.m4a` arm ignores the `window` argument (it uses the seek reader), so `0` is fine. `MAX_ART_BYTES` is in scope via `use super::*` in the test module (the existing constant-equality test already references it).

- [ ] **Step 6: Run the full workspace tests and clippy**

Run: `cargo test -p musefs-format read_pictures && cargo test -p musefs-core probe_file_skips_oversized_mp4_covr`
Expected: PASS (new tests green).

Run: `cargo test`
Expected: PASS (whole workspace green — confirms every caller was updated).

Run: `cargo clippy --all-targets 2>&1 | tail -5`
Expected: no warnings (the `-D warnings` gate will reject any).

Run: `cargo fmt --all` then `cargo fmt --all --check`
Expected: clean (the pre-commit hook gates on fmt; format before committing).

- [ ] **Step 7: Verify the out-of-workspace fuzz target builds**

Run: `cargo +nightly fuzz build mp4 2>&1 | tail -5`
Expected: builds successfully (confirms `fuzz/fuzz_targets/mp4.rs` was updated for the new signature).

- [ ] **Step 8: Commit**

```bash
git add musefs-format/src/mp4.rs musefs-format/src/fuzz_check.rs musefs-core/src/scan.rs fuzz/fuzz_targets/mp4.rs
git commit -m "$(cat <<'EOF'
fix(scan): cap M4A cover-art extraction before materializing (#297)

read_pictures now takes a byte budget and skips covr data payloads larger
than it before the to_vec copy, so a crafted moov can no longer force a
throwaway allocation up to the 256 MiB metadata cap. musefs-core passes
MAX_ART_BYTES at both probe sites; a seek-path scan test asserts an
oversized covr is absent from the probe result.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: `read_binary_tags` byte budget (binary freeform)

**Files:**
- Modify: `musefs-format/src/mp4.rs` — `read_binary_tags` (~line 472) + inline tests
- Modify: `musefs-format/tests/proptest_mp4.rs` (~line 94)
- Modify: `musefs-core/src/scan.rs` — probe sites (~lines 228, 311) + new core test

- [ ] **Step 1: Write the failing format-layer tests**

Add these two tests to the `#[cfg(test)] mod tests` block in `musefs-format/src/mp4.rs` (near the other `read_binary_tags_*` tests, ~line 2199). They use the existing `freeform_atom_typed` and `moov_with_ilst` helpers and a tiny budget:

```rust
#[test]
fn read_binary_tags_skips_payload_over_budget() {
    // A `----` value of 5 bytes with a budget of 4: skipped before any copy.
    let over = vec![0xABu8; 5];
    let atom = freeform_atom_typed("com.serato.dj", "analysis", 0, &over);
    let moov = moov_with_ilst(&atom);
    assert!(read_binary_tags(&moov, 4).is_empty());
}

#[test]
fn read_binary_tags_accepts_payload_exactly_at_budget() {
    // Boundary: value length == budget is still extracted.
    let exact = vec![0xABu8; 4];
    let atom = freeform_atom_typed("com.serato.dj", "analysis", 0, &exact);
    let moov = moov_with_ilst(&atom);
    let tags = read_binary_tags(&moov, 4);
    assert_eq!(tags.len(), 1);
    assert_eq!(tags[0].payload, exact);
}
```

- [ ] **Step 2: Run the tests to verify they fail to compile**

Run: `cargo test -p musefs-format read_binary_tags_skips_payload_over_budget 2>&1 | head -20`
Expected: FAIL — compile error, "this function takes 1 argument but 2 arguments were supplied".

- [ ] **Step 3: Add the budget parameter and skip guard to `read_binary_tags`**

Use `replace_symbol_body` on `read_binary_tags` in `musefs-format/src/mp4.rs`. The signature gains `max_binary_tag_bytes: usize`; the guard goes immediately after the `if dp.len() < 8 { continue; }` check, so an oversized payload is skipped before the `name`/`mean` parsing and the copy:

```rust
/// Extract opaque (non-text) MP4 `----` freeform atoms for binary-tag passthrough.
/// One `EmbeddedBinaryTag` per `----` atom whose first `data` sub-box is
/// binary-typed (type code != 1): key `----:<mean>:<name>`, payload the `data`
/// value bytes (after the 8-byte `[type][locale]` header). Text freeform atoms
/// (type 1) are handled by `read_tags`, so the two paths never double-store.
/// Lenient: malformed atoms are skipped. Only the first `data` sub-box is read
/// (multi-value freeform is rare; mirrors `read_freeform`).
///
/// `max_binary_tag_bytes` caps each value: a `data` payload whose value bytes
/// (after the 8-byte `[type][locale]` header) exceed it is skipped before any
/// copy, so an oversized `----` in a large `moov` is never materialized.
pub fn read_binary_tags(buf: &[u8], max_binary_tag_bytes: usize) -> Vec<EmbeddedBinaryTag> {
    let Some((start, len)) = ilst_region(buf) else {
        return Vec::new();
    };
    let ilst = &buf[start..start + len];
    let mut out = Vec::new();
    for atom in child_boxes(ilst).unwrap_or_default() {
        if &atom.kind != b"----" {
            continue;
        }
        let inner = atom.payload(ilst);
        let Ok(Some(data)) = find_box(inner, b"data") else {
            continue;
        };
        let dp = data.payload(inner);
        if dp.len() < 8 {
            continue;
        }
        if dp.len() - 8 > max_binary_tag_bytes {
            continue;
        }
        // `data` body is `[type: u32][locale: u32][value]`; type 1 == UTF-8 text,
        // which is the text path's job. Everything else is opaque binary.
        let type_code = u32::from_be_bytes([dp[0], dp[1], dp[2], dp[3]]);
        if type_code == 1 {
            continue;
        }
        // name/mean payloads carry a 4-byte FullBox prefix; default mean to iTunes.
        let Some(name) = find_box(inner, b"name").ok().flatten().and_then(|n| {
            let p = n.payload(inner);
            (p.len() >= 4)
                .then(|| std::str::from_utf8(&p[4..]).ok())
                .flatten()
        }) else {
            continue;
        };
        let mean = find_box(inner, b"mean")
            .ok()
            .flatten()
            .map_or("com.apple.iTunes", |m| {
                let p = m.payload(inner);
                if p.len() >= 4 {
                    std::str::from_utf8(&p[4..]).unwrap_or("com.apple.iTunes")
                } else {
                    "com.apple.iTunes"
                }
            });
        out.push(EmbeddedBinaryTag {
            key: format!("----:{mean}:{name}"),
            payload: dp[8..].to_vec(),
        });
    }
    out
}
```

- [ ] **Step 4: Update every `read_binary_tags` caller to pass a budget**

Find them with: `rg -n 'read_binary_tags\(' musefs-format/src/mp4.rs musefs-format/tests/proptest_mp4.rs musefs-core/src/scan.rs`. (`fuzz/fuzz_targets/mp4.rs` does **not** call `read_binary_tags` — no change there.)

Production sites — pass the real cap (`musefs-core/src/scan.rs`):
- `probe_full` arm (~line 228): `binary_tags: mp4::read_binary_tags(bytes),` → `binary_tags: mp4::read_binary_tags(bytes, MAX_BINARY_TAG_BYTES),`
- `probe_file` arm (~line 311): `binary_tags: mp4::read_binary_tags(&scan.moov),` → `binary_tags: mp4::read_binary_tags(&scan.moov, MAX_BINARY_TAG_BYTES),`

Extract-everything sites — pass `usize::MAX`:
- `musefs-format/tests/proptest_mp4.rs` (~line 94): `mp4::read_binary_tags(&served)` → `mp4::read_binary_tags(&served, usize::MAX)`
- Inline `#[cfg(test)]` callers in `musefs-format/src/mp4.rs`: there are four existing `read_binary_tags(...)` single-argument calls (e.g. `read_binary_tags(&moov)`). Add `, usize::MAX` to each. **Do not** touch the two new tests from Step 1. Grep `read_binary_tags\(` within `mp4.rs` and update every call that still passes a single argument.

- [ ] **Step 5: Add the core-level seek-path test**

Add to the `#[cfg(test)]` module in `musefs-core/src/scan.rs`, next to the Task 1 core test. It reuses the existing `mp4_with_binary_freeform` helper:

```rust
#[test]
fn probe_file_skips_oversized_mp4_binary_freeform() {
    // A `----` value larger than MAX_BINARY_TAG_BYTES must be skipped at
    // extraction by the real seek-path scanner, so it is absent from Probed.
    let oversized = vec![0xABu8; MAX_BINARY_TAG_BYTES + 1];
    let bytes = mp4_with_binary_freeform("com.serato.dj", "analysis", &oversized);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("oversized_bin.m4a");
    std::fs::write(&path, &bytes).unwrap();
    let probed = probe_file(&path, bytes.len() as u64, 0)
        .unwrap()
        .expect("m4a should probe");
    assert_eq!(probed.format, Format::M4a);
    assert!(
        probed.binary_tags.is_empty(),
        "oversized binary freeform must be skipped at extraction, not materialized"
    );
}
```

- [ ] **Step 6: Run the full workspace tests and clippy**

Run: `cargo test -p musefs-format read_binary_tags && cargo test -p musefs-core probe_file_skips_oversized_mp4_binary_freeform`
Expected: PASS.

Run: `cargo test`
Expected: PASS (whole workspace green).

Run: `cargo clippy --all-targets 2>&1 | tail -5`
Expected: no warnings.

Run: `cargo fmt --all` then `cargo fmt --all --check`
Expected: clean (format before committing).

- [ ] **Step 7: Commit**

```bash
git add musefs-format/src/mp4.rs musefs-format/tests/proptest_mp4.rs musefs-core/src/scan.rs
git commit -m "$(cat <<'EOF'
fix(scan): cap M4A binary-freeform extraction before materializing (#297)

read_binary_tags now takes a byte budget and skips ---- data payloads
larger than it before the to_vec copy. musefs-core passes
MAX_BINARY_TAG_BYTES at both probe sites; a seek-path scan test asserts an
oversized binary freeform tag is absent from the probe result.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Out of scope (do not implement)

- Text/`read_tags` payload caps (#267) — `read_tags` is untouched.
- Reducing the 256 MiB `moov` cap (`read_structure_from` already enforces it).
- Budgets for non-MP4 formats (their `read_pictures`/`read_binary_tags` read a small bounded prefix and rely on the `ingest` backstop, which is left unchanged).
- Removing `ingest`'s `<= MAX_ART_BYTES` / `<= MAX_BINARY_TAG_BYTES` filters — they remain the universal backstop for the other formats.
- `docs/M4A.md` — it does not document the per-payload byte caps, so no doc change is required.
