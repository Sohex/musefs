use std::collections::HashMap;
use std::path::{Path, PathBuf};

use musefs_db::{Db, Format, NewArt, NewTrack, Tag, TrackArt};
use musefs_format::{flac, mp3, mp4, ogg, EmbeddedPicture};

use crate::error::Result;

/// Skip embedded art whose image bytes exceed this. The binding limit is FLAC's
/// 24-bit PICTURE block length (~16 MiB for the whole block); reserve 64 KiB of
/// headroom so the block framing + mime + description can never push a near-cap
/// image past the limit at synthesis time. Real cover art is far smaller.
const MAX_ART_BYTES: usize = 16 * 1024 * 1024 - 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanStats {
    pub scanned: u64,
    pub skipped: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevalidateStats {
    pub updated: u64,
    pub unchanged: u64,
    pub pruned: u64,
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
        } else if ftype.is_file()
            && (has_ext(&path, "flac")
                || has_ext(&path, "mp3")
                || has_ext(&path, "m4a")
                || has_ext(&path, "m4b")
                || has_ext(&path, "ogg")
                || has_ext(&path, "oga")
                || has_ext(&path, "opus"))
        {
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
    pictures: Vec<EmbeddedPicture>,
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
            pictures: flac::read_pictures(bytes).unwrap_or_default(),
        })
    } else if has_ext(path, "mp3") {
        let bounds = mp3::locate_audio(bytes).ok()?;
        Some(Probed {
            format: Format::Mp3,
            audio_offset: bounds.audio_offset,
            audio_length: bounds.audio_length,
            tags: mp3::read_tags(bytes),
            pictures: mp3::read_pictures(bytes),
        })
    } else if has_ext(path, "m4a") || has_ext(path, "m4b") {
        let bounds = mp4::locate_audio(bytes).ok()?;
        Some(Probed {
            format: Format::M4a,
            audio_offset: bounds.audio_offset,
            audio_length: bounds.audio_length,
            tags: mp4::read_tags(bytes),
            pictures: mp4::read_pictures(bytes),
        })
    } else if has_ext(path, "ogg") || has_ext(path, "oga") || has_ext(path, "opus") {
        let scan = ogg::locate_audio(bytes).ok()?;
        let format = match scan.codec {
            ogg::Codec::Opus => Format::Opus,
            ogg::Codec::Vorbis => Format::Vorbis,
            ogg::Codec::OggFlac => Format::OggFlac,
        };
        Some(Probed {
            format,
            audio_offset: scan.audio_offset,
            audio_length: scan.audio_length,
            tags: ogg::read_tags(bytes).unwrap_or_default(),
            pictures: ogg::read_pictures(bytes).unwrap_or_default(),
        })
    } else {
        None
    }
}

