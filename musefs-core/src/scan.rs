use std::collections::HashMap;
use std::path::{Path, PathBuf};

use musefs_db::{Db, Format, NewTrack, Tag};
use musefs_format::{flac, mp3};

use crate::error::Result;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanStats {
    pub scanned: u64,
    pub skipped: u64,
}

fn mtime_secs(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn has_ext(path: &Path, ext: &str) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case(ext))
        == Some(true)
}

fn collect_audio(root: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        let ftype = entry.file_type()?;
        if ftype.is_dir() {
            collect_audio(&path, out)?;
        } else if ftype.is_file() && (has_ext(&path, "flac") || has_ext(&path, "mp3")) {
            out.push(path);
        }
    }
    Ok(())
}

/// Parse one backing file into `(format, audio_offset, audio_length, raw tags)`,
/// or `None` if it does not parse as a supported format (and should be skipped).
#[allow(clippy::type_complexity)]
fn probe(path: &Path, bytes: &[u8]) -> Option<(Format, u64, u64, Vec<(String, String)>)> {
    if has_ext(path, "flac") {
        let scan = flac::locate_audio(bytes).ok()?;
        let tags = flac::read_vorbis_comments(bytes).unwrap_or_default();
        Some((Format::Flac, scan.audio_offset, scan.audio_length, tags))
    } else if has_ext(path, "mp3") {
        let bounds = mp3::locate_audio(bytes).ok()?;
        let tags = mp3::read_tags(bytes);
        Some((Format::Mp3, bounds.audio_offset, bounds.audio_length, tags))
    } else {
        None
    }
}

/// Walk `root` recursively, inserting/updating a track row for each `.flac`/`.mp3`
/// file (with audio bounds and validation stamps) and seeding its tags from the
/// file's existing metadata. Files that fail to parse are skipped.
pub fn scan_directory(db: &Db, root: &Path) -> Result<ScanStats> {
    let mut files = Vec::new();
    collect_audio(root, &mut files)?;

    let mut stats = ScanStats {
        scanned: 0,
        skipped: 0,
    };
    for path in files {
        let bytes = std::fs::read(&path)?;
        let (format, audio_offset, audio_length, raw_tags) = match probe(&path, &bytes) {
            Some(p) => p,
            None => {
                stats.skipped += 1;
                continue;
            }
        };
        let meta = std::fs::metadata(&path)?;
        let abs = std::fs::canonicalize(&path)?;
        let track_id = db.upsert_track(&NewTrack {
            backing_path: abs.to_string_lossy().to_string(),
            format,
            audio_offset: audio_offset as i64,
            audio_length: audio_length as i64,
            backing_size: meta.len() as i64,
            backing_mtime: mtime_secs(&meta),
        })?;

        let mut tags = Vec::new();
        let mut ordinals: HashMap<String, i64> = HashMap::new();
        for (field, value) in raw_tags {
            let key = field.to_lowercase();
            let ord = ordinals.entry(key.clone()).or_insert(0);
            tags.push(Tag::new(&key, &value, *ord));
            *ord += 1;
        }
        db.replace_tags(track_id, &tags)?;
        stats.scanned += 1;
    }
    Ok(stats)
}
