use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;

use musefs_db::{Db, Format};
use musefs_format::flac::{self, MetadataBlock};
use musefs_format::{mp3, mp4, wav, BinaryTagInput, RegionLayout, Segment};
use quick_cache::sync::Cache;
use quick_cache::Weighter;

use crate::error::{CoreError, Result};
use crate::facade::Mode;
use crate::mapping::{tags_to_inputs, track_art_to_inputs};
use crate::ogg_index::serve_ogg_window;

/// A fully resolved synthesized file: its segment layout, total size, the
/// content version it was built from, and where the backing audio lives.
#[derive(Debug)]
pub struct ResolvedFile {
    pub layout: RegionLayout,
    pub total_len: u64,
    pub content_version: i64,
    pub backing_path: PathBuf,
    pub backing_size: u64,
    pub backing_mtime_secs: i64,
    pub mtime_secs: i64,
    /// One-entry memo of the last patched Ogg page, so consecutive reads skip
    /// re-patching the page straddling a chunk boundary. Empty for non-Ogg files
    /// and reset whenever this resolved entry is rebuilt. (Concrete type spelled
    /// out rather than `ogg_index::LastPageMemo` because that module is private.)
    pub last_page: Mutex<Option<(u64, u64, Vec<u8>)>>,
    /// Approximate resident bytes this entry costs the cache (sum of `Inline`
    /// segment bytes; backing/art/ogg-audio bytes are not resident).
    pub cache_bytes: u64,
    /// Precomputed from the layout: true if any segment streams an opaque binary
    /// tag payload from the DB. Gates the transactional `content_version` guard in
    /// the read fast path so plain Inline/BackingAudio layouts pay no per-read cost.
    pub has_binary_tag: bool,
}

/// Weighs an entry by its resident inline bytes. The `.max(1)` is load-bearing:
/// quick_cache ignores zero-weight entries when evicting, and every
/// StructureOnly layout has `cache_bytes == 0`, so an unweighted entry would
/// escape the byte budget entirely.
#[derive(Clone)]
struct CacheBytesWeighter;

impl Weighter<i64, Arc<ResolvedFile>> for CacheBytesWeighter {
    fn weight(&self, _key: &i64, val: &Arc<ResolvedFile>) -> u64 {
        val.cache_bytes.max(1)
    }
}

/// A per-mount cache of resolved files keyed by track id; an entry
/// self-invalidates when the track's `content_version` changes. Backed by
/// quick_cache: S3-FIFO eviction, byte-weighted, internally sharded.
pub struct HeaderCache {
    cache: Cache<i64, Arc<ResolvedFile>, CacheBytesWeighter>,
    mode: Mode,
}

/// Default resident-bytes budget for the header cache (64 MiB).
pub const DEFAULT_CACHE_BUDGET: u64 = 64 * 1024 * 1024;

/// Item-count sizing hint for quick_cache's internal structures (not a bound):
/// the default budget over 4 KiB, a typical inline tag region. The hint has no
/// observable public-API behavior, so its arithmetic carries an equivalent-mutant
/// exclusion in .cargo/mutants.toml (cargo-mutants does mutate const initializers).
const CACHE_ESTIMATED_ITEMS: usize = (DEFAULT_CACHE_BUDGET / 4096) as usize;

fn mtime_secs(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs() as i64)
}

fn read_front(path: &Path, n: u64) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    crate::metrics::on_open();
    let mut f = std::fs::File::open(path)?;
    let mut buf = vec![0u8; n as usize];
    f.read_exact(&mut buf)?;
    Ok(buf)
}

