//! Shared fixtures for the `musefs-fuse` integration tests: the minimal proven
//! FLAC builder, the default [`MountConfig`], a tiny PNG + FLAC PICTURE block,
//! and a recursive tree walk. The FLAC byte-layout primitives are reused from
//! `musefs_format::fuzz_check::fixtures` so there is a single source of truth.
//!
//! `#![allow(dead_code)]` because each integration-test binary compiles this
//! module in full but only uses a subset of the helpers.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use musefs_core::{Mode, MountConfig};
use musefs_format::fuzz_check::fixtures::{flac_block, streaminfo_body, vorbis_comment_body};

/// Minimal proven FLAC stream: `fLaC` + STREAMINFO + a VORBIS_COMMENT carrying
/// the already-formatted `KEY=value` `comments`, followed by raw `audio`.
pub fn make_flac(comments: &[&str], audio: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"fLaC");
    out.extend_from_slice(&flac_block(0, &streaminfo_body(), false));
    out.extend_from_slice(&flac_block(4, &vorbis_comment_body("orig", comments), true));
    out.extend_from_slice(audio);
    out
}

/// Default mount config: `$artist/$title`, Synthesis mode, no polling.
pub fn config() -> MountConfig {
    config_with_mode(Mode::Synthesis)
}

/// Mount config with an explicit [`Mode`] (passthrough exercises StructureOnly).
pub fn config_with_mode(mode: Mode) -> MountConfig {
    MountConfig {
        template: "$artist/$title".to_string(),
        fallbacks: BTreeMap::new(),
        default_fallback: "Unknown".to_string(),
        mode,
        poll_interval: std::time::Duration::ZERO,
        case_insensitive: false,
        read_ahead_budget: 64 * 1024 * 1024,
        read_ahead_prefetch: false,
        skip_on_missing: false,
    }
}

/// A tiny valid 4×4 front-cover PNG used to embed art into test fixtures.
pub const COVER_PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x04, 0x08, 0x02, 0x00, 0x00, 0x00, 0x26, 0x93, 0x09,
    0x29, 0x00, 0x00, 0x00, 0x09, 0x70, 0x48, 0x59, 0x73, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00,
    0x01, 0x00, 0x4F, 0x25, 0xC4, 0xD6, 0x00, 0x00, 0x00, 0x14, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C,
    0x63, 0x64, 0x60, 0xF8, 0xC7, 0x00, 0x03, 0x2C, 0x0C, 0x48, 0x00, 0x37, 0x07, 0x00, 0x32, 0x3E,
    0x01, 0x0C, 0x1C, 0xDB, 0xAF, 0x41, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42,
    0x60, 0x82,
];

/// Build a FLAC METADATA PICTURE block body (the same structure used verbatim in a
/// FLAC `PICTURE` block and, base64-encoded, in a Vorbis `METADATA_BLOCK_PICTURE`
/// tag): picture type, MIME, description, dimensions, then the image. Big-endian.
pub fn flac_picture_block(png: &[u8]) -> Vec<u8> {
    let mime: &[u8] = b"image/png";
    let mut out = Vec::new();
    out.extend_from_slice(&3u32.to_be_bytes()); // type: front cover
    out.extend_from_slice(&u32::try_from(mime.len()).unwrap().to_be_bytes());
    out.extend_from_slice(mime);
    out.extend_from_slice(&0u32.to_be_bytes()); // description length (empty)
    out.extend_from_slice(&4u32.to_be_bytes()); // width
    out.extend_from_slice(&4u32.to_be_bytes()); // height
    out.extend_from_slice(&24u32.to_be_bytes()); // color depth
    out.extend_from_slice(&0u32.to_be_bytes()); // colors used (0 = non-indexed)
    out.extend_from_slice(&u32::try_from(png.len()).unwrap().to_be_bytes());
    out.extend_from_slice(png);
    out
}

/// All regular files under `dir`, recursively.
pub fn walk_tree(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                out.extend(walk_tree(&p));
            } else {
                out.push(p);
            }
        }
    }
    out
}
