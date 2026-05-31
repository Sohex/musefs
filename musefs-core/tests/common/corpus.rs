//! Deterministic synthetic-library generator for the SP0 bench harness.
//! Shared by `#[ignore]`d timing tests and the read Criterion bench.

use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    Ci,
    LargeCompute,
    Bandwidth,
    Custom,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Flac,
    Mp3,
    M4aMoovFirst,
    M4aMoovLast,
    Wav,
}

#[derive(Clone, Debug)]
pub struct CorpusParams {
    pub albums: usize,
    pub tracks_per_album: usize,
    /// Audio payload bytes per track (file size = payload + format front + art).
    pub bytes_per_track: usize,
    /// Embedded cover bytes per track (0 = no embedded art). One shared cover
    /// per album, so the content-addressed `art` table dedups across the album.
    pub art_bytes_per_track: usize,
    /// Round-robin formats. Default `[Flac]`.
    pub format_mix: Vec<Format>,
    pub seed: u64,
}

impl CorpusParams {
    pub fn track_count(&self) -> usize {
        self.albums * self.tracks_per_album
    }

    pub fn for_tier(t: Tier) -> Self {
        match t {
            // ~200 tracks, tiny, no art — runs in seconds.
            Tier::Ci => CorpusParams {
                albums: 20,
                tracks_per_album: 10,
                bytes_per_track: 4 * 1024,
                art_bytes_per_track: 0,
                format_mix: vec![Format::Flac],
                seed: 1,
            },
            // 100k tracks, ~8 KB payload + one shared ~30 KB cover/album (deduped).
            Tier::LargeCompute => CorpusParams {
                albums: 10_000,
                tracks_per_album: 10,
                bytes_per_track: 8 * 1024,
                art_bytes_per_track: 30 * 1024,
                format_mix: vec![Format::Flac],
                seed: 1,
            },
            // ~1k tracks, realistic payload — real-mount throughput.
            Tier::Bandwidth => CorpusParams {
                albums: 100,
                tracks_per_album: 10,
                bytes_per_track: 30 * 1024 * 1024,
                art_bytes_per_track: 200 * 1024,
                format_mix: vec![Format::Flac],
                seed: 1,
            },
            // Defaults equal to ci; every dimension is env-overridable.
            Tier::Custom => CorpusParams::for_tier(Tier::Ci),
        }
    }

    /// Read `MUSEFS_BENCH_TIER` (default `ci`) then apply any `MUSEFS_BENCH_*`
    /// overrides. `MUSEFS_BENCH_FORMAT_MIX` is a comma list of
    /// flac|mp3|m4a|m4a-last|wav.
    pub fn from_env() -> Self {
        let tier = match std::env::var("MUSEFS_BENCH_TIER").as_deref() {
            Ok("large-compute") => Tier::LargeCompute,
            Ok("bandwidth") => Tier::Bandwidth,
            Ok("custom") => Tier::Custom,
            _ => Tier::Ci,
        };
        let mut p = CorpusParams::for_tier(tier);
        if let Some(v) = env_usize("MUSEFS_BENCH_ALBUMS") {
            p.albums = v;
        }
        if let Some(v) = env_usize("MUSEFS_BENCH_TRACKS_PER_ALBUM") {
            p.tracks_per_album = v;
        }
        if let Some(v) = env_usize("MUSEFS_BENCH_BYTES_PER_TRACK") {
            p.bytes_per_track = v;
        }
        // One cover of this size is generated per album and embedded in each of
        // its tracks (the var names the per-album cover; the field holds its
        // byte size, embedded per track and deduped by the content-addressed
        // `art` table).
        if let Some(v) = env_usize("MUSEFS_BENCH_ART_PER_ALBUM") {
            p.art_bytes_per_track = v;
        }
        if let Some(v) = env_usize("MUSEFS_BENCH_SEED") {
            p.seed = v as u64;
        }
        if let Ok(mix) = std::env::var("MUSEFS_BENCH_FORMAT_MIX") {
            let parsed: Vec<Format> = mix
                .split(',')
                .filter_map(|s| match s.trim() {
                    "flac" => Some(Format::Flac),
                    "mp3" => Some(Format::Mp3),
                    "m4a" => Some(Format::M4aMoovFirst),
                    "m4a-last" => Some(Format::M4aMoovLast),
                    "wav" => Some(Format::Wav),
                    _ => None,
                })
                .collect();
            // An all-unrecognized value keeps the tier default rather than
            // erroring or yielding an empty mix.
            if !parsed.is_empty() {
                p.format_mix = parsed;
            }
        }
        p
    }
}

fn env_usize(key: &str) -> Option<usize> {
    std::env::var(key).ok().and_then(|s| s.parse().ok())
}