impl HeaderCache {
    pub fn new(mode: Mode) -> HeaderCache {
        HeaderCache::with_budget(mode, DEFAULT_CACHE_BUDGET)
    }
    pub fn with_budget(mode: Mode, budget: u64) -> HeaderCache {
        HeaderCache {
            cache: Cache::with_weighter(CACHE_ESTIMATED_ITEMS, budget, CacheBytesWeighter),
            mode,
        }
    }
    /// Drop cached resolutions for tracks no longer present (`live` = current ids).
    pub fn retain(&self, live: &HashSet<i64>) {
        self.cache.retain(|id, _| live.contains(id));
    }
    /// Drop one track's cached resolution (changelog-refresh removal path).
    pub fn remove(&self, id: i64) {
        self.cache.remove(&id);
    }
    /// Resolve a track to its layout, caching on a content-version miss. Validation
    /// (`stat`) and synthesis run outside the cache; quick_cache's internal locks
    /// are only touched by the brief get and insert.
    pub fn resolve<M>(&self, db: &Db<M>, track_id: i64) -> Result<Arc<ResolvedFile>> {
        let track = db
            .get_track(track_id)?
            .ok_or(CoreError::TrackNotFound(track_id))?;

        // Always validate the backing file first — a stale file is an error even
        // on a cache hit, because the audio region may have shifted.
        crate::metrics::on_stat();
        let meta = std::fs::metadata(&track.backing_path)?;
        if meta.len() != track.backing_size as u64 || mtime_secs(&meta) != track.backing_mtime {
            return Err(CoreError::BackingChanged(track.backing_path.clone()));
        }

        if let Some(hit) = self.cache.get(&track_id) {
            if hit.content_version == track.content_version {
                return Ok(hit);
            }
        }
        let resolved = self.build(db, &track, &meta)?;
        self.cache.insert(track_id, resolved.clone());
        Ok(resolved)
    }
    /// Build a `ResolvedFile` for `track` (synthesis or passthrough). No lock held.
    fn build<M>(
        &self,
        db: &Db<M>,
        track: &musefs_db::Track,
        meta: &std::fs::Metadata,
    ) -> Result<Arc<ResolvedFile>> {
        let (layout, total_len, mtime_secs_val) = match self.mode {
            Mode::StructureOnly => {
                // Pure passthrough: the synthesized "file" is the backing file itself.
                // The stored audio bounds are irrelevant here — the whole file is served
                // verbatim — so they are not validated in this mode.
                let layout = RegionLayout::new(vec![Segment::BackingAudio {
                    offset: 0,
                    len: meta.len(),
                }]);
                (layout, meta.len(), track.backing_mtime)
            }
            Mode::Synthesis => {
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

                let tags = db.get_tags(track.id)?;
                let inputs = tags_to_inputs(&tags);
                let art_inputs = track_art_to_inputs(db, track.id)?;
                let binary_tag_inputs = crate::mapping::binary_tags_to_inputs(db, track.id)?;

                // FLAC re-reads the front for its preserved structural blocks; MP3 needs no
                // front read — its ID3v2 tag is regenerated entirely from the DB and the
                // Xing/LAME info frame travels with the backing audio.
                let layout = match track.format {
                    Format::Flac => {
                        let rows = db.get_structural_blocks(track.id)?;
                        // Fast path: the structural store holds STREAMINFO/SEEKTABLE and
                        // APPLICATION/CUESHEET stream from value_blob rows. Legacy
                        // fallback (no structural rows yet): carry every preserved block
                        // — including APPLICATION/CUESHEET — inline from the front
                        // re-read, and suppress the streamed binary tags so those blocks
                        // are not emitted twice.
                        let (structural, binary_tags): (Vec<MetadataBlock>, &[BinaryTagInput]) =
                            if rows.is_empty() {
                                let front = read_front(
                                    Path::new(&track.backing_path),
                                    track.audio_offset as u64,
                                )?;
                                (flac::read_metadata(&front)?.preserved, &[])
                            } else {
                                let structural = rows
                                    .into_iter()
                                    .filter_map(|b| {
                                        flac::structural_block_type(&b.kind).map(|block_type| {
                                            MetadataBlock {
                                                block_type,
                                                body: b.body,
                                            }
                                        })
                                    })
                                    .collect();
                                (structural, &binary_tag_inputs)
                            };
                        flac::synthesize_layout(
                            &structural,
                            track.audio_offset as u64,
                            track.audio_length as u64,
                            &inputs,
                            binary_tags,
                            &art_inputs,
                        )?
                    }
                    Format::Mp3 => mp3::synthesize_layout(
                        track.audio_offset as u64,
                        track.audio_length as u64,
                        &inputs,
                        &binary_tag_inputs,
                        &art_inputs,
                    )?,
                    Format::M4a => {
                        // Read only the structural boxes (ftyp/moov/mdat header) by
                        // seeking — never the (potentially hundreds-of-MB) mdat payload,
                        // which is served from the backing file at read time. The `moov`
                        // box may sit at EOF; the streaming reader skips the mdat payload
                        // to reach it. The resulting layout's leading inline `head` ends
                        // in a deliberately truncated `mdat` header whose payload is the
                        // backing-audio tail.
                        let mut f = std::fs::File::open(&track.backing_path)?;
                        // `meta` was validated against the tracked size/mtime above,
                        // so reuse it rather than issuing a second fstat.
                        let len = meta.len();
                        let scan = mp4::read_structure_from(&mut f, len).map_err(|e| match e {
                            mp4::Mp4ScanError::Io(io) => CoreError::Io(io),
                            mp4::Mp4ScanError::Format(fe) => CoreError::Format(fe),
                            // Unreachable in practice (an ingested file already
                            // passed the cap at scan, and backing-file drift is
                            // caught by the size/mtime BackingChanged guard first),
                            // but preserve the box/size/cap diagnostics rather than
                            // erasing them into a generic Malformed.
                            mp4::Mp4ScanError::MetadataTooLarge {
                                box_kind,
                                size,
                                cap,
                            } => CoreError::Mp4MetadataTooLarge {
                                box_kind,
                                size,
                                cap,
                            },
                        })?;
                        mp4::synthesize_layout(&scan, &inputs, &binary_tag_inputs, &art_inputs)?
                    }
                    Format::Wav => {
                        // Read only the front (RIFF header + fmt/fact); the data
                        // payload is served from the backing file at read time.
                        let front =
                            read_front(Path::new(&track.backing_path), track.audio_offset as u64)?;
                        let scan = wav::read_structure(&front)?;
                        wav::synthesize_layout(
                            &scan,
                            track.audio_offset as u64,
                            track.audio_length as u64,
                            &inputs,
                            &binary_tag_inputs,
                            &art_inputs,
                        )?
                    }
                    Format::Opus | Format::Vorbis | Format::OggFlac => {
                        let front =
                            read_front(Path::new(&track.backing_path), track.audio_offset as u64)?;
                        let header = musefs_format::ogg::read_metadata(&front)?;
                        let art_images = crate::mapping::track_art_images(db, &art_inputs)?;
                        let arts: Vec<musefs_format::ogg::OggArt> = art_inputs
                            .iter()
                            .zip(art_images.iter())
                            .map(|(meta, image)| musefs_format::ogg::OggArt {
                                meta,
                                image: image.as_slice(),
                            })
                            .collect();
                        musefs_format::ogg::synthesize_layout(
                            &header,
                            track.audio_offset as u64,
                            track.audio_length as u64,
                            &inputs,
                            &arts,
                        )?
                    }
                };
                let total = layout.total_len();
                (layout, total, track.backing_mtime.max(track.updated_at))
            }
        };

        let cache_bytes = layout
            .segments()
            .iter()
            .map(|s| match s {
                Segment::Inline(b) => b.len() as u64,
                _ => 0,
            })
            .sum::<u64>();
        let has_binary_tag = layout.has_binary_tag();
        Ok(Arc::new(ResolvedFile {
            layout,
            total_len,
            content_version: track.content_version,
            backing_path: PathBuf::from(&track.backing_path),
            backing_size: track.backing_size as u64,
            backing_mtime_secs: track.backing_mtime,
            mtime_secs: mtime_secs_val,
            last_page: Mutex::new(None),
            cache_bytes,
            has_binary_tag,
        }))
    }
}

