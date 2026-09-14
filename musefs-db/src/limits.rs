//! Size and identity caps enforced at the DB boundary (#267/#269/#278).
//!
//! The `CHECK` constraints in `crate::schema` (`MIGRATION_V1`) enforce these
//! at write time for honest writers; the reader guards in `crate::tags`,
//! `crate::art` and `crate::structural` re-enforce them at read time,
//! because a crafted DB can carry the canonical schema yet smuggle a
//! CHECK-violating row (`PRAGMA ignore_check_constraints`). Values are public so
//! cross-layer drift tests can assert they match the format ceiling and the
//! scanner caps.

/// Max `tags.key` length. Compared against SQLite `length()` (i64).
pub const MAX_TAG_KEY_LEN: i64 = 256;
/// Max `tags.value` length in bytes — 16 MiB − 1. Both the schema `CHECK`
/// (`length(CAST(value AS BLOB)) <= 16777215`) and the read-time guard in
/// `crate::tags` count bytes, not UTF-8 characters, so the
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
/// Max `track_art.mime` length. The column moved there from `art` in schema v4
/// (#716).
pub const MAX_ART_MIME_LEN: i64 = 255;
/// Max `tracks.backing_path` length in bytes — 64 KiB (#758). The column is a
/// BLOB, so the schema `CHECK` and the reader guard in `crate::tracks` both
/// count bytes.
///
/// A generous portable ceiling rather than `PATH_MAX`, which is 4096 on Linux and
/// 1024 on macOS: the store needs one shape wherever it is opened. musefs opens a
/// backing file by absolute path, so nothing past the OS limit could be served
/// anyway, and the cap refuses no servable file. What it buys is the bound: every
/// reader materializes the path, `getattr` included, and before this a crafted
/// store could make that allocation as large as SQLite allows a value.
pub const MAX_BACKING_PATH_BYTES: i64 = 64 * 1024;
/// Exact `art.sha256` length: a hex-encoded SHA-256 digest. The schema `CHECK`
/// pins it to equality; the reader guard in `crate::art` bounds only the
/// upper side, since a short digest is a correctness problem for the caller
/// rather than an allocation one.
pub const ART_SHA256_LEN: i64 = 64;
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

/// The longest valid `structural_blocks.kind`, in characters: the reader bounds
/// the column with it before materializing the value (#715). A property of
/// [`STRUCTURAL_KINDS`], not an invented number — a test pins the two together.
pub const MAX_STRUCTURAL_KIND_LEN: i64 = 10;
/// `tags.value_blob` length cap in bytes — defense-in-depth `CHECK` only (the
/// blob streams at read time, so no reader guard). Mirrors `musefs-core`'s
/// `MAX_BINARY_TAG_BYTES`.
pub const MAX_BINARY_TAG_BYTES: i64 = 16_711_680;
/// `art.byte_len` cap in bytes. Mirrors `musefs-core`'s `MAX_ART_BYTES`. The
/// serve path streams the blob and needs no guard, but `get_art` materializes
/// it whole, so that reader bounds `length(data)` against this before the read
/// — a crafted store can disagree with both the cap and the
/// `byte_len = length(data)` `CHECK` (#693).
pub const MAX_ART_BYTES: i64 = 16_711_680;

/// The most a SQLite record spends beyond its text and blob payloads: a
/// header-size varint, then per column a serial-type varint and an integer
/// body, at most 9 and 8 bytes each. Counted over the five columns of `tags`,
/// the table whose payloads [`MAX_ROW_BYTES`] adds up.
const RECORD_OVERHEAD_BYTES: i64 = 9 + 5 * (9 + 8);

/// The widest record a row valid under the schema can be: a `tags` value at
/// [`MAX_TAG_VALUE_LEN`] beside a key at its byte ceiling, which outweighs a
/// structural block or an art row at their caps. Every [`crate::Db`]
/// connection installs it as `SQLITE_LIMIT_LENGTH`.
///
/// That limit is what bounds a read against a hostile row. The reader guards
/// decide from projected lengths, but `sqlite3_step` materializes every column
/// a statement selects before any guard sees the row, so a guard bounds what
/// Rust allocates and nothing about what SQLite already did. With the limit,
/// SQLite refuses a string or blob past it with `SQLITE_TOOBIG` instead of
/// loading it. It has to admit the whole record rather than the widest value
/// alone, because SQLite applies it to a record built for a write and to one
/// a `VACUUM` copies.
pub const MAX_ROW_BYTES: i64 =
    MAX_TAG_VALUE_LEN + crate::error::max_utf8_bytes(MAX_TAG_KEY_LEN) + RECORD_OVERHEAD_BYTES;
// The widest row is the tag row: a structural block or an art row at its cap is
// narrower, so neither needs a term of its own.
const _: () = assert!(
    MAX_STRUCTURAL_BODY_LEN + MAX_STRUCTURAL_KIND_LEN < MAX_ROW_BYTES - RECORD_OVERHEAD_BYTES
);
const _: () = assert!(MAX_ART_BYTES + ART_SHA256_LEN < MAX_ROW_BYTES - RECORD_OVERHEAD_BYTES);

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
        // The reader cap is exactly the longest allowlisted kind: any narrower
        // and a valid row is refused, any wider and it bounds nothing extra.
        let longest = STRUCTURAL_KINDS.iter().map(|k| k.chars().count()).max();
        assert_eq!(
            i64::try_from(longest.unwrap()).unwrap(),
            MAX_STRUCTURAL_KIND_LEN
        );
        assert_eq!(MAX_ART_ROWS_PER_TRACK, 4096);
        assert_eq!(ART_SHA256_LEN, 64);
        assert_eq!(MAX_BACKING_PATH_BYTES, 65_536);
        assert_eq!(RECORD_OVERHEAD_BYTES, 94);
        assert_eq!(MAX_ROW_BYTES, 16 * 1024 * 1024 - 1 + 4 * 256 + 94);
    }
}
