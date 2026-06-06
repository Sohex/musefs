# Integer Cast Convention Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Establish and enforce an integer-conversion convention (spec:
`docs/superpowers/specs/2026-06-06-integer-cast-convention-design.md`, issue
#133): flip the four clippy cast lints to `warn`, migrate all ~451 existing
violations, and make non-negative db row fields unsigned so i64 never crosses
the db boundary upward.

**Architecture:** Type-driven at the db boundary (rusqlite 0.40's checked
`FromSql`/`ToSql` for unsigned types validates rows once), a single sanctioned
`convert::usize_from` helper for the 64-bit-guarded `u64→usize` class, `From`
for widenings, `try_from` for genuine narrowings. The lint flip lands last,
once the workspace is warning-clean.

**Tech Stack:** Rust workspace (musefs-db/format/core/fuse/cli/latencyfs),
rusqlite 0.40, clippy via `[workspace.lints.clippy]`, CI runs
`cargo clippy --all-targets -- -D warnings`.

---

## Shared context for every task (read first)

**The convention** (final state, enforced from Task 13 on):

1. Widening (`u8/u16/u32` → wider): `From` (`u64::from(x)`), never `as`.
2. usize↔u64: 64-bit-only is declared by a compile-time guard.
   `usize as u64` stays as-is (clippy-clean). `u64 → usize` only via
   `convert::usize_from(v)`.
3. Genuine narrowing (`u64→u32`, `usize→u32`, `→u8`): prefer restructuring
   (`to_be_bytes`, typed loop counters); else `u32::try_from(x)` —
   `.map_err(|_| FormatError::TooLarge)?` (or the appropriate error) where the
   value is input-dependent, `.expect("…invariant…")` where structurally
   bounded (e.g. `x % 255`), plain `.unwrap()` in `#[cfg(test)]` code and
   test-fixture builders.
4. i64 stays at the db edge; row structs expose unsigned fields.
5. Deliberate bit-truncation keeps `as` with
   `#[expect(clippy::…, reason = "…")]`.

**Until Task 13 flips the lints, violations are invisible to plain builds.**
To see the remaining violations for a path at any time, run:

```bash
cargo clippy --all-targets --message-format=short -- \
  -W clippy::cast_possible_truncation -W clippy::cast_sign_loss \
  -W clippy::cast_possible_wrap -W clippy::cast_lossless 2>&1 \
  | grep -E '^musefs-format/src/mp3' | sort -u        # adjust the grep per task
```

Per-file site lists in tasks below come from this command run against the
pre-migration tree (line numbers will drift as you edit — re-run the command
scoped to your file rather than trusting stale line numbers).

**Line numbers in `src/` files above ~900 are usually inside `#[cfg(test)] mod
tests`** — apply the test rule (`.unwrap()`) there even though the file lives
in `src/`.

**`#[expect]` is self-checking**: if the lint stops firing at that site, the
build errors with "unfulfilled lint expectation". Never add an `#[expect]` for
a lint that doesn't currently fire there.

**Commits:** every task ends in its own commit. Trailer for all commits:

```
Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
```

---

### Task 1: `convert` module + 64-bit guard in musefs-db, re-export in core

**Files:**
- Create: `musefs-db/src/convert.rs`
- Modify: `musefs-db/src/lib.rs:1-8` (mod list)
- Modify: `musefs-core/src/lib.rs:15-25` (re-export list)

- [ ] **Step 1: Write the module with its unit test**

Create `musefs-db/src/convert.rs`:

```rust
//! Sanctioned integer conversions, justified by the 64-bit-only guard below.

// musefs supports 64-bit targets only; this is the compile-time declaration
// of that boundary. It makes u64 <-> usize conversions lossless by
// construction everywhere in the workspace.
const _: () = assert!(
    std::mem::size_of::<usize>() == 8,
    "musefs supports 64-bit targets only"
);

/// The workspace's only sanctioned `u64 -> usize` cast (see the guard above).
#[expect(
    clippy::cast_possible_truncation,
    reason = "u64 -> usize is lossless on 64-bit targets; guarded by the const assert above"
)]
#[inline]
#[must_use]
pub fn usize_from(v: u64) -> usize {
    v as usize
}

#[cfg(test)]
mod tests {
    use super::usize_from;

    #[test]
    fn usize_from_is_lossless_across_the_range() {
        assert_eq!(usize_from(0), 0);
        assert_eq!(usize_from(u64::from(u32::MAX) + 1), 4_294_967_296);
        assert_eq!(usize_from(u64::MAX), usize::MAX);
    }
}
```

- [ ] **Step 2: Wire the module and the re-export**

In `musefs-db/src/lib.rs`, the mod list (lines 1-8) gains one line (keep
alphabetical order):

```rust
mod art;
mod bulk;
pub mod convert;
mod error;
...
```

In `musefs-core/src/lib.rs`, add to the `pub use` block (after line 16):

```rust
pub use musefs_db::convert;
```

- [ ] **Step 3: Run the test**

Run: `cargo test -p musefs-db convert`
Expected: PASS (1 test). Also confirms the `#[expect]` is fulfilled (an
unfulfilled expectation is a warning that CI would deny).

- [ ] **Step 4: Commit**

```bash
git add musefs-db/src/convert.rs musefs-db/src/lib.rs musefs-core/src/lib.rs
git commit -m "Add sanctioned u64->usize conversion behind a 64-bit guard (#133)"
```

---

### Task 2: Flip `Track`/`NewTrack` audio bounds to u64 (TDD)