/// Read `size` bytes at virtual `offset` into `out` (appended), opening the
/// backing file once for this call if the layout needs it.
pub fn read_at_into<M>(
    resolved: &ResolvedFile,
    db: &Db<M>,
    offset: u64,
    size: u64,
    out: &mut Vec<u8>,
) -> Result<()> {
    if offset >= resolved.total_len || size == 0 {
        return Ok(());
    }
    let needs_file = resolved
        .layout
        .segments
        .iter()
        .any(|s| matches!(s, Segment::BackingAudio { .. } | Segment::OggAudio { .. }));
    if needs_file {
        crate::metrics::on_open();
        let file = std::fs::File::open(&resolved.backing_path)?;
        read_segments_into(resolved, db, Some(&file), offset, size, out)
    } else {
        read_segments_into(resolved, db, None, offset, size, out)
    }
}

/// Allocating form of `read_at_into` (tests and non-hot-path callers).
pub fn read_at<M>(resolved: &ResolvedFile, db: &Db<M>, offset: u64, size: u64) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    read_at_into(resolved, db, offset, size, &mut out)?;
    Ok(out)
}

/// The single segment-splicing loop. `file` is `Some` whenever the layout has a
/// `BackingAudio`/`OggAudio` segment (guaranteed by `read_at`/`read_at_with_file`);
/// the backing arms treat `None` as a contract violation.
fn read_segments_into<M>(
    resolved: &ResolvedFile,
    db: &Db<M>,
    file: Option<&std::fs::File>,
    offset: u64,
    size: u64,
    out: &mut Vec<u8>,
) -> Result<()> {
    use std::os::unix::fs::FileExt;

    if offset >= resolved.total_len || size == 0 {
        return Ok(());
    }
    let end = offset.saturating_add(size).min(resolved.total_len);
    out.reserve((end - offset) as usize);

    let mut seg_start = 0u64;
    for seg in &resolved.layout.segments {
        let seg_len = seg.len();
        let seg_end = seg_start + seg_len;
        let ov_start = offset.max(seg_start);
        let ov_end = end.min(seg_end);
        if ov_start < ov_end {
            let within = ov_start - seg_start;
            let n = (ov_end - ov_start) as usize;
            match seg {
                Segment::Inline(bytes) => {
                    let w = within as usize;
                    out.extend_from_slice(&bytes[w..w + n]);
                }
                Segment::BackingAudio { offset: bo, .. } => {
                    let f = file.expect("backing segment requires an open backing file");
                    // Finding #15 (ESTALE, untested by design): on an NFS-backed mount a stale file
                    // handle surfaces here as a raw io::Error from the positioned read (or as
                    // BackingChanged from the size/mtime re-validation) and is propagated verbatim
                    // through the FUSE layer. There is no test-framework support to inject NFS ESTALE,
                    // so this path is documented rather than covered.
                    let start = out.len();
                    out.resize(start + n, 0);
                    f.read_exact_at(&mut out[start..], bo + within)?;
                    crate::metrics::on_pread(n as u64);
                }
                Segment::ArtImage { art_id, .. } => {
                    let start = out.len();
                    out.resize(start + n, 0);
                    db.read_art_chunk_into(*art_id, within, &mut out[start..])?;
                    crate::metrics::on_art_chunk();
                }
                Segment::BinaryTag { payload_id, .. } => {
                    let start = out.len();
                    out.resize(start + n, 0);
                    db.read_binary_tag_chunk_into(*payload_id, within, &mut out[start..])?;
                    crate::metrics::on_binary_tag_chunk();
                }
                Segment::OggAudio {
                    offset: ao,
                    seq_delta,
                    len,
                } => {
                    let f = file.expect("ogg-audio segment requires an open backing file");
                    serve_ogg_window(
                        f,
                        *ao,
                        *len,
                        *seq_delta,
                        within,
                        within + n as u64,
                        &mut *out,
                        Some(&resolved.last_page),
                    )?;
                }
                Segment::OggArtSlice {
                    art_id,
                    offset,
                    base64,
                    art_total,
                    ..
                } => {
                    if *base64 {
                        // Output base64 chars [offset+within, +n) of base64(image).
                        let w =
                            musefs_format::ogg::b64_window(*offset + within, n as u64, *art_total);
                        let raw = db.read_art_chunk(*art_id, w.in_start, w.in_len as usize)?;
                        crate::metrics::on_art_chunk();
                        out.extend_from_slice(&musefs_format::ogg::encode_b64_slice(
                            &raw, w.skip, n,
                        ));
                    } else {
                        // Raw image bytes (OggFLAC PICTURE block).
                        let start = out.len();
                        out.resize(start + n, 0);
                        db.read_art_chunk_into(*art_id, *offset + within, &mut out[start..])?;
                        crate::metrics::on_art_chunk();
                    }
                }
            }
        }
        seg_start = seg_end;
        if seg_start >= end {
            break;
        }
    }
    Ok(())
}

