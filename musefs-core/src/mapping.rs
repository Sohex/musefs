use std::collections::BTreeMap;

use musefs_db::{Db, Tag};
use musefs_format::{ArtInput, BinaryTagInput, TagInput};

use crate::error::Result;

/// Convert DB tag rows into the ordered list of synthesis inputs (one per value).
/// `Db::get_tags` already returns rows ordered by `(key, ordinal)`, so order is preserved.
pub(crate) fn tags_to_inputs(tags: &[Tag]) -> Vec<TagInput> {
    tags.iter()
        .map(|t| TagInput::new(&t.key, &t.value))
        .collect()
}

/// Build the field map used for path-template rendering: the first value (lowest
/// ordinal) of each key. Relies on `Db::get_tags` ordering by `(key, ordinal)`.
/// Keys are ASCII-lowercased so a `$field` placeholder resolves regardless of the
/// stored key's case (unlike `tags_to_inputs`, which passes keys verbatim to synthesis).
pub(crate) fn tags_to_fields(tags: &[Tag]) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for t in tags {
        map.entry(t.key.to_ascii_lowercase())
            .or_insert_with(|| t.value.clone());
    }
    map
}

/// Build the synthesis art inputs for a track from `track_art` + art metadata.
/// Reads metadata only (never the image blob) so resolve stays memory-bounded;
/// the bytes are streamed at read time.
pub(crate) fn track_art_to_inputs(db: &Db, track_id: i64) -> Result<Vec<ArtInput>> {
    let mut inputs = Vec::new();
    for ta in db.get_track_art(track_id)? {
        // `track_art.art_id` is a foreign key into `art` (enforced, no ON DELETE),
        // so the row always exists; the `if let` is defensive, not a real branch.
        if let Some(meta) = db.get_art_meta(ta.art_id)? {
            // A negative byte_len is a malformed external write to the contract
            // column; skip the row rather than cast it to a huge u64 segment
            // length (which would fail layout validation and break the track).
            if meta.byte_len < 0 {
                continue;
            }
            inputs.push(ArtInput {
                art_id: ta.art_id,
                mime: meta.mime,
                description: ta.description,
                picture_type: ta.picture_type as u32,
                width: meta.width.unwrap_or(0) as u32,
                height: meta.height.unwrap_or(0) as u32,
                data_len: meta.byte_len as u64,
            });
        }
    }
    Ok(inputs)
}

/// Map a track's binary tag rows to `BinaryTagInput`s for synthesis. Never reads
/// the payload bytes — only `(rowid, key, byte_len)`; the bytes stream at read
/// time. Ordered by (key, ordinal), matching `get_binary_tags`.
#[allow(dead_code)] // wired into the reader resolve arms in Task 2.9
pub(crate) fn binary_tags_to_inputs(db: &Db, track_id: i64) -> Result<Vec<BinaryTagInput>> {
    Ok(db
        .get_binary_tags(track_id)?
        .into_iter()
        .map(|row| BinaryTagInput {
            key: row.key,
            payload_id: row.rowid,
            len: row.byte_len as u64,
        })
        .collect())
}