**Files:**
- Modify: `musefs-db/src/models.rs:86-106` (Track, NewTrack)
- Modify: `musefs-db/src/tracks.rs` (add test)
- Modify: `musefs-core/src/reader.rs:118,148-162,303` and its test module
- Modify: `musefs-core/src/scan.rs:414-416,502-504,1677`
- Modify: consumers that compile-break (core `tests/`, `benches/`, cli `tests/`)

- [ ] **Step 1: Write the failing test**

In `musefs-db/src/tracks.rs`, inside the existing `#[cfg(test)] mod tests`
(the `Db` struct's `conn` field is crate-root-private, so in-crate test
modules can reach it):

```rust
#[test]
fn negative_audio_bounds_error_at_row_read() {
    let db = Db::open_in_memory().unwrap();
    let id = db
        .upsert_track(&NewTrack {
            backing_path: "/x.flac".into(),
            format: Format::Flac,
            audio_offset: 0,
            audio_length: 1,
            backing_size: 1,
            backing_mtime: 0,
        })
        .unwrap();
    // Simulate a malformed external write to a contract column.
    db.conn
        .execute("UPDATE tracks SET audio_offset = -1 WHERE id = ?1", [id])
        .unwrap();
    assert!(
        db.get_track(id).is_err(),
        "negative audio_offset must fail row-read, not wrap"
    );
}
```

(Match the imports of the surrounding test module; add `Format`/`NewTrack` if
not already imported.)

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p musefs-db negative_audio_bounds`
Expected: FAIL — `get_track` currently returns `Ok(Some(track))` with
`audio_offset == -1` (the field is i64, nothing rejects it).

- [ ] **Step 3: Flip the field types**

In `musefs-db/src/models.rs`:

```rust
pub struct Track {
    pub id: i64,
    pub backing_path: String,
    pub format: Format,
    pub audio_offset: u64,
    pub audio_length: u64,
    pub backing_size: u64,
    pub backing_mtime: i64,
    pub content_version: i64,
    pub updated_at: i64,
}
```

and the same three fields in `NewTrack`. No change to `tracks.rs` row mapping
or `params![]` writes — rusqlite's `FromSql for u64` / `ToSql for u64` are
inferred from the field types and do the checked conversion.

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p musefs-db negative_audio_bounds`
Expected: PASS (`FromSqlError::OutOfRange` surfaces through `DbError`).

- [ ] **Step 5: Fix compile fallout in musefs-core src**

`cargo build -p musefs-core` and fix each error. The known sites:

`reader.rs:148-162` — the Synthesis-mode guard loses its negative arms (they
are now unrepresentable) but **keeps** the overflow/file-size check. Replace:

```rust
                // Guard the stored audio bounds before any cast/allocation: a negative
                // bound, or an audio region that runs past the end of the backing file,
                // means the row no longer matches the file. Only synthesis splices at
                // these bounds, so the check is scoped to this mode.
                if track.audio_offset < 0
                    || track.audio_length < 0
                    || (track.audio_offset as u64).saturating_add(track.audio_length as u64)
                        > meta.len()
                {
                    return Err(CoreError::BackingChanged(track.backing_path.clone()));
                }
```

with:

```rust
                // Guard the stored audio bounds before any cast/allocation: an audio
                // region that runs past the end of the backing file means the row no
                // longer matches the file (negative bounds are unrepresentable — the
                // row-read already rejected them). Only synthesis splices at these
                // bounds, so the check is scoped to this mode.
                if track
                    .audio_offset
                    .saturating_add(track.audio_length)
                    > meta.len()
                {
                    return Err(CoreError::BackingChanged(track.backing_path.clone()));
                }
```

`reader.rs:118`: `track.backing_size as u64` → `track.backing_size`.
`reader.rs:303`: `backing_size: track.backing_size as u64` →
`backing_size: track.backing_size`.
All other `track.audio_offset as u64` / `track.audio_length as u64` reads in
`reader.rs` (lines ~182, 201-210, 251-278): drop the ` as u64`.

`scan.rs:414-416` and `scan.rs:502-504` (the two `NewTrack` constructions):

```rust
        audio_offset: probed.audio_offset,
        audio_length: probed.audio_length,
        backing_size: meta.len(),
```

(second site uses `backing_size: meta_len`). `probed.audio_offset/length` are
already u64; `meta.len()`/`meta_len` are u64.

- [ ] **Step 6: Fix compile fallout in test/bench/cli code**

Run `cargo clippy --all-targets 2>&1 | grep -E '^error'` and fix. Expected
fallout (all are `Track`/`NewTrack` literals or comparisons against the
flipped fields):

- `musefs-core/src/reader.rs` test module (~588, 627-628, 684-685, 723):
  `audio_offset: audio_offset as i64` → `audio_offset: audio_offset` (the
  locals are already u64).
- `musefs-core/src/scan.rs:1677` (test): `track.audio_length as usize` →
  `usize::try_from(track.audio_length).unwrap()`.
- `musefs-core/tests/common/mod.rs:18-20,206-208` (`usize as i64` into
  NewTrack fields): `len as i64` → `len as u64` (usize→u64 is clippy-clean).
- `musefs-fuse`, `musefs-cli` tests: same pattern — fixture `NewTrack`
  literals flip `as i64` → `as u64` or drop casts where the source is u64.

- [ ] **Step 7: Run the affected crates' tests**

Run: `cargo test -p musefs-db -p musefs-core -p musefs-cli`
Expected: PASS (FUSE e2e stays `#[ignore]`d).