/// Serve into `out` from an already-open backing `file` (per-handle path).
pub fn read_at_with_file_into<M>(
    resolved: &ResolvedFile,
    db: &Db<M>,
    file: &std::fs::File,
    offset: u64,
    size: u64,
    out: &mut Vec<u8>,
) -> Result<()> {
    read_segments_into(resolved, db, Some(file), offset, size, out)
}

/// Allocating form of `read_at_with_file_into`.
pub fn read_at_with_file<M>(
    resolved: &ResolvedFile,
    db: &Db<M>,
    file: &std::fs::File,
    offset: u64,
    size: u64,
) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    read_at_with_file_into(resolved, db, file, offset, size, &mut out)?;
    Ok(out)
}

#[cfg(test)]
mod ogg_serve_tests {
    use super::*;
    use musefs_format::ogg::page_test_support::lace_packet_pub;
    use musefs_format::Segment;
    use std::io::Write;

    #[test]
    fn read_at_renumbers_audio_and_preserves_payload() {
        // Build a file: 8 header bytes + two audio pages (seq 3,4).
        let (mut audio, _) = lace_packet_pub(0x99, 3, false, 10, &[0xA1u8; 200]);
        let (a2, _) = lace_packet_pub(0x99, 4, false, 20, &vec![0xB2u8; 250]);
        audio.extend_from_slice(&a2);
        let audio_offset = 8u64;
        let mut file_bytes = vec![0xFFu8; audio_offset as usize];
        file_bytes.extend_from_slice(&audio);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.opus");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(&file_bytes)
            .unwrap();

        let layout = RegionLayout::new(vec![
            Segment::Inline(b"HDRBYTES".to_vec()), // 8 inline header bytes
            Segment::OggAudio {
                offset: audio_offset,
                len: audio.len() as u64,
                seq_delta: 1, // 3->4, 4->5
            },
        ]);
        let total = layout.total_len();
        let resolved = ResolvedFile {
            layout,
            total_len: total,
            content_version: 0,
            backing_path: path.clone(),
            backing_size: 0,
            backing_mtime_secs: 0,
            mtime_secs: 0,
            last_page: Mutex::new(None),
            cache_bytes: 8,
            has_binary_tag: false,
        };

        // Read the whole virtual file; needs a Db only for ArtImage (unused here).
        let db = musefs_db::Db::open_in_memory().unwrap();
        let got = read_at(&resolved, &db, 0, total).unwrap();
        assert_eq!(got.len(), total as usize);
        assert_eq!(&got[0..8], b"HDRBYTES");

        // The served audio region must have renumbered seqs (4 and 5) and identical
        // payloads to the source.
        let served_audio = &got[8..];
        let h0 = musefs_format::ogg::parse_page(served_audio, 0).unwrap();
        assert_eq!(h0.seq, 4);
        let p1_off = h0.total_len();
        let h1 = musefs_format::ogg::parse_page(served_audio, p1_off).unwrap();
        assert_eq!(h1.seq, 5);
        // Payload bytes unchanged.
        assert!(served_audio[h0.header_len..h0.total_len()]
            .iter()
            .all(|&b| b == 0xA1));
        assert!(
            served_audio[p1_off + h1.header_len..p1_off + h1.total_len()]
                .iter()
                .all(|&b| b == 0xB2)
        );
    }
}

#[cfg(test)]
mod resolve_ogg_tests {
    use super::*;
    use musefs_db::{Db, Format, NewTrack, Tag};
    use musefs_format::ogg::page_test_support::lace_packet_pub;
    use std::io::Write;

    fn build_opus_file(path: &std::path::Path) -> (u64, u64) {
        let head = b"OpusHead\x01\x02\x38\x01\x80\xbb\x00\x00\x00\x00\x00".to_vec();
        let mut tags = b"OpusTags".to_vec();
        tags.extend_from_slice(&musefs_format::ogg::page_test_support::vorbis_body_empty());
        let (mut bytes, pages) =
            musefs_format::ogg::page_test_support::build_header_pub(0x1234, &[&head, &tags]);
        let audio_offset = bytes.len() as u64;
        let _ = pages;
        let (audio, _) = lace_packet_pub(0x1234, 2, false, 960, &vec![0x7Eu8; 400]);
        bytes.extend_from_slice(&audio);
        std::fs::File::create(path)
            .unwrap()
            .write_all(&bytes)
            .unwrap();
        (audio_offset, bytes.len() as u64 - audio_offset)
    }

    #[test]
    fn resolves_and_reads_opus_with_identical_audio() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("track.opus");
        let (audio_offset, audio_length) = build_opus_file(&path);
        let original = std::fs::read(&path).unwrap();

        let db = Db::open_in_memory().unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        let track_id = db
            .upsert_track(&NewTrack {
                backing_path: path.to_string_lossy().to_string(),
                format: Format::Opus,
                audio_offset: audio_offset as i64,
                audio_length: audio_length as i64,
                backing_size: meta.len() as i64,
                backing_mtime: mtime_secs(&meta),
            })
            .unwrap();
        db.replace_tags(track_id, &[Tag::new("title", "Telephasic Workshop", 0)])
            .unwrap();

        let cache = HeaderCache::new(Mode::Synthesis);
        let resolved = cache.resolve(&db, track_id).unwrap();
        let out = read_at(&resolved, &db, 0, resolved.total_len).unwrap();

        // The synthesized audio region (after the regenerated header) must be the
        // original audio pages, byte-identical (seq_delta==0 here since the original
        // OpusTags is also an empty-comment musefs-style header of equal page count).
        let header = musefs_format::ogg::read_header(&out).unwrap();
        let synth_audio = &out[header.audio_offset as usize..];
        assert_eq!(synth_audio, &original[audio_offset as usize..]);