/// Read each embedded image's raw bytes for synthesis (Ogg needs the bytes to
/// compute page CRCs at resolve). Parallel to `track_art_to_inputs`; returns the
/// same order. Only the Ogg synthesis path calls this — FLAC/MP3/MP4 stream art
/// via `ArtImage` and never materialize it.
pub(crate) fn track_art_images(db: &Db, inputs: &[ArtInput]) -> Result<Vec<Vec<u8>>> {
    let mut out = Vec::with_capacity(inputs.len());
    for a in inputs {
        out.push(db.read_art_chunk(a.art_id, 0, a.data_len as usize)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use musefs_db::{BinaryTag, Db, Format, NewTrack};

    fn tag(key: &str, value: &str, ordinal: i64) -> Tag {
        Tag::new(key, value, ordinal)
    }

    #[test]
    fn inputs_preserve_order_including_multivalue() {
        let tags = vec![
            tag("artist", "Alice", 0),
            tag("artist", "Bob", 1),
            tag("title", "Song", 0),
        ];
        let inputs = tags_to_inputs(&tags);
        assert_eq!(
            inputs,
            vec![
                TagInput::new("artist", "Alice"),
                TagInput::new("artist", "Bob"),
                TagInput::new("title", "Song"),
            ]
        );
    }

    #[test]
    fn fields_take_first_value_per_key() {
        let tags = vec![
            tag("artist", "Alice", 0),
            tag("artist", "Bob", 1),
            tag("album", "X", 0),
        ];
        let fields = tags_to_fields(&tags);
        assert_eq!(fields.get("artist").map(String::as_str), Some("Alice"));
        assert_eq!(fields.get("album").map(String::as_str), Some("X"));
    }

    #[test]
    fn tags_to_fields_lowercases_keys_for_template_lookup() {
        let tags = vec![
            Tag::new("MyRating", "5", 0), // verbatim user-defined key
            Tag::new("albumartist", "VA", 0),
        ];
        let fields = tags_to_fields(&tags);
        assert_eq!(fields.get("myrating").map(String::as_str), Some("5"));
        assert_eq!(fields.get("albumartist").map(String::as_str), Some("VA"));
    }

    #[test]
    fn binary_tags_to_inputs_maps_rows() {
        let db = Db::open_in_memory().unwrap();
        let tid = db
            .upsert_track(&NewTrack {
                backing_path: "/a.mp3".into(),
                format: Format::Mp3,
                audio_offset: 0,
                audio_length: 0,
                backing_size: 0,
                backing_mtime: 0,
            })
            .unwrap();
        db.set_binary_tags(
            tid,
            &[BinaryTag {
                key: "PRIV".into(),
                payload: vec![1, 2, 3, 4],
                ordinal: 0,
            }],
        )
        .unwrap();

        let inputs = super::binary_tags_to_inputs(&db, tid).unwrap();
        assert_eq!(inputs.len(), 1);
        assert_eq!(inputs[0].key, "PRIV");
        assert_eq!(inputs[0].len, 4);
        // payload_id is the streaming handle (the tags rowid).
        let rowid = db.get_binary_tags(tid).unwrap()[0].rowid;
        assert_eq!(inputs[0].payload_id, rowid);
    }

    #[test]
    fn binary_rows_do_not_pollute_tags_to_fields() {
        let db = Db::open_in_memory().unwrap();
        let tid = db
            .upsert_track(&NewTrack {
                backing_path: "/a.mp3".into(),
                format: Format::Mp3,
                audio_offset: 0,
                audio_length: 0,
                backing_size: 0,
                backing_mtime: 0,
            })
            .unwrap();
        db.replace_tags(tid, &[Tag::new("artist", "A", 0)]).unwrap();
        db.set_binary_tags(
            tid,
            &[BinaryTag {
                key: "PRIV".into(),
                payload: vec![1, 2, 3],
                ordinal: 0,
            }],
        )
        .unwrap();

        let tags = db.get_tags(tid).unwrap();
        let fields = super::tags_to_fields(&tags);
        assert_eq!(fields.get("artist").map(String::as_str), Some("A"));
        assert!(
            !fields.contains_key("priv"),
            "binary PRIV leaked into fields: {fields:?}"
        );
    }

    #[test]
    fn track_art_to_inputs_skips_negative_byte_len() {
        use musefs_db::{NewArt, TrackArt}; // NewTrack already in scope at module level
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("art.db");
        let db = Db::open(&path).unwrap();
        let tid = db
            .upsert_track(&NewTrack {
                backing_path: "/a.flac".into(),
                format: Format::Flac,
                audio_offset: 0,
                audio_length: 0,
                backing_size: 0,
                backing_mtime: 0,
            })
            .unwrap();
        let good = db
            .upsert_art(&NewArt {
                mime: "image/png".into(),
                width: None,
                height: None,
                data: vec![1, 2, 3, 4],
            })
            .unwrap();
        let bad = db
            .upsert_art(&NewArt {
                mime: "image/png".into(),
                width: None,
                height: None,
                data: vec![9, 9, 9, 9, 9],
            })
            .unwrap();
        // A zero-byte_len row pins the guard's strict `<`: only *negative* is
        // skipped, so byte_len == 0 must still be kept (distinguishes `< 0` from
        // `<= 0`).
        let zero = db
            .upsert_art(&NewArt {
                mime: "image/png".into(),
                width: None,
                height: None,
                data: vec![5, 5, 5],
            })
            .unwrap();
        db.set_track_art(
            tid,
            &[
                TrackArt {
                    art_id: good,
                    picture_type: 3,
                    description: String::new(),
                    ordinal: 0,
                },
                TrackArt {
                    art_id: bad,
                    picture_type: 3,
                    description: String::new(),
                    ordinal: 1,
                },
                TrackArt {
                    art_id: zero,
                    picture_type: 3,
                    description: String::new(),
                    ordinal: 2,
                },
            ],
        )
        .unwrap();

        // Simulate external malformed writes: byte_len is a stored column.
        let raw = rusqlite::Connection::open(&path).unwrap();
        raw.execute("UPDATE art SET byte_len = -1 WHERE id = ?1", [bad])
            .unwrap();
        raw.execute("UPDATE art SET byte_len = 0 WHERE id = ?1", [zero])
            .unwrap();
        drop(raw);

        let inputs = super::track_art_to_inputs(&db, tid).unwrap();
        let ids: Vec<i64> = inputs.iter().map(|a| a.art_id).collect();
        assert_eq!(
            ids,
            vec![good, zero],
            "negative byte_len is skipped; zero is kept (strict `<`)"
        );
    }
}
