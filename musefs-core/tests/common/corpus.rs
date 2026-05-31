//! Deterministic synthetic-library generator for the SP0 bench harness.
//! Shared by `#[ignore]`d timing tests and the read Criterion bench.

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