        // Tags were rewritten. `ogg::read_tags` now returns canonical lowercase
        // keys for known Vorbis fields (Tasks 1–6 changed the format layer).
        let tags = musefs_format::ogg::read_tags(&out).unwrap();
        assert!(tags
            .iter()
            .any(|(k, v)| k == "title" && v == "Telephasic Workshop"));
    }

    #[test]
    fn read_at_with_file_matches_read_at() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("track.opus");
        let (audio_offset, audio_length) = build_opus_file(&path);
        let db = Db::open_in_memory().unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        let track_id = db
            .upsert_track(&NewTrack {
                backing_path: path.to_string_lossy().to_string(),
                format: Format::Opus,
                audio_offset: audio_offset as i64,
                audio_length: audio_length as i64,
                backing_size: meta.len() as i64,
                backing_mtime: mtime_secs(&meta),
            })
            .unwrap();
        let cache = HeaderCache::new(Mode::Synthesis);
        let resolved = cache.resolve(&db, track_id).unwrap();

        let via_open = read_at(&resolved, &db, 0, resolved.total_len).unwrap();
        let file = std::fs::File::open(&resolved.backing_path).unwrap();
        let via_file = read_at_with_file(&resolved, &db, &file, 0, resolved.total_len).unwrap();
        assert_eq!(via_open, via_file);
    }

    fn build_wav_file(path: &std::path::Path) -> (u64, u64, Vec<u8>) {
        use std::io::Write;
        let mut fmt = Vec::new();
        fmt.extend_from_slice(&1u16.to_le_bytes());
        fmt.extend_from_slice(&1u16.to_le_bytes());
        fmt.extend_from_slice(&44_100u32.to_le_bytes());
        fmt.extend_from_slice(&88_200u32.to_le_bytes());
        fmt.extend_from_slice(&2u16.to_le_bytes());
        fmt.extend_from_slice(&16u16.to_le_bytes());

        let data: Vec<u8> = (0..32u8).collect();
        let mut body = Vec::new();
        for (id, payload) in [(&b"fmt "[..], &fmt[..]), (&b"data"[..], &data[..])] {
            body.extend_from_slice(id);
            body.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            body.extend_from_slice(payload);
        }
        let mut bytes = b"RIFF".to_vec();
        bytes.extend_from_slice(&((body.len() + 4) as u32).to_le_bytes());
        bytes.extend_from_slice(b"WAVE");
        bytes.extend_from_slice(&body);

        let audio_offset = (bytes.len() - data.len()) as u64;
        std::fs::File::create(path)
            .unwrap()
            .write_all(&bytes)
            .unwrap();
        (audio_offset, data.len() as u64, data)
    }

    #[test]
    fn resolves_and_reads_wav_with_identical_audio() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("track.wav");
        let (audio_offset, audio_length, original_data) = build_wav_file(&path);

        let db = Db::open_in_memory().unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        let track_id = db
            .upsert_track(&NewTrack {
                backing_path: path.to_string_lossy().to_string(),
                format: Format::Wav,
                audio_offset: audio_offset as i64,
                audio_length: audio_length as i64,
                backing_size: meta.len() as i64,
                backing_mtime: mtime_secs(&meta),
            })
            .unwrap();
        db.replace_tags(track_id, &[Tag::new("title", "Wave One", 0)])
            .unwrap();

        let cache = HeaderCache::new(Mode::Synthesis);
        let resolved = cache.resolve(&db, track_id).unwrap();
        let out = read_at(&resolved, &db, 0, resolved.total_len).unwrap();

        // The synthesized output is a valid WAV; its data payload is byte-identical
        // to the original audio.
        let bounds = musefs_format::wav::locate_audio(&out).unwrap();
        assert_eq!(
            &out[bounds.audio_offset as usize
                ..(bounds.audio_offset + bounds.audio_length) as usize],
            original_data.as_slice()
        );

        // The title was synthesized into the embedded id3 chunk.
        let tags = musefs_format::wav::read_tags(&out);
        assert!(tags.contains(&("title".to_string(), "Wave One".to_string())));
    }

    #[test]
    fn build_cache_bytes_counts_inline_segments_for_ogg() {
        use musefs_db::{Format, NewTrack};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.opus");
        let (audio_offset, audio_length) = build_opus_file(&path);
        let db = musefs_db::Db::open_in_memory().unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        let id = db
            .upsert_track(&NewTrack {
                backing_path: path.to_string_lossy().to_string(),
                format: Format::Opus,
                audio_offset: audio_offset as i64,
                audio_length: audio_length as i64,
                backing_size: meta.len() as i64,
                backing_mtime: mtime_secs(&meta),
            })
            .unwrap();
        let cache = HeaderCache::new(Mode::Synthesis);
        let resolved = cache.resolve(&db, id).unwrap();
        let inline_sum: u64 = resolved
            .layout
            .segments()
            .iter()
            .map(|s| match s {
                Segment::Inline(b) => b.len() as u64,
                _ => 0,
            })
            .sum();
        // SP4: no per-file index estimate; cache_bytes == inline segment bytes only.
        assert_eq!(resolved.cache_bytes, inline_sum);
        assert!(
            inline_sum > 0,
            "Opus header should have non-empty inline segments"
        );
    }
}

#[cfg(test)]
mod ogg_art_serve_tests {
    use super::*;

