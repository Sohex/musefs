# PR 4 Ogg Hardening And Cache Accounting Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Guard full Ogg cover-art comment lengths, document/test the Ogg payload invariant, and make resident Ogg page indexes visible to cache budgeting.

**Architecture:** Keep format-level Ogg length validation in `musefs-format`. For cache accounting, use a bounded policy tied to actual page-index residency, not only a rough pre-resolve estimate.

**Tech Stack:** Rust, musefs-format Ogg synthesis, musefs-core header cache, Ogg page index.

---

### Task 1: Guard Full VorbisComment Art Value Length

**Files:**
- Modify: `musefs-format/src/ogg/mod.rs`

- [ ] **Step 1: Add failing overflow test**

Add a unit test in `musefs-format/src/ogg/mod.rs` that builds an Opus or Vorbis
header with art whose full `METADATA_BLOCK_PICTURE=` value exceeds `u32::MAX`
once key, prefix, and base64 image length are included.

- [ ] **Step 2: Implement full length guard**

Before calling `comment_packet_chunks`, compute:

```rust
let value_len = METADATA_BLOCK_PICTURE_KEY.len() as u64
    + b64_len(picture_prefix(a.meta).len() as u64)
    + b64_len(a.meta.data_len);
if value_len > u32::MAX as u64 {
    return Err(FormatError::TooLarge);
}
```

Define `METADATA_BLOCK_PICTURE_KEY` once and reuse it in both the guard and
comment emission.

### Task 2: Account Ogg Page Index Residency

**Files:**
- Modify: `musefs-core/src/ogg_index.rs`
- Modify: `musefs-core/src/reader.rs`
- Test: `musefs-core/tests/reader.rs` or `musefs-core/src/reader.rs` tests

- [ ] **Step 1: Add exact index-size API**

Add a method on `OggPageIndex`:

```rust
impl OggPageIndex {
    pub fn estimated_heap_bytes(&self) -> u64 {
        let page_structs = self.pages.len() as u64 * std::mem::size_of::<IndexedPage>() as u64;
        let header_bytes = self
            .pages
            .iter()
            .map(|p| p.header.len() as u64)
            .sum::<u64>();
        page_structs + header_bytes
    }
}
```

Use the actual local field names in `ogg_index.rs`.

- [ ] **Step 2: Make cache charge include actual resident index bytes**

When `resolved.ogg_index.get_or_try_init(...)` builds the index in `reader.rs`,
the owning cache entry must be charged for `estimated_heap_bytes()`. Acceptable
implementation choices:
- store an `AtomicU64` `resident_extra_bytes` in `ResolvedFile` and include it in
  cache accounting/eviction checks; or
- evict/reinsert the cache entry with the larger charge immediately after index
  construction.

Do not rely only on `audio_length / 8192`; page-dense files must not escape the
budget.

- [ ] **Step 3: Add page-dense regression test**

Create an Ogg fixture with many small pages, resolve it, trigger index creation,
and assert the cache-visible charge grows by at least
`index.estimated_heap_bytes()`. If exact cache totals are private, add a
`#[cfg(test)]` accessor rather than weakening the assertion.

### Task 3: Document And Test Ogg Invariant

**Files:**
- Create: `docs/OGG_INVARIANT.md`
- Test: relevant Ogg read/proptest fixture

- [ ] **Step 1: Add docs**

Create `docs/OGG_INVARIANT.md` stating:

```markdown
# Ogg Invariant

Ogg synthesis preserves original packet payload bytes. It may intentionally
change Ogg page sequence numbers and CRC fields because resized metadata changes
page numbering.
```

Include references to the Ogg property tests and interop tests.

- [ ] **Step 2: Add or verify payload-specific test**

Ensure a test compares Ogg packet payload bytes, not entire Ogg page bytes. Page
headers are allowed to differ.

- [ ] **Step 3: Verify**

Run:

```bash
cargo test -p musefs-format --features fuzzing
cargo test -p musefs-core
```

- [ ] **Step 4: Commit**

```bash
git add docs/OGG_INVARIANT.md musefs-format/src/ogg/mod.rs musefs-core/src/ogg_index.rs musefs-core/src/reader.rs
git commit -m "fix(ogg): harden art length and account resident page indexes

Closes #9
Closes #10
Closes #16"
```
