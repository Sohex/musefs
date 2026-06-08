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

#[test]
fn fallback_chain_uses_first_present_candidate() {
    let f = fields(&[("artist", "Beck"), ("title", "Loser")]);
    let path = Template::parse("${albumartist|artist}/$title").render(
        &f,
        &BTreeMap::new(),
        "Unknown",
        "flac",
    );
    assert_eq!(path, "Beck/Loser.flac");
}

#[test]
fn fallback_chain_skips_empty_value() {
    let f = fields(&[("albumartist", ""), ("artist", "Beck"), ("title", "Loser")]);
    let path = Template::parse("${albumartist|artist}/$title").render(
        &f,
        &BTreeMap::new(),
        "Unknown",
        "flac",
    );
    assert_eq!(path, "Beck/Loser.flac");
}

#[test]
fn fallback_chain_all_empty_falls_to_default_at_top_level() {
    let f = fields(&[("title", "Loser")]);
    let path = Template::parse("${albumartist|artist}/$title").render(
        &f,
        &BTreeMap::new(),
        "Unknown",
        "flac",
    );
    assert_eq!(path, "Unknown/Loser.flac");
}

#[test]
fn field_names_are_case_insensitive() {
    let f = fields(&[("albumartist", "VA")]);
    let path = Template::parse("$AlbumArtist").render(&f, &BTreeMap::new(), "Unknown", "flac");
    assert_eq!(path, "VA.flac");
}

#[test]
fn section_suppressed_when_field_absent() {
    let f = fields(&[("album", "LP")]);
    let path =
        Template::parse("$album[ - CD $disc]").render(&f, &BTreeMap::new(), "Unknown", "flac");
    assert_eq!(path, "LP.flac");
}

#[test]
fn section_emitted_when_field_present() {
    let f = fields(&[("album", "LP"), ("disc", "2")]);
    let path =
        Template::parse("$album[ - CD $disc]").render(&f, &BTreeMap::new(), "Unknown", "flac");
    assert_eq!(path, "LP - CD 2.flac");
}

#[test]
fn nested_section_outer_kept_inner_dropped() {
    let f = fields(&[("artist", "AC"), ("album", "LP"), ("title", "Song")]);
    let path = Template::parse("$artist[/[$date - ]$album]/$title").render(
        &f,
        &BTreeMap::new(),
        "Unknown",
        "flac",
    );
    assert_eq!(path, "AC/LP/Song.flac");
}

#[test]
fn nested_section_inner_present_renders_prefix() {
    let f = fields(&[
        ("artist", "AC"),
        ("date", "1999"),
        ("album", "LP"),
        ("title", "Song"),
    ]);
    let path = Template::parse("$artist[/[$date - ]$album]/$title").render(
        &f,
        &BTreeMap::new(),
        "Unknown",
        "flac",
    );
    assert_eq!(path, "AC/1999 - LP/Song.flac");
}

#[test]
fn section_all_referenced_fields_empty_is_suppressed() {
    let f = fields(&[("album", "LP")]);
    let path =
        Template::parse("$album[ $[$date$]]").render(&f, &BTreeMap::new(), "Unknown", "flac");
    assert_eq!(path, "LP.flac");
}

#[test]
fn escaped_brackets_render_literally_inside_kept_section() {
    let f = fields(&[("album", "LP"), ("date", "1999")]);
    let path =
        Template::parse("$album[ $[$date$]]").render(&f, &BTreeMap::new(), "Unknown", "flac");
    assert_eq!(path, "LP [1999].flac");
}

#[test]
fn empty_field_inside_kept_section_renders_blank_not_default() {
    let f = fields(&[("album", "LP")]);
    let path = Template::parse("[$album$disc]").render(&f, &BTreeMap::new(), "Unknown", "flac");
    assert_eq!(path, "LP.flac");
}

#[test]
fn empty_section_emits_nothing() {
    let f = fields(&[("title", "Song")]);
    let path = Template::parse("$title[]").render(&f, &BTreeMap::new(), "Unknown", "flac");
    assert_eq!(path, "Song.flac");
}

#[test]
fn path_field_keeps_slashes_as_separators() {
    let f = fields(&[("p", "Pink Floyd/Animals/01 Pigs")]);
    let path = Template::parse("$!{p}").render(&f, &BTreeMap::new(), "Unknown", "flac");
    assert_eq!(path, "Pink Floyd/Animals/01 Pigs.flac");
}

#[test]
fn path_field_drops_empty_and_dot_segments() {
    let f = fields(&[("p", "a//../b/./c")]);
    let path = Template::parse("$!{p}").render(&f, &BTreeMap::new(), "Unknown", "flac");
    assert_eq!(path, "a/b/c.flac");
}

#[test]
fn path_field_all_segments_dropped_falls_to_default() {
    let f = fields(&[("p", "..")]);
    let path = Template::parse("$!{p}").render(&f, &BTreeMap::new(), "Unknown", "flac");
    assert_eq!(path, "Unknown.flac");
}

#[test]
fn path_field_fallback_chain() {
    let f = fields(&[("lidarr_path", "Artist/Album/Song")]);
    let path = Template::parse("$!{beets_path|lidarr_path}").render(
        &f,
        &BTreeMap::new(),
        "Unknown",
        "flac",
    );
    assert_eq!(path, "Artist/Album/Song.flac");
}

#[test]
fn path_field_sanitizes_control_chars_within_segments() {
    let f = fields(&[("p", "a\u{0001}b/c")]);
    let path = Template::parse("$!{p}").render(&f, &BTreeMap::new(), "Unknown", "flac");
    assert_eq!(path, "a_b/c.flac");
}

#[test]
fn escaped_brackets_at_top_level_render_literally() {
    let f = fields(&[("title", "Song")]);
    let path = Template::parse("$[$title$]").render(&f, &BTreeMap::new(), "Unknown", "flac");
    assert_eq!(path, "[Song].flac");
}

#[test]
fn stray_closing_bracket_is_literal() {
    let f = fields(&[("title", "Song")]);
    let path = Template::parse("$title]").render(&f, &BTreeMap::new(), "Unknown", "flac");
    assert_eq!(path, "Song].flac");
}

#[test]
fn unterminated_section_runs_to_end_of_input() {
    let f = fields(&[("album", "LP"), ("disc", "2")]);
    let path = Template::parse("$album[ CD $disc").render(&f, &BTreeMap::new(), "Unknown", "flac");
    assert_eq!(path, "LP CD 2.flac");
}

#[test]
fn dollar_bang_without_brace_stays_literal() {
    let f = fields(&[("title", "Song")]);
    let path = Template::parse("$!x/$title").render(&f, &BTreeMap::new(), "Unknown", "flac");
    assert_eq!(path, "$!x/Song.flac");
}

#[test]
fn empty_braced_field_is_absent() {
    let f = fields(&[("title", "Song")]);
    let path = Template::parse("${}/$title").render(&f, &BTreeMap::new(), "Unknown", "flac");
    assert_eq!(path, "Unknown/Song.flac");
}