    #[test]
    fn read_at_serves_base64_art_slice_matching_full_encode() {
        let image: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
        // Compute full base64 via the format crate (base64 is not a direct dep of musefs-core).
        let full_b64 = musefs_format::ogg::encode_b64_slice(
            &image,
            0,
            musefs_format::ogg::b64_len(image.len() as u64) as usize,
        );

        let db = musefs_db::Db::open_in_memory().unwrap();
        let art_id = db
            .upsert_art(&musefs_db::NewArt {
                mime: "image/png".to_string(),
                width: Some(1),
                height: Some(1),
                data: image.clone(),
            })
            .unwrap();

        let layout = RegionLayout::new(vec![
            Segment::Inline(b"HEAD".to_vec()),
            Segment::OggArtSlice {
                art_id,
                offset: 0,
                len: full_b64.len() as u64,
                base64: true,
                art_total: image.len() as u64,
            },
            Segment::Inline(b"XY".to_vec()),
        ]);
        let total = layout.total_len();
        let resolved = ResolvedFile {
            layout,
            total_len: total,
            content_version: 0,
            backing_path: std::path::PathBuf::from("/dev/null"),
            backing_size: 0,
            backing_mtime_secs: 0,
            mtime_secs: 0,
            last_page: Mutex::new(None),
            cache_bytes: 0,
            has_binary_tag: false,
        };

        // Full read.
        let got = read_at(&resolved, &db, 0, total).unwrap();
        let mut want = b"HEAD".to_vec();
        want.extend_from_slice(&full_b64);
        want.extend_from_slice(b"XY");
        assert_eq!(got, want);

        // Partial read straddling into the middle of the art slice (non-4-aligned).
        let part = read_at(&resolved, &db, 7, 23).unwrap();
        assert_eq!(part, want[7..30]);
    }

    #[test]
    fn read_at_serves_raw_art_slice() {
        let image: Vec<u8> = (0..300u32).map(|i| (i % 256) as u8).collect();
        let db = musefs_db::Db::open_in_memory().unwrap();
        let art_id = db
            .upsert_art(&musefs_db::NewArt {
                mime: "image/png".to_string(),
                width: None,
                height: None,
                data: image.clone(),
            })
            .unwrap();
        let layout = RegionLayout::new(vec![Segment::OggArtSlice {
            art_id,
            offset: 0,
            len: image.len() as u64,
            base64: false,
            art_total: image.len() as u64,
        }]);
        let total = layout.total_len();
        let resolved = ResolvedFile {
            layout,
            total_len: total,
            content_version: 0,
            backing_path: std::path::PathBuf::from("/dev/null"),
            backing_size: 0,
            backing_mtime_secs: 0,
            mtime_secs: 0,
            last_page: Mutex::new(None),
            cache_bytes: 0,
            has_binary_tag: false,
        };
        let got = read_at(&resolved, &db, 10, 50).unwrap();
        assert_eq!(got, image[10..60]);
    }
}

#[cfg(test)]
mod cache_bound_tests {
    use super::*;
    use musefs_db::{Db, Format, NewTrack};

    fn entry(content_version: i64, inline_len: usize) -> Arc<ResolvedFile> {
        Arc::new(ResolvedFile {
            layout: RegionLayout::new(vec![Segment::Inline(vec![0u8; inline_len])]),
            total_len: inline_len as u64,
            content_version,
            backing_path: std::path::PathBuf::from("/nonexistent"),
            backing_size: 0,
            backing_mtime_secs: 0,
            mtime_secs: 0,
            last_page: Mutex::new(None),
            cache_bytes: inline_len as u64,
            has_binary_tag: false,
        })
    }

    #[test]
    fn header_cache_resolve_caches_by_content_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.flac");
        let (audio_offset, audio_length) = write_flac_local(&path);
        let db = Db::open_in_memory().unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        let id = db
            .upsert_track(&NewTrack {
                backing_path: path.to_string_lossy().to_string(),
                format: Format::Flac,
                audio_offset,
                audio_length,
                backing_size: meta.len() as i64,
                backing_mtime: mtime_secs(&meta),
            })
            .unwrap();
        let cache = HeaderCache::new(Mode::Synthesis); // NOTE: not `mut` — resolve is &self now
        let a = cache.resolve(&db, id).unwrap();
        let b = cache.resolve(&db, id).unwrap();
        assert!(Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn resolve_is_safe_under_concurrent_access() {
        // Many threads resolving the same track exercise the off-lock build race
        // (concurrent miss → build → insert on one shard) and concurrent gets.
        // Each thread needs its own connection (Db is !Sync), so use a file-backed
        // DB and open_readonly per thread.
        let dir = tempfile::tempdir().unwrap();
        let flac_path = dir.path().join("a.flac");
        let (audio_offset, audio_length) = write_flac_local(&flac_path);
        let db_path = dir.path().join("m.db");
        let track_id = {
            let db = Db::open(&db_path).unwrap();
            let meta = std::fs::metadata(&flac_path).unwrap();
            db.upsert_track(&NewTrack {
                backing_path: flac_path.to_string_lossy().to_string(),
                format: Format::Flac,
                audio_offset,
                audio_length,
                backing_size: meta.len() as i64,
                backing_mtime: mtime_secs(&meta),
            })
            .unwrap()
        };

        let cache = std::sync::Arc::new(HeaderCache::new(Mode::Synthesis));
        std::thread::scope(|s| {
            for _ in 0..8 {
                let cache = std::sync::Arc::clone(&cache);
                let db_path = db_path.clone();
                s.spawn(move || {
                    let db = Db::open_readonly(&db_path).unwrap();
                    for _ in 0..50 {
                        let r = cache.resolve(&db, track_id).unwrap();
                        assert!(r.total_len > 0);
                        assert_eq!(r.content_version, 0);
                    }
                });
            }
        });
    }

    #[test]
    fn header_cache_retain_drops_absent_tracks() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_in_memory().unwrap();
        let mk = |name: &str| {
            let path = dir.path().join(name);
            let (audio_offset, audio_length) = write_flac_local(&path);
            let meta = std::fs::metadata(&path).unwrap();
            db.upsert_track(&NewTrack {
                backing_path: path.to_string_lossy().to_string(),
                format: Format::Flac,
                audio_offset,
                audio_length,
                backing_size: meta.len() as i64,
                backing_mtime: mtime_secs(&meta),
            })
            .unwrap()
        };
        let keep = mk("keep.flac");
        let gone = mk("gone.flac");
        let cache = HeaderCache::new(Mode::Synthesis);
        let keep_a = cache.resolve(&db, keep).unwrap();
        let gone_a = cache.resolve(&db, gone).unwrap();

        let live: HashSet<i64> = [keep].into_iter().collect();
        cache.retain(&live);

        // The kept track stays the same cached Arc; the dropped one re-resolves fresh.
        assert!(Arc::ptr_eq(&keep_a, &cache.resolve(&db, keep).unwrap()));
        assert!(!Arc::ptr_eq(&gone_a, &cache.resolve(&db, gone).unwrap()));
    }

