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

/// A backing file parsed into the fields a track row needs, plus its raw
/// `(key, value)` tags to seed.
struct Probed {
    format: Format,
    audio_offset: u64,
    audio_length: u64,
    tags: Vec<(String, String)>,
}

/// Parse one backing file into a `Probed`, or `None` if it does not parse as a
/// supported format (and should be skipped).
fn probe(path: &Path, bytes: &[u8]) -> Option<Probed> {
    if has_ext(path, "flac") {
        let scan = flac::locate_audio(bytes).ok()?;
        Some(Probed {
            format: Format::Flac,
            audio_offset: scan.audio_offset,
            audio_length: scan.audio_length,
            tags: flac::read_vorbis_comments(bytes).unwrap_or_default(),
        })
    } else if has_ext(path, "mp3") {
        let bounds = mp3::locate_audio(bytes).ok()?;
        Some(Probed {
            format: Format::Mp3,
            audio_offset: bounds.audio_offset,
            audio_length: bounds.audio_length,
            tags: mp3::read_tags(bytes),
        })
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
        let probed = match probe(&path, &bytes) {
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
            format: probed.format,
            audio_offset: probed.audio_offset as i64,
            audio_length: probed.audio_length as i64,
            backing_size: meta.len() as i64,
            backing_mtime: mtime_secs(&meta),
        })?;

        let mut tags = Vec::new();
        let mut ordinals: HashMap<String, i64> = HashMap::new();
        for (field, value) in probed.tags {
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
