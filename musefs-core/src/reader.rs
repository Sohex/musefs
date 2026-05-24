use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use musefs_db::Db;
use musefs_format::flac::{read_metadata, synthesize_layout, FlacScan};
use musefs_format::RegionLayout;

use crate::error::{CoreError, Result};
use crate::mapping::tags_to_inputs;

/// A fully resolved synthesized file: its segment layout, total size, the
/// content version it was built from, and where the backing audio lives.
#[derive(Debug)]
pub struct ResolvedFile {
    pub layout: RegionLayout,
    pub total_len: u64,
    pub content_version: i64,
    pub backing_path: PathBuf,
    pub mtime_secs: i64,
}

/// A per-mount cache of resolved files, keyed by track id and invalidated when a
/// track's `content_version` changes (the DB bumps it on any tag/art edit).
#[derive(Default)]
pub struct HeaderCache {
    map: HashMap<i64, Arc<ResolvedFile>>,
}

fn mtime_secs(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn read_front(path: &Path, n: u64) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut buf = vec![0u8; n as usize];
    f.read_exact(&mut buf)?;
    Ok(buf)
}

impl HeaderCache {
    pub fn new() -> HeaderCache {
        HeaderCache::default()
    }

    /// Resolve a track to its synthesized layout, building (and caching) it on a
    /// content-version miss. Validates the backing file's size and mtime first.
    pub fn resolve(&mut self, db: &Db, track_id: i64) -> Result<Arc<ResolvedFile>> {
        let track = db.get_track(track_id)?.ok_or(CoreError::TrackNotFound(track_id))?;

        // Always validate the backing file first — a stale file is an error even
        // on a cache hit, because the audio region may have shifted.
        let meta = std::fs::metadata(&track.backing_path)?;
        if meta.len() != track.backing_size as u64 || mtime_secs(&meta) != track.backing_mtime {
            return Err(CoreError::BackingChanged(track.backing_path.clone()));
        }

        // Guard the stored audio bounds before any cast/allocation: a negative or
        // out-of-file offset means the row no longer matches the backing file.
        if track.audio_offset < 0
            || track.audio_length < 0
            || track.audio_offset as u64 > meta.len()
        {
            return Err(CoreError::BackingChanged(track.backing_path.clone()));
        }

        if let Some(cached) = self.map.get(&track_id) {
            if cached.content_version == track.content_version {
                return Ok(cached.clone());
            }
        }

        let front = read_front(Path::new(&track.backing_path), track.audio_offset as u64)?;
        let fmeta = read_metadata(&front)?;
        let tags = db.get_tags(track_id)?;
        let inputs = tags_to_inputs(&tags);

        let scan = FlacScan {
            audio_offset: track.audio_offset as u64,
            audio_length: track.audio_length as u64,
            preserved: fmeta.preserved,
        };
        let layout = synthesize_layout(&scan, &inputs, &[]);
        let total_len = layout.total_len();

        let resolved = Arc::new(ResolvedFile {
            layout,
            total_len,
            content_version: track.content_version,
            backing_path: PathBuf::from(&track.backing_path),
            mtime_secs: track.backing_mtime.max(track.updated_at),
        });
        self.map.insert(track_id, resolved.clone());
        Ok(resolved)
    }
}

use musefs_format::Segment;

/// Read `size` bytes starting at virtual `offset` from a resolved file, splicing
/// inline framing with positioned reads of the backing audio. Returns fewer bytes
/// (possibly empty) near EOF.
pub fn read_at(resolved: &ResolvedFile, offset: u64, size: u64) -> Result<Vec<u8>> {
    use std::os::unix::fs::FileExt;

    if offset >= resolved.total_len || size == 0 {
        return Ok(Vec::new());
    }
    let end = offset.saturating_add(size).min(resolved.total_len);
    let mut out = Vec::with_capacity((end - offset) as usize);

    let mut seg_start = 0u64;
    let mut backing: Option<std::fs::File> = None;

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
                    if backing.is_none() {
                        backing = Some(std::fs::File::open(&resolved.backing_path)?);
                    }
                    let f = backing.as_ref().unwrap();
                    let mut buf = vec![0u8; n];
                    f.read_exact_at(&mut buf, bo + within)?;
                    out.extend_from_slice(&buf);
                }
                Segment::ArtImage { .. } => {
                    return Err(CoreError::ArtNotSupported);
                }
            }
        }
        seg_start = seg_end;
        if seg_start >= end {
            break;
        }
    }
    Ok(out)
}
