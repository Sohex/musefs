use musefs_core::Template;
use std::collections::BTreeMap;

fn fields<'a>(pairs: &[(&str, &'a str)]) -> BTreeMap<String, &'a str> {
    pairs.iter().map(|&(k, v)| (k.to_string(), v)).collect()
}

fn owned(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
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
    let path = Template::parse("$albumartist/${album}/$title").render(
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
    let fallbacks = owned(&[("albumartist", "Unknown Artist")]);
    let path =
        Template::parse("$albumartist/$album/$title").render(&f, &fallbacks, "Unknown", "flac");
    assert_eq!(path, "Unknown Artist/Unknown/Untitled Track.flac");
}

#[test]
fn sanitizes_path_illegal_characters_in_values() {
    let f = fields(&[("artist", "AC/DC"), ("title", "Back\u{0000}In")]);
    let path = Template::parse("$artist/$title").render(&f, &BTreeMap::new(), "Unknown", "flac");
    assert_eq!(path, "AC_DC/Back_In.flac");
}

#[test]
fn lone_dollar_stays_literal() {
    let f = fields(&[("title", "Song")]);
    // '$' before a non-field char, and a trailing '$', both stay literal;
    // the non-field char after the lone '$' is kept.
    let path = Template::parse("100$ bill/$title$").render(&f, &BTreeMap::new(), "Unknown", "mp3");
    assert_eq!(path, "100$ bill/Song$.mp3");
}

#[test]
fn unterminated_brace_consumes_rest_as_field_name() {
    let f = fields(&[("album", "X")]);
    let path = Template::parse("${album").render(&f, &BTreeMap::new(), "Unknown", "ogg");
    assert_eq!(path, "X.ogg");
}
