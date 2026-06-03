from musefs._core import parse_field_map


def test_value_with_comma_preserved():
    result = parse_field_map("comment=This is a great, upbeat song")
    assert result == {"comment": "This is a great, upbeat song"}


def test_multiple_lines_parsed():
    result = parse_field_map("comment=hello\ngrouping=My Set")
    assert result == {"comment": "hello", "grouping": "My Set"}


def test_blank_and_invalid_lines_skipped():
    result = parse_field_map("\ncomment=hi\nnot a mapping\n  \nkey=value\n")
    assert result == {"comment": "hi", "key": "value"}


def test_empty_text_returns_empty():
    assert parse_field_map("") == {}
    assert parse_field_map(None) == {}
