use std::collections::BTreeMap;

use musefs_db::{Db, Tag};
use musefs_format::{ArtInput, TagInput};

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
pub(crate) fn tags_to_fields(tags: &[Tag]) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for t in tags {
        map.entry(t.key.clone()).or_insert_with(|| t.value.clone());
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
