use musefs_db::Format;

#[test]
fn format_round_trips_through_db_string() {
    assert_eq!(Format::Flac.as_str(), "flac");
    assert_eq!(Format::Mp3.as_str(), "mp3");
    assert_eq!("flac".parse::<Format>(), Ok(Format::Flac));
    assert_eq!("mp3".parse::<Format>(), Ok(Format::Mp3));
    assert_eq!(
        "ogg".parse::<Format>(),
        Err(strum::ParseError::VariantNotFound)
    );
}
