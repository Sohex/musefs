#![allow(dead_code)]

use musefs_db::{Format, NewArt, NewTrack};

pub fn new_track(path: &str) -> NewTrack {
    NewTrack {
        backing_path: path.to_string(),
        format: Format::Flac,
        audio_offset: 100,
        audio_length: 1000,
        backing_size: 1100,
        backing_mtime_ns: 1_700_000_000_000_000_000,
        backing_ctime_ns: 1_700_000_000_000_000_000,
    }
}

pub fn jpeg(data: Vec<u8>) -> NewArt {
    NewArt {
        mime: "image/jpeg".to_string(),
        width: None,
        height: None,
        data,
    }
}
