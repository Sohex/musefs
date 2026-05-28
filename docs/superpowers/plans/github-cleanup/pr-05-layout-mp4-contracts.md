# PR 5 Layout And MP4 Contracts Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Validate `RegionLayout` producer invariants at synthesis boundaries and document/test MP4 first-cover behavior.

**Architecture:** Keep `RegionLayout` simple, but add a checked validation method and call it before returning synthesized layouts from each format producer. MP4 behavior remains unchanged: the first art input wins.

**Tech Stack:** Rust, musefs-format, README documentation.

---

### Task 1: Add RegionLayout Validation

**Files:**
- Modify: `musefs-format/src/layout.rs`
- Modify: `musefs-format/src/lib.rs`
- Test: `musefs-format/tests/layout.rs`

- [ ] **Step 1: Add validation type and tests**

Add `LayoutError::{EmptySegment, TotalOverflow}` and
`RegionLayout::validate() -> Result<(), LayoutError>`.

Tests must cover:
- empty inline segment fails;
- zero-length backing segment fails;
- `u64::MAX + 1` total length fails;
- normal inline plus backing layout passes.

- [ ] **Step 2: Export the error**

Export from `musefs-format/src/lib.rs`:

```rust
pub use layout::{LayoutError, RegionLayout, Segment};
```

### Task 2: Validate At Synthesis Boundaries

**Files:**
- Modify: `musefs-format/src/flac.rs`
- Modify: `musefs-format/src/mp3.rs`
- Modify: `musefs-format/src/mp4.rs`
- Modify: `musefs-format/src/ogg/mod.rs`
- Modify: `musefs-format/src/wav.rs`

- [ ] **Step 1: Add helper**

Add a small helper in `layout.rs` or call directly:

```rust
let layout = RegionLayout::new(segments);
layout.validate().map_err(|_| FormatError::InvalidLayout)?;
Ok(layout)
```

Add `FormatError::InvalidLayout` if no existing format error fits.

- [ ] **Step 2: Use helper in every synthesis producer**

Apply the validation before returning from all format `synthesize_layout`
functions. Do not validate ad hoc test layouts unless they are exercising the
validation API directly.

- [ ] **Step 3: Verify validation is wired**

Search:

```bash
rg "RegionLayout::new\\(segments\\)" musefs-format/src
```

Expected: no synthesis producer returns `RegionLayout::new(segments)` without
validation.

### Task 3: Document And Test MP4 First-Cover Limitation

**Files:**
- Modify: `README.md`
- Test: `musefs-format/tests/mp4_oracle.rs`

- [ ] **Step 1: Add README limitation**

Document that MP4/M4A synthesis embeds only the first cover-art input by
`track_art.ordinal`; later art inputs are ignored.

- [ ] **Step 2: Add regression test**

In `mp4_oracle.rs`, synthesize with `[art1]` and `[art1, art2]` and assert the
layouts are identical. This locks silent first-art behavior.

- [ ] **Step 3: Verify**

Run:

```bash
cargo test -p musefs-format --test layout --test mp4_oracle
cargo test -p musefs-format
```

- [ ] **Step 4: Commit**

```bash
git add README.md musefs-format/src musefs-format/tests/layout.rs musefs-format/tests/mp4_oracle.rs
git commit -m "feat(format): validate layouts and document MP4 first-art behavior

Closes #15
Closes #17"
```
