use std::collections::BTreeMap;

use musefs_db::{Db, Tag};
use musefs_format::{ArtInput, BinaryTagInput, TagInput};

use crate::error::Result;

/// Convert DB tag rows into the ordered list of synthesis inputs (one per value),
/// moving the strings out of the rows rather than copying them.
/// `Db::get_tags` already returns rows ordered by `(key, ordinal)`, so order is preserved.
pub(crate) fn tags_to_inputs(tags: Vec<Tag>) -> Vec<TagInput> {
    tags.into_iter()
        .map(|t| TagInput {
            key: t.key,
            value: t.value,
        })
        .collect()
}

/// Build the field map used for path-template rendering: the first value (lowest
/// ordinal) of each key, borrowed from the rows. Relies on `Db::get_tags` ordering
/// by `(key, ordinal)`. Keys are ASCII-lowercased so a `$field` placeholder
/// resolves regardless of the stored key's case (unlike `tags_to_inputs`, which
/// passes keys verbatim to synthesis).
pub(crate) fn tags_to_fields(tags: &[Tag]) -> BTreeMap<String, &str> {
    let mut map = BTreeMap::new();
    for t in tags {
        map.entry(t.key.to_ascii_lowercase())
            .or_insert_with(|| t.value.as_str());
    }
    map
}

/// Build the synthesis art inputs for a track from `track_art` + art metadata.
/// Reads metadata only (never the image blob) so resolve stays memory-bounded;
/// the bytes are streamed at read time.
pub(crate) fn track_art_to_inputs<M>(db: &Db<M>, track_id: i64) -> Result<Vec<ArtInput>> {
    let mut inputs = Vec::new();
    for ta in db.get_track_art(track_id)? {
        // `track_art.art_id` is a foreign key into `art`, but SQLite FK
        // enforcement is per-connection and external writers can disable it or
        // import a partial DB. A missing `art` row is a contract violation we
        // surface (the read fails) rather than silently dropping the art.
        let Some(meta) = db.get_art_meta(ta.art_id)? else {
            return Err(crate::error::CoreError::OrphanedArt {
                track_id,
                art_id: ta.art_id,
            });
        };
        let Some(data_len) = musefs_format::BlobLen::new(meta.byte_len) else {
            continue; // zero-length art: synthesis would skip it anyway (now type-level).
        };

        if data_len.get() > crate::scan::MAX_ART_BYTES as u64 {
            log::warn!(
                "track {track_id} art {} is {} bytes, exceeds the {}-byte art cap; refusing to serve",
                ta.art_id,
                data_len.get(),
                crate::scan::MAX_ART_BYTES,
            );
            return Err(crate::error::CoreError::ArtTooLarge {
                track_id,
                art_id: ta.art_id,
                byte_len: data_len.get(),
                cap: crate::scan::MAX_ART_BYTES as u64,
            });
        }

        let Some(picture_type) = musefs_format::PictureType::new(ta.picture_type) else {
            return Err(crate::error::CoreError::InvalidPictureType {
                track_id,
                art_id: ta.art_id,
                value: ta.picture_type,
            });
        };
        inputs.push(ArtInput {
            art_id: ta.art_id,
            mime: meta.mime,
            description: ta.description,
            picture_type,
            width: meta.width.unwrap_or(0),
            height: meta.height.unwrap_or(0),
            data_len,
        });
    }
    Ok(inputs)
}

/// Map a track's binary tag rows to `BinaryTagInput`s for synthesis. Never reads
/// the payload bytes — only `(rowid, key, byte_len)`; the bytes stream at read
/// time. Ordered by (key, ordinal), matching `get_binary_tags`.
#[allow(dead_code)] // wired into the reader resolve arms in Task 2.9
pub(crate) fn binary_tags_to_inputs<M>(db: &Db<M>, track_id: i64) -> Result<Vec<BinaryTagInput>> {
    Ok(db
        .get_binary_tags(track_id)?
        .into_iter()
        .filter_map(|row| {
            musefs_format::BlobLen::new(row.byte_len).map(|len| BinaryTagInput {
                key: row.key,
                payload_id: row.rowid,
                len,
            })
        })
        .collect())
}