/// Upsert a track from a probed backing file: write the track row, replace its
/// seeded tags, and ingest its embedded art (capped, deduped, clamped).
fn ingest(db: &Db, abs_path: &str, meta: &std::fs::Metadata, probed: Probed) -> Result<()> {
    let track_id = db.upsert_track(&NewTrack {
        backing_path: abs_path.to_string(),
        format: probed.format,
        audio_offset: probed.audio_offset as i64,
        audio_length: probed.audio_length as i64,
        backing_size: meta.len() as i64,
        backing_mtime: mtime_secs(meta),
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

    let mut track_arts = Vec::new();
    // Filter before enumerating so skipped (oversized) art doesn't leave gaps
    // in the stored ordinals.
    let accepted = probed
        .pictures
        .into_iter()
        .filter(|p| p.data.len() <= MAX_ART_BYTES);
    for (ordinal, pic) in accepted.enumerate() {
        let art_id = db.upsert_art(&NewArt {
            mime: pic.mime,
            width: (pic.width != 0).then_some(pic.width as i64),
            height: (pic.height != 0).then_some(pic.height as i64),
            data: pic.data,
        })?;
        // Valid ID3/FLAC picture types are 0..=20; clamp anything out of range.
        let picture_type = if pic.picture_type <= 20 {
            pic.picture_type as i64
        } else {
            0
        };
        track_arts.push(TrackArt {
            art_id,
            picture_type,
            description: pic.description,
            ordinal: ordinal as i64,
        });
    }
    db.set_track_art(track_id, &track_arts)?;
    Ok(())
}

/// Insert/update a track row for each `.flac`/`.mp3` file under `root` (with
/// audio bounds and validation stamps), seeding its tags from the file's
/// existing metadata. `root` may be a single audio file (only that file is
/// scanned) or a directory (walked recursively). Files that fail to parse are
/// skipped.
pub fn scan_directory(db: &Db, root: &Path) -> Result<ScanStats> {
    let mut files = Vec::new();
    if root.is_file() {
        if has_ext(root, "flac") || has_ext(root, "mp3") {
            files.push(root.to_path_buf());
        }
    } else {
        collect_audio(root, &mut files)?;
    }

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
        ingest(db, &abs.to_string_lossy(), &meta, probed)?;
        stats.scanned += 1;
    }
    Ok(stats)
}

/// Re-validate an already-scanned library subtree: re-probe only files whose
/// size/mtime changed since the last scan (skipping unchanged ones so external
/// tag edits in the DB are preserved), then delete tracks **under `root`** whose
/// backing file is gone (cascading tags/art links) and garbage-collect
/// now-unreferenced art. Pruning is scoped to `root`, so revalidating one library
/// root never removes tracks belonging to another.
pub fn revalidate(db: &Db, root: &Path) -> Result<RevalidateStats> {
    let mut files = Vec::new();
    collect_audio(root, &mut files)?;

    let mut stats = RevalidateStats {
        updated: 0,
        unchanged: 0,
        pruned: 0,
    };
    for path in files {
        let meta = std::fs::metadata(&path)?;
        let abs = std::fs::canonicalize(&path)?;
        let abs_str = abs.to_string_lossy().to_string();

        if let Some(existing) = db.get_track_by_path(&abs_str)? {
            if existing.backing_size == meta.len() as i64
                && existing.backing_mtime == mtime_secs(&meta)
            {
                stats.unchanged += 1;
                continue;
            }
        }

        let bytes = std::fs::read(&path)?;
        if let Some(probed) = probe(&path, &bytes) {
            ingest(db, &abs_str, &meta, probed)?;
            stats.updated += 1;
        }
    }

    // Prune tracks under `root` whose backing file is gone. Scoped to `root` so a
    // targeted revalidate never touches tracks from a different library root, and
    // gated on `NotFound` so a transient I/O error (an unreadable mount, a denied
    // permission) does not delete a track whose file still exists.
    let canon_root = std::fs::canonicalize(root)?;
    for track in db.list_tracks()? {
        if !Path::new(&track.backing_path).starts_with(&canon_root) {
            continue;
        }
        match std::fs::metadata(&track.backing_path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                db.delete_track(track.id)?;
                stats.pruned += 1;
            }
            _ => {}
        }
    }
    db.gc_orphan_art()?;

    Ok(stats)
}

#[cfg(test)]
mod ogg_probe_tests {
    use super::*;
    use musefs_format::ogg::page_test_support::{
        build_header_pub, lace_packet_pub, vorbis_body_empty,
    };
    use std::io::Write;

    #[test]
    fn probe_detects_opus_and_seeds_tags() {
        let head = b"OpusHead\x01\x02\x38\x01\x80\xbb\x00\x00\x00\x00\x00".to_vec();
        let mut tags = b"OpusTags".to_vec();
        tags.extend_from_slice(&vorbis_body_empty());
        let (mut bytes, _) = build_header_pub(0x1234, &[&head, &tags]);
        let (audio, _) = lace_packet_pub(0x1234, 2, false, 960, &[0u8; 100]);
        bytes.extend_from_slice(&audio);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("song.opus");
        std::fs::File::create(&path)
            .unwrap()
            .write_all(&bytes)
            .unwrap();

        let probed = probe(&path, &bytes).expect("opus should probe");
        assert_eq!(probed.format, Format::Opus);
        assert_eq!(probed.audio_offset, (bytes.len() - audio.len()) as u64);
    }
}
