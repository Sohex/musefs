use musefs_core::render_path;
use std::collections::BTreeMap;

fn fields(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[test]
fn substitutes_dollar_and_braced_fields_and_appends_ext() {
    let f = fields(&[
        ("albumartist", "Pink Floyd"),
        ("album", "Animals"),
        ("title", "Pigs"),
    ]);
    let path = render_path(
        "$albumartist/${album}/$title",
        &f,
        &BTreeMap::new(),
        "Unknown",
        "flac",
    );
    assert_eq!(path, "Pink Floyd/Animals/Pigs.flac");
}

#[test]
fn missing_field_uses_per_field_fallback_then_default() {
    let f = fields(&[("title", "Untitled Track")]);
    let fallbacks = fields(&[("albumartist", "Unknown Artist")]);
    let path = render_path(
        "$albumartist/$album/$title",
        &f,
        &fallbacks,
        "Unknown",
        "flac",
    );
    assert_eq!(path, "Unknown Artist/Unknown/Untitled Track.flac");
}

#[test]
fn sanitizes_path_illegal_characters_in_values() {
    let f = fields(&[("artist", "AC/DC"), ("title", "Back\u{0000}In")]);
    let path = render_path("$artist/$title", &f, &BTreeMap::new(), "Unknown", "flac");
    assert_eq!(path, "AC_DC/Back_In.flac");
}