- [ ] **Step 8: Commit**

```bash
git add -u
git commit -m "Make Track audio bounds unsigned; validate at row-read (#133)"
```

(`git add -u` is acceptable here: only tracked files change in this task.)

---

### Task 3: Flip art/binary-tag byte quantities to unsigned (TDD)

**Files:**
- Modify: `musefs-db/src/models.rs:109-114,136-153,179-183` (NewArt, Art, ArtMeta, BinaryTagRow)
- Modify: `musefs-db/src/art.rs:97`, `musefs-db/src/bulk.rs:111`
- Modify: `musefs-core/src/mapping.rs:43-62,75-81,212-…` (guard, inputs, test)
- Modify: `musefs-core/src/scan.rs:469-470,555-556` (width/height writes)

- [ ] **Step 1: Rewrite the skip-behavior test to pin the new error behavior**

`musefs-core/src/mapping.rs:212` has
`track_art_to_inputs_skips_negative_byte_len`, which pins the old
skip-with-warn behavior. Rewrite it (keep the existing setup code that
creates the db, track, and two art rows; replace name, the `UPDATE`, and the
assertions):

```rust
    #[test]
    fn track_art_to_inputs_errors_on_negative_byte_len() {
        // ... existing setup: tempdir, Db::open, upsert_track -> tid,
        //     upsert_art -> good, upsert_art -> bad, set_track_art ...
        // (byte_len is derived from data, so corrupt it directly — a
        // malformed external write to the contract column.)
        db.conn
            .execute("UPDATE art SET byte_len = -1 WHERE id = ?1", [bad])
            .unwrap();
        let err = track_art_to_inputs(&db, tid);
        assert!(
            err.is_err(),
            "negative byte_len must error at row-read, not be skipped: {err:?}"
        );
    }
```

