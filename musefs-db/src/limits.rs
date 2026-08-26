//! Size and identity caps enforced at the DB boundary (#267/#269/#278).
//!
//! The `CHECK` constraints in [`crate::schema`] (`MIGRATION_V1`) enforce these
//! at write time for honest writers; the reader guards in [`crate::tags`],
//! [`crate::art`] and [`crate::structural`] re-enforce them at read time,
//! because a crafted DB can carry the canonical schema yet smuggle a
//! CHECK-violating row (`PRAGMA ignore_check_constraints`). Values are public so
//! cross-layer drift tests can assert they match the format ceiling and the
//! scanner caps.

/// Max `tags.key` length. Compared against SQLite `length()` (i64).
pub const MAX_TAG_KEY_LEN: i64 = 256;
/// Max `tags.value` length in bytes — 16 MiB − 1. Both the schema `CHECK`
/// (`length(CAST(value AS BLOB)) <= 16777215`) and the read-time guard in
/// [`crate::tags`] count bytes, not UTF-8 characters, so the
/// materialized-memory bound is exact rather than ~4x looser for multibyte
/// text (#505).
///
/// The value is FLAC's 24-bit metadata-block ceiling
/// (`musefs_format::flac::MAX_BLOCK_BODY`, equal to [`MAX_STRUCTURAL_BODY_LEN`]),
/// i.e. the largest tag synthesis could ever serve. It was 256 KiB until #644,
/// which was a number musefs invented: a lyrics/cuesheet/review tag past it
/// aborted the whole scan on the `CHECK`. Capping below what the format can
/// carry is enumerating badness against people who do unusual-but-legal things
/// to their files, so the cap now sits exactly at the format ceiling and
/// anything past it fails that one file with a legible message.
///
/// Materialization bound, stated plainly rather than papered over: a crafted DB
/// can pair this with [`MAX_TAGS_PER_TRACK`] for ~64 GiB of text on one track.
/// That is a wider DoS surface than the old 256 KiB gave (~1 GiB), accepted as
/// low-severity — it needs a hand-crafted store the reader already distrusts,
/// and the payoff is that no honest file can be rejected for a limit musefs
/// made up.
pub const MAX_TAG_VALUE_LEN: i64 = 0x00FF_FFFF;
/// Max `art.mime` length.
pub const MAX_ART_MIME_LEN: i64 = 255;
/// Max `track_art.description` length — 8 KiB. Raised from 1 KiB in #644: a
/// picture description is free-form UTF-8 with a 32-bit length in both FLAC
/// `PICTURE` and ID3 `APIC`, and a tagger pasting a paragraph of provenance
/// into it is odd but legal. The row cost is negligible next to the art blob it
/// annotates, so the tight cap bought nothing and only risked failing a file.
pub const MAX_ART_DESCRIPTION_LEN: i64 = 8192;
/// Max `structural_blocks.body` length in bytes. Mirrors
/// `musefs_format::flac::MAX_BLOCK_BODY` (FLAC's 24-bit block limit); the db
/// layer cannot depend on the format layer, so the equality is asserted by a
/// `musefs-core` test (see the plan, Task 7).
pub const MAX_STRUCTURAL_BODY_LEN: i64 = 0x00FF_FFFF;
/// Max tag rows materialized per track, applied to the text and binary sets
/// independently.
pub const MAX_TAGS_PER_TRACK: usize = 4096;
/// Max `track_art` rows materialized per track on the serve path. Art is
/// low-cardinality (cover/back/leaflet/per-disc), so this is a crafted-DB
/// corruption backstop, not a semantic limit. Mirrors `MAX_TAGS_PER_TRACK`'s
/// reader-guard role (a per-track row COUNT cannot be a column CHECK, so there is
/// no write-time enforcement to lean on).
pub const MAX_ART_ROWS_PER_TRACK: usize = 4096;
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
        assert_eq!(MAX_TAG_VALUE_LEN, 16 * 1024 * 1024 - 1);
        // The tag cap is the format ceiling, not an independent number: it must
        // track FLAC's 24-bit block limit, which `MAX_STRUCTURAL_BODY_LEN`
        // already mirrors (and a musefs-core test ties to the format constant).
        assert_eq!(MAX_TAG_VALUE_LEN, MAX_STRUCTURAL_BODY_LEN);
        assert_eq!(MAX_ART_DESCRIPTION_LEN, 8 * 1024);
        assert_eq!(MAX_STRUCTURAL_BODY_LEN, 0x00FF_FFFF);
        assert_eq!(MAX_BINARY_TAG_BYTES, 16 * 1024 * 1024 - 64 * 1024);
        assert_eq!(MAX_ART_BYTES, 16 * 1024 * 1024 - 64 * 1024);
        assert_eq!(STRUCTURAL_KINDS, ["STREAMINFO", "SEEKTABLE"]);
        assert_eq!(MAX_ART_ROWS_PER_TRACK, 4096);
    }
}