    #[test]
    fn header_cache_remove_drops_one_track_only() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_in_memory().unwrap();
        let mk = |name: &str| {
            let path = dir.path().join(name);
            let (audio_offset, audio_length) = write_flac_local(&path);
            let meta = std::fs::metadata(&path).unwrap();
            db.upsert_track(&NewTrack {
                backing_path: path.to_string_lossy().to_string(),
                format: Format::Flac,
                audio_offset,
                audio_length,
                backing_size: meta.len() as i64,
                backing_mtime: mtime_secs(&meta),
            })
            .unwrap()
        };
        let keep = mk("keep.flac");
        let gone = mk("gone.flac");
        let cache = HeaderCache::new(Mode::Synthesis);
        let keep_a = cache.resolve(&db, keep).unwrap();
        let gone_a = cache.resolve(&db, gone).unwrap();

        cache.remove(gone);

        // The kept track stays the same cached Arc; the removed one re-resolves fresh.
        assert!(Arc::ptr_eq(&keep_a, &cache.resolve(&db, keep).unwrap()));
        assert!(!Arc::ptr_eq(&gone_a, &cache.resolve(&db, gone).unwrap()));
    }

    #[test]
    fn default_cache_budget_is_64_mib() {
        assert_eq!(DEFAULT_CACHE_BUDGET, 67_108_864);
    }

    #[test]
    fn read_segments_returns_empty_past_end_of_range() {
        let db = musefs_db::Db::open_in_memory().unwrap();
        let resolved = entry(0, 10);
        let out = read_at(&resolved, &db, 11, 1).unwrap();
        assert!(out.is_empty());
        let out0 = read_at(&resolved, &db, 0, 0).unwrap();
        assert!(out0.is_empty());
    }

    fn track_with_bounds(
        path: &std::path::Path,
        audio_offset: i64,
        audio_length: i64,
    ) -> (musefs_db::Db, i64) {
        use musefs_db::{Format, NewTrack};
        let db = musefs_db::Db::open_in_memory().unwrap();
        let meta = std::fs::metadata(path).unwrap();
        let id = db
            .upsert_track(&NewTrack {
                backing_path: path.to_string_lossy().to_string(),
                format: Format::Flac,
                audio_offset,
                audio_length,
                backing_size: meta.len() as i64,
                backing_mtime: mtime_secs(&meta),
            })
            .unwrap();
        (db, id)
    }

    #[test]
    fn build_rejects_audio_region_past_end_of_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.flac");
        let _ = write_flac_local(&path);
        let len = std::fs::metadata(&path).unwrap().len() as i64;
        let (db, id) = track_with_bounds(&path, len, 5);
        let cache = HeaderCache::new(Mode::Synthesis);
        assert!(matches!(
            cache.resolve(&db, id),
            Err(CoreError::BackingChanged(_))
        ));
    }

    #[test]
    fn build_accepts_audio_region_ending_exactly_at_eof() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.flac");
        let (audio_offset, audio_length) = write_flac_local(&path);
        let (db, id) = track_with_bounds(&path, audio_offset, audio_length);
        let cache = HeaderCache::new(Mode::Synthesis);
        let resolved = cache
            .resolve(&db, id)
            .expect("exact-fit bounds must resolve");
        assert!(resolved.total_len > 0);
    }

    #[test]
    fn build_cache_bytes_counts_inline_segments() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.flac");
        let (audio_offset, audio_length) = write_flac_local(&path);
        let (db, id) = track_with_bounds(&path, audio_offset, audio_length);
        let cache = HeaderCache::new(Mode::Synthesis);
        let resolved = cache.resolve(&db, id).unwrap();
        let inline_sum: u64 = resolved
            .layout
            .segments()
            .iter()
            .map(|s| match s {
                Segment::Inline(b) => b.len() as u64,
                _ => 0,
            })
            .sum();
        assert!(inline_sum > 0);
        assert_eq!(resolved.cache_bytes, inline_sum);
    }

    fn write_flac_local(path: &std::path::Path) -> (i64, i64) {
        fn block(bt: u8, body: &[u8], last: bool) -> Vec<u8> {
            let mut v = vec![(if last { 0x80 } else { 0 }) | (bt & 0x7F)];
            let n = body.len();
            v.extend_from_slice(&[(n >> 16) as u8, (n >> 8) as u8, n as u8]);
            v.extend_from_slice(body);
            v
        }
        let mut si = vec![
            0x10, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0A, 0xC4, 0x42, 0xF0,
            0x00, 0x00, 0x00, 0x00,
        ];
        si.extend_from_slice(&[0u8; 16]);
        let mut vc = Vec::new();
        let vendor = b"x";
        vc.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
        vc.extend_from_slice(vendor);
        vc.extend_from_slice(&0u32.to_le_bytes());
        let mut out = b"fLaC".to_vec();
        out.extend(block(0, &si, false));
        out.extend(block(4, &vc, true));
        let audio = [0xABu8; 256];
        let audio_offset = out.len() as i64;
        out.extend_from_slice(&audio);
        std::fs::write(path, &out).unwrap();
        (audio_offset, audio.len() as i64)
    }

    #[test]
    fn cache_weight_stays_within_budget_after_flood() {
        let cache = HeaderCache::with_budget(Mode::Synthesis, 4096);
        for id in 0..64i64 {
            cache.cache.insert(id, entry(0, 256)); // 64 × 256 B = 16 KiB ≫ 4 KiB
        }
        // End-state assertion only: quick_cache does not document per-insert
        // synchronous eviction, so the per-insert bound is not guaranteed.
        assert!(
            cache.cache.weight() <= 4096,
            "total weight {} exceeds the 4096-byte budget",
            cache.cache.weight()
        );
        // len() is assumed to count resident entries. If this assertion ever
        // trips, the diagnosis is the same as the weight() note above: re-read
        // the spec's eviction-timing section and escalate — don't loosen.
        assert!(
            cache.cache.len() < 64,
            "no eviction happened: all 64 over-budget entries are resident"
        );
    }

    #[test]
    fn zero_cache_bytes_entry_still_weighs_one() {
        // StructureOnly layouts have cache_bytes == 0; the weigher's .max(1) keeps
        // them inside the weighted bound instead of escaping it (quick_cache
        // ignores zero-weight entries when evicting).
        let cache = HeaderCache::with_budget(Mode::StructureOnly, 1024);
        cache.cache.insert(1, entry(0, 0));
        assert_eq!(cache.cache.weight(), 1);
        assert!(cache.cache.get(&1).is_some());
    }
}

