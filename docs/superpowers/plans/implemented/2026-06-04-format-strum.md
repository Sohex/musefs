# strum-derived `Format` Implementation Plan (#129)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the hand-written `Format::as_str()`/`parse()` pair with strum derives so the enum↔string mapping cannot drift when a format is added.

**Architecture:** `Format` (musefs-db/src/models.rs) gains `EnumString`/`IntoStaticStr`/`EnumIter` derives with `serialize_all = "lowercase"` (which reproduces every current DB string, including `OggFlac` → `"oggflac"`). `as_str()` stays as a one-line delegate so its three call sites don't change; hand-written `parse()` is deleted and its single production caller (`parse_format_col`) switches to the derived `FromStr`. The DB strings are an external contract (beets/Picard write them), pinned by an explicit test.

**Tech Stack:** Rust, strum 0.28 (new third-party proc-macro dependency in musefs-db — call this out in the PR for supply-chain scrutiny), rusqlite.

**Spec:** `docs/superpowers/specs/2026-06-04-format-strum-db-typestate-design.md` (Part 1)

---

### Task 1: Branch, dependency, and failing tests

**Files:**
- Modify: `musefs-db/Cargo.toml`
- Test: `musefs-db/src/models.rs` (the `tests` module at the bottom, lines ~40-67)

- [ ] **Step 1: Create the branch**

```bash
git checkout -b format-strum main
```

- [ ] **Step 2: Add the strum dependency**

In `musefs-db/Cargo.toml`, the `[dependencies]` section currently reads:

```toml
[dependencies]
rusqlite = { version = "0.40", features = ["bundled", "blob"] }
sha2 = "0.11"
thiserror = "2"
```

Add strum after sha2:

```toml
[dependencies]
rusqlite = { version = "0.40", features = ["bundled", "blob"] }
sha2 = "0.11"
strum = { version = "0.28", features = ["derive"] }
thiserror = "2"
```

- [ ] **Step 3: Write the failing tests**

In `musefs-db/src/models.rs`, replace the entire `#[cfg(test)] mod tests` block (it currently holds three tests: `m4a_round_trips`, `ogg_codecs_round_trip`, `wav_round_trips`) with:

```rust
#[cfg(test)]
mod tests {
    use super::Format;
    use strum::IntoEnumIterator;

    #[test]
    fn every_format_round_trips() {
        for f in Format::iter() {
            assert_eq!(f.as_str().parse::<Format>(), Ok(f));
        }
    }

    /// The strings are a DB contract — external writers (beets/Picard) store
    /// them. A variant rename must not silently change the stored string.
    #[test]
    fn db_strings_are_pinned() {
        let expected = [
            (Format::Flac, "flac"),
            (Format::Mp3, "mp3"),
            (Format::M4a, "m4a"),
            (Format::Opus, "opus"),
            (Format::Vorbis, "vorbis"),
            (Format::OggFlac, "oggflac"),
            (Format::Wav, "wav"),
        ];
        assert_eq!(expected.len(), Format::iter().count());
        for (f, s) in expected {
            assert_eq!(f.as_str(), s);
        }
    }
}
```

- [ ] **Step 4: Run the tests to verify they fail**

```bash
cargo test -p musefs-db models
```

Expected: COMPILE ERROR — `Format::iter()` does not exist yet (E0599, no `iter` on `Format`) and `parse::<Format>()` has no `FromStr` impl (E0277). This is the failing state TDD wants.

### Task 2: Derive the mapping, delete `parse`, switch the call site

**Files:**
- Modify: `musefs-db/src/models.rs:1-38` (the `Format` enum and `impl Format`)
- Modify: `musefs-db/src/tracks.rs:10-18` (`parse_format_col`)

- [ ] **Step 1: Derive on the enum and shrink `impl Format`**

In `musefs-db/src/models.rs`, the file currently opens directly with the `Format` enum (no imports above it). Replace the enum and the whole `impl Format` block (which holds `as_str` and `parse`) with:

```rust
use strum::{EnumIter, EnumString, IntoStaticStr};

/// The DB text representation (the `tracks.format` column) is derived:
/// `serialize_all = "lowercase"` lowercases the whole variant ident
/// (`OggFlac` → `"oggflac"`). The strings are an external contract —
/// beets/Picard write them — pinned by `tests::db_strings_are_pinned`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumString, IntoStaticStr, EnumIter)]
#[strum(serialize_all = "lowercase")]
#[cfg_attr(feature = "mutants", derive(Default))]
pub enum Format {
    #[cfg_attr(feature = "mutants", default)]
    Flac,
    Mp3,
    M4a,
    Opus,
    Vorbis,
    OggFlac,
    Wav,
}

impl Format {
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}
```

Note what changed: the original `Debug, Clone, Copy, PartialEq, Eq` derives and both `cfg_attr(feature = "mutants", ...)` attributes are preserved; `EnumString, IntoStaticStr, EnumIter` and the `#[strum(...)]` attribute are added; `parse()` is gone; `as_str()` delegates to the derived `IntoStaticStr`.

- [ ] **Step 2: Switch `parse_format_col` to the derived `FromStr`**

In `musefs-db/src/tracks.rs`, `parse_format_col` currently reads:

```rust
fn parse_format_col(fmt: &str) -> rusqlite::Result<Format> {
    Format::parse(fmt).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            usize::MAX,
            rusqlite::types::Type::Text,
            format!("unknown format {fmt}").into(),
        )
    })
}
```

Change only the first line of the body (the error mapping with its `"unknown format {fmt}"` message stays — it is more diagnostic than strum's generic `ParseError`):

```rust
fn parse_format_col(fmt: &str) -> rusqlite::Result<Format> {
    fmt.parse::<Format>().ok().ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            usize::MAX,
            rusqlite::types::Type::Text,
            format!("unknown format {fmt}").into(),
        )
    })
}
```

`parse_format_col` is the **only** production caller of the old `Format::parse` (the row mappers at `tracks.rs:157` and `:212` go through it); nothing else in the workspace calls `Format::parse`, so deleting it breaks no other call site. The three `as_str()` call sites (`tracks.rs:61`, `bulk.rs:59`, `musefs-core/src/facade.rs:228`) are untouched — the method still exists with the same signature.

- [ ] **Step 3: Run the musefs-db tests to verify they pass**

```bash
cargo test -p musefs-db
```

Expected: PASS, including `models::tests::every_format_round_trips` and `models::tests::db_strings_are_pinned`.

- [ ] **Step 4: Commit**

```bash
git add musefs-db/Cargo.toml musefs-db/src/models.rs musefs-db/src/tracks.rs Cargo.lock
git commit -m "Derive the Format string mapping with strum (#129)"
```

### Task 3: Workspace validation and mutation gate

**Files:** none modified — verification only.

- [ ] **Step 1: Full workspace check**

```bash
cargo test --workspace && cargo clippy --all-targets && cargo fmt --all --check
```

Expected: all pass. (`facade.rs:228` exercises `as_str` through the core suite.) Check the exit status of each directly — do not infer success from quiet output.

- [ ] **Step 2: In-diff mutation gate (CI parity)**

```bash
git diff "$(git merge-base main HEAD)...HEAD" -- '*.rs' > mutants.diff
grep -q '^@@ ' mutants.diff
cargo mutants --in-diff mutants.diff -j2 --exclude 'musefs-latencyfs/**' --output /tmp/mutants-out/in-diff
```

Expected: `grep` succeeds (non-empty diff — an empty diff is a silent false pass), `cargo mutants` exits 0 with no missed mutants. Do NOT set TMPDIR.

- [ ] **Step 3: Hand off**

The branch is ready for review/merge — use the superpowers:finishing-a-development-branch skill (PR title: `Derive the Format string mapping with strum (#129)`; flag the new strum dependency in the PR body).