/// Deterministic filler audio: a seedable byte ramp (content is irrelevant —
/// `BackingAudio` is served verbatim and probing reads only headers).
fn filler(seed: u64, idx: usize, len: usize) -> Vec<u8> {
    // Knuth multiplicative hash constant (⌊φ · 2³²⌋) to spread the seed bits.
    let base = seed.wrapping_add(idx as u64).wrapping_mul(2_654_435_761);
    (0..len)
        .map(|i| (base.wrapping_add(i as u64) & 0xFF) as u8)
        .collect()
}

/// One shared cover per album so the content-addressed `art` table dedups.
fn cover(seed: u64, album: usize, len: usize) -> Vec<u8> {
    filler(
        seed ^ 0x00C0_FFEE,
        album.wrapping_mul(101).wrapping_add(1),
        len,
    )
}

/// A FLAC with STREAMINFO + comments + (optionally) a PICTURE block + audio.
/// Mirrors `tests/common/scan.rs`'s `flac_with_picture` layout.
fn flac_bytes(comments: &[String], art: Option<&[u8]>, audio: &[u8]) -> Vec<u8> {
    use super::{flac_block, streaminfo_body, vorbis_comment_body};
    let refs: Vec<&str> = comments.iter().map(String::as_str).collect();
    let mut out = Vec::new();
    out.extend_from_slice(b"fLaC");
    out.extend_from_slice(&flac_block(0, &streaminfo_body(), false));
    let last_meta = art.is_none();
    out.extend_from_slice(&flac_block(
        4,
        &vorbis_comment_body("musefs-bench", &refs),
        last_meta,
    ));
    if let Some(img) = art {
        let mut body = Vec::new();
        body.extend_from_slice(&3u32.to_be_bytes()); // picture type: front cover
        body.extend_from_slice(&(b"image/png".len() as u32).to_be_bytes());
        body.extend_from_slice(b"image/png");
        body.extend_from_slice(&0u32.to_be_bytes()); // description len
        body.extend_from_slice(&0u32.to_be_bytes()); // width
        body.extend_from_slice(&0u32.to_be_bytes()); // height
        body.extend_from_slice(&0u32.to_be_bytes()); // depth
        body.extend_from_slice(&0u32.to_be_bytes()); // colors
        body.extend_from_slice(&(img.len() as u32).to_be_bytes());
        body.extend_from_slice(img);
        out.extend_from_slice(&flac_block(6, &body, true));
    }
    out.extend_from_slice(audio);
    out
}

/// Generate the corpus into `dir` (created if missing). Layout is
/// `dir/album-{a}/track-{t}.{ext}`. Returns created file paths in stable order.
pub fn generate(dir: &Path, p: &CorpusParams) -> Vec<PathBuf> {
    std::fs::create_dir_all(dir).unwrap();
    let mut paths = Vec::with_capacity(p.track_count());
    let mut idx = 0usize;
    for album in 0..p.albums {
        let adir = dir.join(format!("album-{album:05}"));
        std::fs::create_dir_all(&adir).unwrap();
        let art_blob =
            (p.art_bytes_per_track > 0).then(|| cover(p.seed, album, p.art_bytes_per_track));
        for track in 0..p.tracks_per_album {
            let fmt = p.format_mix[idx % p.format_mix.len()];
            let audio = filler(p.seed, idx, p.bytes_per_track);
            let comments = vec![
                format!("ARTIST=Artist {album:05}"),
                format!("ALBUM=Album {album:05}"),
                format!("TITLE=Track {track:03}"),
            ];
            let path = generate_one(&adir, idx, fmt, &comments, art_blob.as_deref(), &audio);
            paths.push(path);
            idx += 1;
        }
    }
    paths
}

/// `comments` and `art` are only consumed by [`Format::Flac`]; the other formats
/// carry tags via the DB at scan time and have no embedded-art builder, so they
/// ignore both here.
fn generate_one(
    adir: &Path,
    idx: usize,
    fmt: Format,
    comments: &[String],
    art: Option<&[u8]>,
    audio: &[u8],
) -> PathBuf {
    match fmt {
        Format::Flac => {
            let path = adir.join(format!("track-{idx:06}.flac"));
            std::fs::write(&path, flac_bytes(comments, art, audio)).unwrap();
            path
        }
        Format::Mp3 => {
            let path = adir.join(format!("track-{idx:06}.mp3"));
            super::write_mp3(&path, audio);
            path
        }
        Format::M4aMoovFirst => {
            let path = adir.join(format!("track-{idx:06}.m4a"));
            super::write_m4a(&path, audio);
            path
        }
        Format::M4aMoovLast => {
            let path = adir.join(format!("track-{idx:06}.m4a"));
            super::write_m4a_moov_last(&path, audio);
            path
        }
        Format::Wav => {
            let path = adir.join(format!("track-{idx:06}.wav"));
            super::write_wav(&path, audio);
            path
        }
    }
}
