//! Size and identity caps enforced at the DB boundary (#267/#269/#278).
//!
//! The `CHECK` constraints in [`crate::schema`] (`MIGRATION_V4`) enforce these
//! at write time for honest writers; the reader guards in [`crate::tags`],
//! [`crate::art`] and [`crate::structural`] re-enforce them at read time,
//! because a crafted DB can carry the canonical schema yet smuggle a
//! CHECK-violating row (`PRAGMA ignore_check_constraints`). Values are public so
//! cross-layer drift tests can assert they match the format ceiling and the
//! scanner caps.

/// Max `tags.key` length. Compared against SQLite `length()` (i64).
pub const MAX_TAG_KEY_LEN: i64 = 256;
/// Max `tags.value` length in bytes — 256 KiB.
pub const MAX_TAG_VALUE_LEN: i64 = 262_144;
/// Max `art.mime` length.
pub const MAX_ART_MIME_LEN: i64 = 255;
/// Max `track_art.description` length — 1 KiB.
pub const MAX_ART_DESCRIPTION_LEN: i64 = 1024;
/// Max `structural_blocks.body` length in bytes. Mirrors
/// `musefs_format::flac::MAX_BLOCK_BODY` (FLAC's 24-bit block limit); the db
/// layer cannot depend on the format layer, so the equality is asserted by a
/// `musefs-core` test (see the plan, Task 7).
pub const MAX_STRUCTURAL_BODY_LEN: i64 = 0x00FF_FFFF;
/// Max tag rows materialized per track, applied to the text and binary sets
/// independently.
pub const MAX_TAGS_PER_TRACK: usize = 4096;
/// Valid `structural_blocks.kind` values. Single source for the V4 `CHECK`
/// (asserted by a drift test) and the `get_structural_blocks` guard.
pub const STRUCTURAL_KINDS: [&str; 2] = ["STREAMINFO", "SEEKTABLE"];
/// `tags.value_blob` length cap in bytes — defense-in-depth `CHECK` only (the
/// blob streams at read time, so no reader guard). Mirrors `musefs-core`'s
/// `MAX_BINARY_TAG_BYTES`.
pub const MAX_BINARY_TAG_BYTES: i64 = 16_711_680;
/// `art.byte_len` cap in bytes — defense-in-depth `CHECK` only. Mirrors
/// `musefs-core`'s `MAX_ART_BYTES`.
pub const MAX_ART_BYTES: i64 = 16_711_680;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cap_values_are_pinned() {
        assert_eq!(MAX_TAG_VALUE_LEN, 256 * 1024);
        assert_eq!(MAX_ART_DESCRIPTION_LEN, 1024);
        assert_eq!(MAX_STRUCTURAL_BODY_LEN, 0x00FF_FFFF);
        assert_eq!(MAX_BINARY_TAG_BYTES, 16 * 1024 * 1024 - 64 * 1024);
        assert_eq!(MAX_ART_BYTES, 16 * 1024 * 1024 - 64 * 1024);
        assert_eq!(STRUCTURAL_KINDS, ["STREAMINFO", "SEEKTABLE"]);
    }
}
