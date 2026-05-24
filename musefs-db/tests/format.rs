use musefs_db::Format;

#[test]
fn format_round_trips_through_db_string() {
    assert_eq!(Format::Flac.as_str(), "flac");
    assert_eq!(Format::Mp3.as_str(), "mp3");
    assert_eq!(Format::parse("flac"), Some(Format::Flac));
    assert_eq!(Format::parse("mp3"), Some(Format::Mp3));
    assert_eq!(Format::parse("ogg"), None);
}