Keep whatever zero-byte_len assertion the old test made for the `good` row in
a separate, passing form if it still applies (byte_len == 0 remains valid and
must still produce an input).

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p musefs-core errors_on_negative_byte_len`
Expected: FAIL — the current guard skips the row, `track_art_to_inputs`
returns `Ok` with one input.

- [ ] **Step 3: Flip the field types**

In `musefs-db/src/models.rs`:

```rust
pub struct NewArt {
    pub mime: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub data: Vec<u8>,
}
```

`Art`: `width`/`height` → `Option<u32>`, `byte_len` → `u64` (`id`, `sha256`,
`mime`, `data` unchanged). `ArtMeta`: same three fields. `BinaryTagRow`:
`byte_len` → `u64` (`rowid` stays i64).

In `musefs-db/src/art.rs:97` and `musefs-db/src/bulk.rs:111` (the two
`upsert_art` INSERTs — `byte_len` is computed, not a struct field):
`a.data.len() as i64` → `a.data.len() as u64` (clippy-clean; stored via the
fallible `ToSql for u64`).

- [ ] **Step 4: Remove the dissolved guard and casts in mapping.rs**

In `musefs-core/src/mapping.rs:43-62`, the `if meta.byte_len < 0 { … continue; }`
block is now a dead comparison (`unused_comparisons` — warn-by-default rustc
lint, denied in CI) and must go in this same task. Replace the guard plus the
`ArtInput` push with:

```rust
        if let Some(meta) = db.get_art_meta(ta.art_id)? {
            inputs.push(ArtInput {
                art_id: ta.art_id,
                mime: meta.mime,
                description: ta.description,
                picture_type: ta.picture_type as u32,
                width: meta.width.unwrap_or(0),
                height: meta.height.unwrap_or(0),
                data_len: meta.byte_len,
            });
```

(`picture_type as u32` survives until Task 4 flips that field; `width`/
`height`/`data_len` casts vanish now.) Also `mapping.rs:79`:
`len: row.byte_len as u64` → `len: row.byte_len`.

- [ ] **Step 5: Fix remaining compile fallout**

`cargo clippy --all-targets 2>&1 | grep -E '^error'`. Known sites:

- `musefs-core/src/scan.rs:469-470,555-556`:
  `width: (pic.width != 0).then_some(pic.width as i64)` →
  `width: (pic.width != 0).then_some(pic.width)` (pic.width is u32; same for
  height, both sites).
- `musefs-core/src/mapping.rs:91`: `a.data_len as usize` →
  `convert::usize_from(a.data_len)` (import `musefs_db::convert`).
- Test fixtures constructing `NewArt`/`Art` with `Some(300)` etc. continue to
  compile (integer literals infer u32); fix any explicit `i64` annotations.

- [ ] **Step 6: Run the tests**

Run: `cargo test -p musefs-db -p musefs-core`
Expected: PASS, including the rewritten Step 1 test.

- [ ] **Step 7: Commit**

```bash
git add -u
git commit -m "Make art/binary-tag byte quantities unsigned (#133)"
```

---

### Task 4: Flip ordinals to u64 and picture_type to u32

**Files:**
- Modify: `musefs-db/src/models.rs:118-132,157-162,169-173,189-193` (Tag + Tag::new, TrackArt, BinaryTag, StructuralBlock)
- Modify: `musefs-core/src/scan.rs:421-453,483,509-530,568` (ordinal counters)
- Modify: `musefs-core/src/mapping.rs:58,101` (picture_type cast, test helper)
- Modify: `musefs-format/src/mp3.rs:192` area (`apic_framing`)

- [ ] **Step 1: Flip the field types**

In `musefs-db/src/models.rs`: `Tag.ordinal`, `TrackArt.ordinal`,
`BinaryTag.ordinal`, `StructuralBlock.ordinal` → `u64`;
`TrackArt.picture_type` → `u32`; and:

```rust
impl Tag {
    pub fn new(key: &str, value: &str, ordinal: u64) -> Tag {
```

- [ ] **Step 2: Fix the scan.rs ordinal counters**

Four manual accumulators flip `HashMap<String, i64>` → `HashMap<String, u64>`
(`scan.rs:421,442,509,530`; the `or_insert(0)`/`*ord += 1` bodies are
type-inferred and need no edit). The two `.enumerate()` writes at
`scan.rs:437,525` (`ordinal: ordinal as i64`) → `ordinal: ordinal as u64`,
and at `scan.rs:483,568` likewise. `scan.rs:475,560`:
`pic.picture_type as i64` → `pic.picture_type` (already u32).

- [ ] **Step 3: Fix mapping.rs and mp3's APIC framing**

`mapping.rs:58`: `picture_type: ta.picture_type as u32` →
`picture_type: ta.picture_type` (cast vanishes). The test helper at
`mapping.rs:101` (`fn tag(key, value, ordinal: i64)`) flips to `u64`.

`musefs-format/src/mp3.rs:192` (`apic_framing`): `art.picture_type as u8` is
a deliberate pre-existing one-byte truncation (ID3 APIC stores one byte;
valid types are 0..=20). Keep it, made explicit:

```rust
    #[expect(
        clippy::cast_possible_truncation,
        reason = "ID3 APIC type is one byte; valid picture types are 0..=20"
    )]
    d.push(art.picture_type as u8);
```

(Attach the attribute to the statement; if the builder style makes that
awkward, hoist `let picture_type_byte = …;` with the attribute on the `let`.)

- [ ] **Step 4: Fix remaining compile fallout**

`cargo clippy --all-targets 2>&1 | grep -E '^error'`. Expected: `Tag::new`
callers and struct literals with i64 ordinals across db/core/cli tests —
integer literals (`0`, `1`) re-infer to u64 with no edit; explicit
`as i64` fixture casts flip to `as u64` or drop.

- [ ] **Step 5: Run the tests**

Run: `cargo test -p musefs-db -p musefs-core -p musefs-format -p musefs-cli`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add -u
git commit -m "Make ordinals u64 and picture_type u32 across db rows (#133)"
```

---

### Task 5: Remaining musefs-db sites

**Files:**
- Modify: `musefs-db/src/schema.rs:125-145,461`
- Modify: `musefs-db/src/art.rs:62`, `musefs-db/src/tags.rs:116`

- [ ] **Step 1: Restructure the migration loop (no cast at all)**

`schema.rs:128/140` cast loop indices (`(i + 1) as i64`) for `user_version`
targets. Restructure with a typed counter — pattern (adapt to the actual loop
shape at those lines):

```rust
for (target, migration) in (1i64..).zip(MIGRATIONS) {
```

so `target` is born i64 and both casts disappear. `schema.rs:461` is a test:
`MIGRATIONS.len() as i64` → `i64::try_from(MIGRATIONS.len()).unwrap()`.

- [ ] **Step 2: Route blob offsets through the helper**

`art.rs:62` and `tags.rs:116`: `blob.read_at_exact(buf, offset as usize)` →
`blob.read_at_exact(buf, crate::convert::usize_from(offset))`.

- [ ] **Step 3: Verify db is clippy-clean and tests pass**

Run the shared-context clippy command grepped to `musefs-db/src`.
Expected: no output. Run: `cargo test -p musefs-db` → PASS.

- [ ] **Step 4: Commit**

```bash
git add -u
git commit -m "Clear remaining cast-lint sites in musefs-db (#133)"
```

---

### Task 6: musefs-core src sweep

**Files:**
- Modify: `musefs-core/src/reader.rs`, `facade.rs`, `ogg_index.rs`, `scan.rs`, `template.rs`

- [ ] **Step 1: Route `u64 → usize` through the helper (mechanical)**

Every `<expr> as usize` where the source is u64 becomes
`usize_from(<expr>)` with `use musefs_db::convert::usize_from;`. Non-test
sites:
`reader.rs:83,363,373,376,431,528,701,702,760`, `facade.rs:178`
(`(self.0.get() - 1) as usize` → `usize_from(self.0.get() - 1)`),
`ogg_index.rs:76,169,214,215,224,421,545,563,579,595,610`,
`scan.rs:240,253,255,262` (the read-window logic — e.g.
`(window as u64).min(file_len) as usize` →
`usize_from(u64::from(window).min(file_len))` if `window` is u32, or
`usize_from((window as u64).min(file_len))` if it's usize — match the local
type; `usize as u64` stays a plain cast).

- [ ] **Step 2: Fix the Ogg sequence-renumber narrowing**

`ogg_index.rs:198`:

```rust
let new_seq = (old_seq as i64 + seq_delta) as u32;
```

→

```rust
let new_seq = u32::try_from(i64::from(old_seq) + seq_delta)
    .map_err(|_| CoreError::BackingChanged(path.display().to_string()))?;
```

Adapt the error to what's in scope at that site: the function already maps
errors into `CoreError` (line 199); a sequence that leaves u32 range means
the index no longer matches the backing pages — use the same error the
surrounding bounds checks use (look 20 lines up for the established choice;
`BackingChanged` is the semantic fit).

- [ ] **Step 3: Fix the in-src test-module sites (test rule)**

`reader.rs:493` (`vec![0xFFu8; audio_offset as usize]` — local is u64 after
Task 2): `usize::try_from(audio_offset).unwrap()` or `usize_from`;
`reader.rs:605,606,701,702` slicing by u64 locals: same;
`reader.rs:656,660,1093` (`usize → u32` fixture lengths):
`u32::try_from(x).unwrap()`;
`reader.rs:812` (`u32 → u8`), `reader.rs:1082`, `scan.rs:1230`
(`usize → u8` synthetic bytes like `(i % 250 + 1) as u8`): keep `as` only if
masked/bounded **and** add no expect (lints aren't on yet) — preferred:
`u8::try_from(i % 250 + 1).unwrap()`;
`reader.rs:1100,1103` (`usize → i64`): `i64::try_from(x).unwrap()`;
`reader.rs:1178` (`u64 → i64`): `i64::try_from(x).unwrap()`;
`scan.rs:991,1134,1138,1245-1265` (`usize → u32` fixture chunk sizes):
`u32::try_from(x).unwrap()`.

- [ ] **Step 4: Verify core src is clippy-clean**

Run the shared-context clippy command grepped to `musefs-core/src`.
Expected: no output (test-module sites included).
Run: `cargo test -p musefs-core` → PASS.

- [ ] **Step 5: Commit**

```bash
git add -u
git commit -m "Apply cast convention across musefs-core (#133)"
```

---

### Task 7: musefs-format — flac.rs, vorbiscomment.rs, wav.rs

**Files:**
- Modify: `musefs-format/src/flac.rs`, `vorbiscomment.rs`, `wav.rs`

Patterns for all format tasks (7-10):

- **`u64 → usize`** (slicing scanned buffers at parsed offsets):
  `use musefs_db::convert::usize_from;` and wrap. These offsets were already
  bounds-checked by the parser before slicing — the helper just makes the
  pointer-width cast sanctioned.
- **`usize → u32` writing a length field into synthesized metadata** (the
  value derives from generated framing): `u32::try_from(x).map_err(|_|
  FormatError::TooLarge)?` in fallible fns. The synthesized region is
  attacker-influenced via db contents, so `TooLarge` (the existing variant
  for "synthesized metadata exceeds the format's size limit") is the honest
  error, matching the established `if frames_len > … return Err(TooLarge)`
  style (mp3.rs:389, mp4.rs:727, wav.rs:248-area).
- **`u64 → u32` for `art.data_len`** (flac.rs:205, ogg/mod.rs:301): same
  `TooLarge` treatment — art comes from the untrusted db.
- **lossless `u32 as u64`/`u8 as u32`**: `u64::from(x)` / `u32::from(x)`.
- **Test modules**: `try_from(...).unwrap()`.

- [ ] **Step 1: flac.rs (15 sites)**

`151,197,199,499` (+test-mod `567-637`): `usize → u32` length fields → per
the pattern (src sites `TooLarge`, test sites `.unwrap()`).
`205`: `(art.data_len as u32)` →
`u32::try_from(art.data_len).map_err(|_| FormatError::TooLarge)?` — this
makes the picture-builder fn fallible if it isn't already; follow the
existing `Result` plumbing of its caller (the FLAC synthesis fns all return
`Result<_, FormatError>`).
`262,283,451,954`: `u64 → usize` → `usize_from`.

- [ ] **Step 2: vorbiscomment.rs (3 sites: 15,17,22)**

`usize → u32` vorbis length prefixes (tag strings from the db; a >4GiB tag
is malformed input): `u32::try_from(x).map_err(|_| FormatError::TooLarge)?`.

- [ ] **Step 3: wav.rs (11 sites)**

`45,206,248,651`: `u32 as u64` → `u64::from(x)`.
`50,59`: `u64 → usize` → `usize_from`.
`226` (`tag_len as u32`), `235` (`audio_length as u32`), `253`
(`riff_size as u32`): note `riff_size` is explicitly guarded by
`if riff_size > u32::MAX as u64 { return Err(FormatError::TooLarge) }` just
above — replace guard+cast with a single
`let riff_size = u32::try_from(riff_size).map_err(|_| FormatError::TooLarge)?;`
(and the guard's `u32::MAX as u64` lossless cast disappears with it). For
`226`/`235` add the same `try_from … TooLarge` (they are unguarded today —
a >4GiB id3 chunk or data chunk cannot be represented in RIFF).
`179,421,428,539,662,665`: `usize → u32` chunk lengths → same pattern (src)
or `.unwrap()` (test mod).

- [ ] **Step 4: Verify and commit**

Run the clippy command grepped to
`musefs-format/src/(flac|vorbiscomment|wav)`. Expected: no output.
Run: `cargo test -p musefs-format flac wav vorbis` (three invocations or one
plain `cargo test -p musefs-format`). Expected: PASS.

```bash
git add -u
git commit -m "Apply cast convention to FLAC/WAV/VorbisComment synthesis (#133)"
```

---

### Task 8: musefs-format — mp3.rs (22 sites)

**Files:**
- Modify: `musefs-format/src/mp3.rs`

- [ ] **Step 1: Lossless widenings**

`16-21` (`synchsafe_decode`): each `(b[n] & 0x7F) as u32` →
`u32::from(b[n] & 0x7F)`. `537` same pattern. `650`
(`(a << 8) | b as u64` fold): `u64::from(b)`.

- [ ] **Step 2: `u64 → usize` → `usize_from`**

Sites `348,366` (`bt.len as usize`, `data_len as usize` for frame headers)
and test-mod `1343,1373,1707,1733,1760`. For the test-mod sites
`usize::try_from(x).unwrap()` is equally fine — pick one style per file.

- [ ] **Step 3: Narrowings**

`192` was handled in Task 4 (APIC `#[expect]`). `392`
(`syncsafe(frames_len as u32)`): the line above already guards
`frames_len > SYNCHSAFE_MAX as u64` — replace the pair with

```rust
    let frames_len_ss = u32::try_from(frames_len)
        .ok()
        .filter(|&v| v <= SYNCHSAFE_MAX)
        .ok_or(FormatError::TooLarge)?;
    header.extend_from_slice(&syncsafe(frames_len_ss));
```

(keep the explanatory comment about hard error vs truncated file; note
`SYNCHSAFE_MAX as u64` itself was a lossless cast that disappears).
`145` (`usize → u32`): src-side length → `TooLarge` pattern.
Test-mod `1332,1452,1481,1577,1610,1660,1681,1782-1803` (`usize → u32`) and
`1826` (`u32 → u8`): `.try_from(...).unwrap()`.

- [ ] **Step 4: Verify and commit**

Clippy command grepped to `musefs-format/src/mp3`: no output.
`cargo test -p musefs-format mp3`: PASS. Also
`cargo test -p musefs-format --test proptest_mp3` if present (check
`musefs-format/tests/` for the exact proptest name): PASS.

```bash
git add -u
git commit -m "Apply cast convention to MP3/ID3 synthesis (#133)"
```

---

### Task 9: musefs-format — mp4.rs (26 sites)

**Files:**
- Modify: `musefs-format/src/mp4.rs`

- [ ] **Step 1: Lossless widenings**

`66,105,726,825,1042…` etc. (`u32 as u64`): `u64::from(x)`. `781,782,2099,
2103` (`u32 as i64`): `i64::from(x)`.

- [ ] **Step 2: `u64 → usize`** (box-walking slices: `116,319,321,942,1010,
1256,1678,1726,2264,2265`): `usize_from`.

- [ ] **Step 3: Box-size narrowings (`u64 → u32`)**

`604,608` (`---- ` atom), `680,685` (`covr`/`data` sizes), `731,733,737`
(`udta`/`meta`/`ilst` — these three sit right after an existing
`return Err(FormatError::TooLarge)` guard), `836` (`new_moov_size`):
all become `u32::try_from(x).map_err(|_| FormatError::TooLarge)?`. Where a
fn is currently infallible (`build_dash_atom`-style returning `Vec<u8>`),
make it return `Result<Vec<u8>, FormatError>` and `?` at callers — every
transitive caller already returns `Result<_, FormatError>`. Where the
explicit guard makes the `try_from` infallible, you may delete the guard if
and only if the `try_from` error carries the same semantics (it does:
`TooLarge`); otherwise keep both.
`530,872,916` (`usize → u32`): same `TooLarge` pattern.
`785` (`i64 → u32`): inspect the site — it converts a parsed value; use
`u32::try_from(x).map_err(|_| FormatError::Malformed)?` since it's file
input, not synthesized size.

- [ ] **Step 4: Test-module sites** (`1306-1321,2070-2071,2307,2365-2394`):
`.unwrap()` / `From` per the test rule.

- [ ] **Step 5: Verify and commit**

Clippy grepped to `musefs-format/src/mp4`: no output.
`cargo test -p musefs-format mp4`: PASS.

```bash
git add -u
git commit -m "Apply cast convention to MP4 synthesis (#133)"
```

---

### Task 10: musefs-format — ogg/ and fuzz_check.rs

**Files:**
- Modify: `musefs-format/src/ogg/mod.rs`, `ogg/page.rs`, `ogg/b64.rs`, `ogg/crc.rs`, `fuzz_check.rs`

- [ ] **Step 1: ogg/mod.rs (30 sites)**

- `u64 → usize` (page/window arithmetic: `390,452,562,623,753,799,838,839,
  871-874,903,934,964,1003,1169,1317`): `usize_from`.
- `usize → u32` vorbis/picture length fields (`293,295,378,391,660-677,
  1284`): `TooLarge` pattern (src) / `.unwrap()` (test mod ≥1284 — check:
  1284 is inside `mod tests`; verify with the file).
- `301` (`art.data_len as u32`): `TooLarge` pattern (same as flac.rs:205).
- `269` (`u32 as i64` ×2), `341,1042` (`u32 as u64`): `From`.

- [ ] **Step 2: ogg/page.rs (5 sites)**

`73` (`(payload_len % 255) as u8`):
`u8::try_from(payload_len % 255).expect("x % 255 < 256")`.
`116,330` (`chunk as u8`, `seg_count as u8` — both bounded ≤255 by the
lacing construction): `u8::try_from(x).expect("lacing builds at most 255 segments per page")`.
`570` (test, `u64 → usize` ×2): `usize::try_from(*offset).unwrap()` etc.

- [ ] **Step 3: ogg/b64.rs (8 sites) + ogg/crc.rs (1 site)**

b64.rs:`32` (`(out_offset - g0 * 4) as usize` — bounded by base64 group
arithmetic): `usize_from`. `63` (`u64 → u8` — inspect: it extracts a byte
index into the alphabet, bounded by `% 64`-style masking):
`u8::try_from(...).expect("…mask bound…")` matching what the expression
guarantees. `65,72,73,76` (test-mod slicing): `usize_from` or
`.try_from(...).unwrap()`.

crc.rs:`11` (`(i as u32) << 24` in a `const fn` — `try_from` is not const):
restructure the loop variable:

```rust
const fn build_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut i = 0u32;
    while i < 256 {
        let mut crc = i << 24;
        ...
        t[i as usize] = crc;   // u32 -> usize: widening on 64-bit, clippy-clean
        i += 1;
    }
    t
}
```

- [ ] **Step 4: fuzz_check.rs (12 sites — all fixture builders)**

`64-66` (`usize → u8`), `89-93,123,189,220-228,401` (`usize → u32`): these
build minimal valid files for fuzz seeds/tests; values are literal-bounded →
`u8::try_from(x).unwrap()` / `u32::try_from(x).unwrap()`.

- [ ] **Step 5: Verify and commit**

Clippy grepped to `musefs-format/src`: **no output at all** (format crate now
fully clean). `cargo test -p musefs-format`: PASS (includes the
feature-gated proptests via the self-dev-dependency).

```bash
git add -u
git commit -m "Apply cast convention to Ogg synthesis and fixtures (#133)"
```

---

### Task 11: musefs-fuse and musefs-latencyfs

**Files:**
- Modify: `musefs-fuse/src/lib.rs:339,383`
- Modify: `musefs-latencyfs/src/lib.rs` (10 sites + local guard)

- [ ] **Step 1: fuse (2 sites)**

`339`: `size as u64` → `u64::from(size)`.
`383`: `.skip(offset as usize)` → `.skip(usize_from(offset))` with
`use musefs_core::convert::usize_from;` (the Task 1 re-export; fuse depends
only on core). Do **not** touch `lib.rs:112` (`attr.mtime_secs as u64`) —
clippy does not flag it (see spec); adding an `#[expect]` there would error
as unfulfilled.

- [ ] **Step 2: latencyfs**

latencyfs is standalone (no musefs deps — keep it that way). Add at the top
of `musefs-latencyfs/src/lib.rs` (after the existing inner attributes/imports):

```rust
// latencyfs is 64-bit-only, like the rest of the workspace (musefs-db's
// convert module declares the same bound for the dependent crates).
const _: () = assert!(
    std::mem::size_of::<usize>() == 8,
    "musefs-latencyfs supports 64-bit targets only"
);
```

Then per site:
- `159` (`nsec as u32` in the `t` closure — i64 nanoseconds, OS-guaranteed
  `0..=999_999_999`): `u32::try_from(nsec).unwrap_or(0)`.
- `174-178` (`m.nlink() as u32`? — inspect; MetadataExt returns u64 for
  `nlink`/`rdev`/`blksize`): `u32::try_from(x).unwrap_or(u32::MAX)` — this
  is a latency-measurement harness translating OS metadata to FUSE wire
  types; saturation is harmless and honest.
- `296,358` (`offset as usize`, `size as usize`): local
  `#[expect(clippy::cast_possible_truncation, reason = "64-bit-only (const assert at top of file); u64 -> usize is lossless")]`
  on the smallest enclosing item, or a local `fn usize_from(v: u64) -> usize`
  clone of the db helper — pick the helper if both sites can share it.
- `417-419` (statfs `s.f_bfree as u64` etc. — inspect: statfs fields are
  already u64 on linux; the flagged ones are the `as u32` trio at the end:
  `f_bsize/f_namemax/f_frsize`): `u32::try_from(x).unwrap_or(u32::MAX)`.
- `502` (`reply.written(n as u32)` where n: usize from `write_at`):
  `u32::try_from(n).unwrap_or(u32::MAX)` (a single write can't exceed u32 in
  practice; saturate rather than panic in the harness).

- [ ] **Step 3: Verify and commit**

Clippy grepped to `musefs-fuse/src|musefs-latencyfs/src`: no output.
`cargo test -p musefs-fuse -p musefs-latencyfs`: PASS.

```bash
git add -u
git commit -m "Apply cast convention to fuse adapter and latencyfs (#133)"
```

---

### Task 12: Test/bench fixture sweep (~190 sites)

**Files:**
- Modify: `musefs-core/tests/`, `musefs-core/benches/`, `musefs-format/tests/`,
  `musefs-cli/tests/`, `musefs-db` test modules, `musefs-fuse` tests — as
  enumerated by clippy.

- [ ] **Step 1: Enumerate what's left**

Run the shared-context clippy command with grep
`-E 'tests/|benches/'`. Everything remaining is fixture code.

- [ ] **Step 2: Apply the test rule mechanically**

- `len() as u32` / `usize → u32` header fields in fixture builders
  (`musefs-core/tests/common/corpus.rs:234,241`, `common/mod.rs:136,190`,
  `musefs-cli/tests/scan.rs:26-30`, `musefs-format/tests/*` …):
  `u32::try_from(x).unwrap()`.
- `usize → u8` synthetic byte streams (`musefs-cli/tests/scan.rs:7-9` …):
  `u8::try_from(x).unwrap()` — or restructure the iterator to produce u8
  (`(0u8..=200)`) where it reads better.
- `usize → i64` / `u64 → i64` into db params: `i64::try_from(x).unwrap()` —
  but first check whether Task 2-4's field flips already removed the i64
  target (then the cast just drops).
- `u32 as u64` lossless (`musefs-format/tests/wav_synthesize.rs:166`,
  `mp3_synthesize.rs:29-32` …): `u64::from(x)`.
- `u64 → usize` slicing: `usize::try_from(x).unwrap()` (tests prefer the
  loud form over the helper).

- [ ] **Step 3: Verify the whole workspace is clean**

Run the shared-context clippy command with **no** grep filter.
Expected: zero warnings.

- [ ] **Step 4: Run everything**

Run: `cargo test`
Expected: PASS (workspace, FUSE e2e still ignored).

- [ ] **Step 5: Commit**

```bash
git add -u
git commit -m "Apply cast convention to test and bench fixtures (#133)"
```

---

### Task 13: Flip the lints, document the convention

**Files:**
- Modify: `Cargo.toml` (workspace lints block)
- Modify: `CLAUDE.md` (Conventions section)

- [ ] **Step 1: Flip the four lints and replace the stale comment**

In the root `Cargo.toml`, replace:

```toml
# Explicit `as` casts are deliberate throughout the byte/offset arithmetic
# (audio bounds, chunk sizes, FUSE inode/size fields); flagging every one is noise.
cast_possible_truncation = "allow"
cast_sign_loss = "allow"
cast_possible_wrap = "allow"
cast_lossless = "allow"
```

with:

```toml
# Integer-conversion convention (docs/superpowers/specs/2026-06-06-integer-cast-convention-design.md):
# widenings use `From`; u64->usize goes through musefs_db::convert::usize_from
# (64-bit-only, compile-time guarded); genuine narrowings use `try_from`
# (`?` on input-dependent values, `.expect`/`.unwrap` on structurally bounded
# ones and in tests); deliberate bit-truncation carries a per-site `#[expect]`.
cast_possible_truncation = "warn"
cast_sign_loss = "warn"
cast_possible_wrap = "warn"
cast_lossless = "warn"
```

- [ ] **Step 2: Add the convention to CLAUDE.md**

In `CLAUDE.md` under `## Conventions`, append one bullet:

```markdown
- Integer conversions: the four clippy cast lints are deny-via-CI. Widenings
  use `From`; `u64 -> usize` only via `musefs_db::convert::usize_from`
  (the workspace is declared 64-bit-only; `usize as u64` is fine); genuine
  narrowings use `try_from` (`?` for input-dependent values, `.expect` for
  structurally bounded ones, `.unwrap` in tests); deliberate bit-truncation
  keeps `as` under a reasoned `#[expect]`. Non-negative db row fields are
  unsigned; rusqlite's checked conversions validate at the row boundary.
```

- [ ] **Step 3: Verify both CI clippy gates pass**

Run: `cargo clippy --all-targets -- -D warnings`
Expected: clean exit 0.
Run: `cargo clippy -p musefs-db --features mutants --all-targets -- -D warnings`
Expected: clean exit 0 (the `mutants` feature adds `derive(Default)` paths).

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml CLAUDE.md
git commit -m "Enforce the integer cast convention via clippy lints (#133)"
```

---

### Task 14: Full validation sweep

No new code — the gates, in order (stop and fix on any failure; re-run the
failed gate after fixing):

- [ ] **Step 1: Format check**

Run: `cargo fmt --all --check`
Expected: exit 0 (check the exit status directly, not just output).
If it fails: `cargo fmt --all`, re-check, and amend the fix into a new commit.

- [ ] **Step 2: Workspace tests + proptests**

Run: `cargo test`
Run: `cargo test -p musefs-format`
Run: `cargo test -p musefs-core --test proptest_read_fidelity`
Expected: all PASS — the byte-identical serve-path invariant must hold.

- [ ] **Step 3: Fuzz targets still build (out-of-workspace crate)**

Run, for each of flac mp3 mp4 ogg wav ogg_page b64 vorbiscomment:
`cargo +nightly fuzz build <target>`
Expected: builds. (Format-layer signature changes — the new `Result` returns
from Task 9 — only surface here and in CI's smoke job.)

- [ ] **Step 4: In-diff mutation gate (CI parity)**

```bash
git diff "$(git merge-base main HEAD)...HEAD" -- '*.rs' > mutants.diff
grep -q '^@@ ' mutants.diff   # MUST succeed — an empty diff is a silent false pass
cargo mutants --in-diff mutants.diff -j2 --exclude 'musefs-latencyfs/**' --output /tmp/mutants-out/in-diff
```

Do NOT set TMPDIR. Expected: no missed mutants. Notes: the diff is large and
the run will be long; `convert::usize_from` mutations are killed by the
existing read-path tests plus Task 1's range test. If a mutant in a
`try_from(...).map_err(...)` arm survives because no test exercises the
overflow, prefer adding a small unit test for that arm over an exclude.

- [ ] **Step 5: FUSE end-to-end (real mount, this machine has /dev/fuse)**

Run: `cargo test -p musefs-fuse -- --ignored`
Expected: PASS.

- [ ] **Step 6: Final commit (if validation produced fixes)**

```bash
git add -u
git commit -m "Validation fixes for the cast convention migration (#133)"
```

---

## Execution notes

- Tasks 2-4 change db model types; between tasks the tree always compiles
  and tests pass, but the cast lints stay quiet until Task 13 — use the
  shared-context clippy command as your per-task progress meter.
- Tasks 7-10 (format files) are independent of each other and of Task 11;
  they can be parallelized across subagents if desired. Tasks 1→2→3→4 are
  strictly ordered; Task 5 needs 1; Task 6 needs 1-4; Task 12 needs all
  prior; 13 needs 12; 14 needs 13.
- If clippy reveals sites this plan's lists miss (lists were generated from
  one pre-migration run), classify them by the Shared-context rules — the
  convention covers every case; the lists are navigation aids, not the spec.