#[cfg(test)]
mod binary_tag_serve_tests {
    use super::*;
    use musefs_db::{BinaryTag, NewTrack};

    #[test]
    fn resolve_mp3_emits_binary_tag_in_synthesized_region() {
        use id3::frame::{Content, Unknown};
        use id3::{Encoder, Frame, Tag, TagLike, Version};
        let dir = tempfile::tempdir().unwrap();
        let mut tag = Tag::new();
        let needle = [0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x02, 0x77, 0x88];
        tag.add_frame(Frame::with_content(
            "PRIV",
            Content::Unknown(Unknown {
                data: needle.to_vec(),
                version: Version::Id3v24,
            }),
        ));
        let mut bytes = Vec::new();
        Encoder::new()
            .version(Version::Id3v24)
            .encode(&tag, &mut bytes)
            .unwrap();
        bytes.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
        let path = dir.path().join("a.mp3");
        std::fs::write(&path, &bytes).unwrap();

        let db = musefs_db::Db::open_in_memory().unwrap();
        let bounds = musefs_format::mp3::locate_audio(&bytes).unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        let tid = db
            .upsert_track(&musefs_db::NewTrack {
                backing_path: path.to_string_lossy().to_string(),
                format: musefs_db::Format::Mp3,
                audio_offset: bounds.audio_offset as i64,
                audio_length: bounds.audio_length as i64,
                backing_size: meta.len() as i64,
                backing_mtime: meta
                    .modified()
                    .unwrap()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs() as i64,
            })
            .unwrap();
        db.set_binary_tags(
            tid,
            &[musefs_db::BinaryTag {
                key: "PRIV".into(),
                payload: needle.to_vec(),
                ordinal: 0,
            }],
        )
        .unwrap();

        let cache = crate::reader::HeaderCache::new(crate::Mode::Synthesis);
        let resolved = cache.resolve(&db, tid).unwrap();
        let whole = crate::reader::read_at(&resolved, &db, 0, resolved.total_len).unwrap();
        assert!(
            whole.windows(needle.len()).any(|w| w == needle),
            "PRIV body not in synthesized file"
        );
    }

    #[test]
    fn read_at_serves_binary_tag_segment() {
        let db = Db::open_in_memory().unwrap();
        let id = db
            .upsert_track(&NewTrack {
                backing_path: "/x.mp3".into(),
                format: Format::Mp3,
                audio_offset: 0,
                audio_length: 0,
                backing_size: 0,
                backing_mtime: 0,
            })
            .unwrap();
        db.set_binary_tags(
            id,
            &[BinaryTag {
                key: "PRIV".into(),
                payload: vec![10, 20, 30, 40],
                ordinal: 0,
            }],
        )
        .unwrap();
        let rowid = db.get_binary_tags(id).unwrap()[0].rowid;

        let resolved = ResolvedFile {
            layout: RegionLayout::new(vec![Segment::BinaryTag {
                payload_id: rowid,
                len: 4,
            }]),
            total_len: 4,
            content_version: 0,
            backing_path: PathBuf::from("/x.mp3"),
            backing_size: 0,
            backing_mtime_secs: 0,
            mtime_secs: 0,
            last_page: Mutex::new(None),
            cache_bytes: 0,
            has_binary_tag: true,
        };
        // No BackingAudio segment, so read_at opens no file.
        let got = read_at(&resolved, &db, 1, 2).unwrap();
        assert_eq!(got, vec![20, 30]);
    }
}