/// Read each embedded image's raw bytes for synthesis (Ogg needs the bytes to
/// compute page CRCs at resolve). Parallel to `track_art_to_inputs`; returns the
/// same order. Only the Ogg synthesis path calls this — FLAC/MP3/MP4 stream art
/// via `ArtImage` and never materialize it.
pub(crate) fn track_art_images<M>(db: &Db<M>, inputs: &[ArtInput]) -> Result<Vec<Vec<u8>>> {
    let mut out = Vec::with_capacity(inputs.len());
    for a in inputs {
        out.push(db.read_art_chunk(
            a.art_id,
            0,
            musefs_db::convert::usize_from(a.data_len.get()),
        )?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use musefs_db::{BinaryTag, Db, Format, NewTrack};

    fn tag(key: &str, value: &str, ordinal: u64) -> Tag {
        Tag::new(key, value, ordinal)
    }

    #[test]
    fn inputs_preserve_order_including_multivalue() {
        let tags = vec![
            tag("artist", "Alice", 0),
            tag("artist", "Bob", 1),
            tag("title", "Song", 0),
        ];
        let inputs = tags_to_inputs(tags);
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
        assert_eq!(fields.get("artist").copied(), Some("Alice"));
        assert_eq!(fields.get("album").copied(), Some("X"));
    }

    #[test]
    fn tags_to_fields_lowercases_keys_for_template_lookup() {
        let tags = vec![
            Tag::new("MyRating", "5", 0), // verbatim user-defined key
            Tag::new("albumartist", "VA", 0),
        ];
        let fields = tags_to_fields(&tags);
        assert_eq!(fields.get("myrating").copied(), Some("5"));
        assert_eq!(fields.get("albumartist").copied(), Some("VA"));
    }

    #[test]
    fn bridge_drops_zero_length_art() {
        use musefs_db::{NewArt, TrackArt};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("z.db");
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
        let nonempty = db
            .upsert_art(&NewArt {
                mime: "image/png".into(),
                width: None,
                height: None,
                data: vec![1, 2, 3],
            })
            .unwrap();
        let empty = db
            .upsert_art(&NewArt {
                mime: "image/png".into(),
                width: None,
                height: None,
                data: vec![],
            })
            .unwrap();
        db.set_track_art(
            tid,
            &[
                TrackArt {
                    art_id: nonempty,
                    picture_type: 3,
                    description: String::new(),
                    ordinal: 0,
                },
                TrackArt {
                    art_id: empty,
                    picture_type: 3,
                    description: String::new(),
                    ordinal: 1,
                },
            ],
        )
        .unwrap();
        let inputs = super::track_art_to_inputs(&db, tid).unwrap();
        // The zero-length art is dropped at construction (synthesis would skip it).
        assert_eq!(inputs.len(), 1);
        assert_eq!(inputs[0].art_id, nonempty);
        assert_eq!(inputs[0].data_len.get(), 3);
        assert_eq!(inputs[0].picture_type.get(), 3);
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
        assert_eq!(inputs[0].len.get(), 4);
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
        assert_eq!(fields.get("artist").copied(), Some("A"));
        assert!(
            !fields.contains_key("priv"),
            "binary PRIV leaked into fields: {fields:?}"
        );
    }

    #[test]
    fn track_art_to_inputs_errors_on_negative_byte_len() {
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
            ],
        )
        .unwrap();

        // A negative byte_len is a malformed external write to the contract
        // column: the V4 `byte_len = length(data)` CHECK rejects it on a normal
        // connection, so bypass CHECK enforcement to plant the bad row — the
        // row-reader defensive path (not the CHECK) is what this test pins.
        let raw = rusqlite::Connection::open(&path).unwrap();
        raw.pragma_update(None, "ignore_check_constraints", true)
            .unwrap();
        raw.execute("UPDATE art SET byte_len = -1 WHERE id = ?1", [bad])
            .unwrap();
        raw.pragma_update(None, "ignore_check_constraints", false)
            .unwrap();
        drop(raw);
        assert!(
            super::track_art_to_inputs(&db, tid).is_err(),
            "negative byte_len must error at row-read, not be skipped"
        );
    }

    #[test]
    fn track_art_to_inputs_errors_on_orphaned_row() {
        use crate::CoreError;
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
        let orphan_id = db
            .upsert_art(&NewArt {
                mime: "image/png".into(),
                width: None,
                height: None,
                data: vec![1, 2, 3, 4],
            })
            .unwrap();
        db.set_track_art(
            tid,
            &[TrackArt {
                art_id: orphan_id,
                picture_type: 3,
                description: String::new(),
                ordinal: 0,
            }],
        )
        .unwrap();

        // Well-formed art resolves to one input (kills the "always error" mutant).
        let inputs = super::track_art_to_inputs(&db, tid).unwrap();
        assert_eq!(inputs.len(), 1);

        // Orphan the track_art row: delete the referenced art row on a raw
        // connection (FK enforcement off by default), leaving the track_art
        // link dangling. The production Db sets foreign_keys=true, so the
        // delete would RESTRICT-fail there.
        let raw = rusqlite::Connection::open(&path).unwrap();
        raw.pragma_update(None, "foreign_keys", false).unwrap();
        let deleted = raw
            .execute("DELETE FROM art WHERE id = ?1", [orphan_id])
            .unwrap();
        assert_eq!(deleted, 1, "delete must remove exactly one art row");
        drop(raw);

        let err = super::track_art_to_inputs(&db, tid).unwrap_err();
        assert!(
            matches!(
                err,
                CoreError::OrphanedArt { track_id, art_id }
                    if track_id == tid && art_id == orphan_id
            ),
            "orphaned track_art must yield OrphanedArt with the offending ids, got {err:?}"
        );
    }

    #[test]
    fn track_art_images_reads_stored_blob_bytes() {
        use musefs_db::{NewArt, TrackArt};
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
        let first = db
            .upsert_art(&NewArt {
                mime: "image/png".into(),
                width: None,
                height: None,
                data: vec![1, 2, 3, 4],
            })
            .unwrap();
        let second = db
            .upsert_art(&NewArt {
                mime: "image/jpeg".into(),
                width: None,
                height: None,
                data: vec![7, 8, 9],
            })
            .unwrap();
        db.set_track_art(
            tid,
            &[
                TrackArt {
                    art_id: first,
                    picture_type: 3,
                    description: String::new(),
                    ordinal: 0,
                },
                TrackArt {
                    art_id: second,
                    picture_type: 4,
                    description: String::new(),
                    ordinal: 1,
                },
            ],
        )
        .unwrap();

        let inputs = super::track_art_to_inputs(&db, tid).unwrap();
        let images = super::track_art_images(&db, &inputs).unwrap();
        assert_eq!(
            images,
            vec![vec![1, 2, 3, 4], vec![7, 8, 9]],
            "must return each stored blob's exact bytes, in input order"
        );
    }

    #[test]
    fn track_art_to_inputs_enforces_art_cap() {
        use crate::CoreError;
        use musefs_db::{NewArt, TrackArt}; // NewTrack already in scope at module level
        // The boundary uses the real MAX_ART_BYTES (not a smaller test value) so the
        // mutation gate catches a `>`→`>=` flip; this hashes ~32 MiB across the two
        // blobs, which is fine here — do not "optimize" the boundary away.
        let cap = crate::scan::MAX_ART_BYTES;
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path().join("cap.db")).unwrap();
        let tid = db
            .upsert_track(&NewTrack {
                backing_path: "/a.opus".into(),
                format: Format::Opus,
                audio_offset: 0,
                audio_length: 0,
                backing_size: 0,
                backing_mtime: 0,
            })
            .unwrap();

        // Exactly at the cap: accepted.
        let at_cap = db
            .upsert_art(&NewArt {
                mime: "image/png".into(),
                width: None,
                height: None,
                data: vec![0u8; cap],
            })
            .unwrap();
        db.set_track_art(
            tid,
            &[TrackArt {
                art_id: at_cap,
                picture_type: 3,
                description: String::new(),
                ordinal: 0,
            }],
        )
        .unwrap();
        let ok = super::track_art_to_inputs(&db, tid).unwrap();
        assert_eq!(ok.len(), 1, "art exactly at the cap must be accepted");

        // One byte over the cap: rejected with ArtTooLarge naming the offending ids.
        let over = db
            .upsert_art(&NewArt {
                mime: "image/png".into(),
                width: None,
                height: None,
                data: vec![0u8; cap + 1],
            })
            .unwrap();
        db.set_track_art(
            tid,
            &[TrackArt {
                art_id: over,
                picture_type: 3,
                description: String::new(),
                ordinal: 0,
            }],
        )
        .unwrap();
        let err = super::track_art_to_inputs(&db, tid).unwrap_err();
        assert!(
            matches!(
                err,
                CoreError::ArtTooLarge { track_id, art_id, byte_len, cap: c }
                    if track_id == tid && art_id == over
                        && byte_len == (cap as u64) + 1 && c == cap as u64
            ),
            "oversize art must yield ArtTooLarge with the offending ids, got {err:?}"
        );
    }
}
