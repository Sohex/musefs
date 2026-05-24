use std::path::Path;

use musefs_db::{Db, Format, NewTrack, Tag};
use musefs_format::flac::{locate_audio, read_vorbis_comments};

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

fn collect_flacs(root: &Path, out: &mut Vec<std::path::PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        let ftype = entry.file_type()?;
        if ftype.is_dir() {
            collect_flacs(&path, out)?;
        } else if ftype.is_file()
            && path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("flac"))
                == Some(true)
        {
            out.push(path);
        }
    }
    Ok(())
}

/// Walk `root` recursively, inserting/updating a track row for each `.flac` file
/// (with audio bounds and validation stamps) and seeding its tags from the file's
/// existing Vorbis comments. Files that fail to parse as FLAC are skipped.
pub fn scan_directory(db: &Db, root: &Path) -> Result<ScanStats> {
    let mut files = Vec::new();
    collect_flacs(root, &mut files)?;

    let mut stats = ScanStats {
        scanned: 0,
        skipped: 0,
    };
    for path in files {
        let bytes = std::fs::read(&path)?;
        let scan = match locate_audio(&bytes) {
            Ok(s) => s,
            Err(_) => {
                stats.skipped += 1;
                continue;
            }
        };
        let meta = std::fs::metadata(&path)?;
        let abs = std::fs::canonicalize(&path)?;
        let track_id = db.upsert_track(&NewTrack {
            backing_path: abs.to_string_lossy().to_string(),
            format: Format::Flac,
            audio_offset: scan.audio_offset as i64,
            audio_length: scan.audio_length as i64,
            backing_size: meta.len() as i64,
            backing_mtime: mtime_secs(&meta),
        })?;

        let comments = read_vorbis_comments(&bytes).unwrap_or_default();
        let mut tags = Vec::new();
        let mut ordinals: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
        for (field, value) in comments {
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
