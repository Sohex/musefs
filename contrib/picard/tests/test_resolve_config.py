from musefs._core import parse_field_map, resolve_config


def test_page_values_used_when_no_env():
    settings = {"musefs_db": "/page.db", "musefs_bin": "/page/musefs", "musefs_autoscan": False}
    opts = resolve_config(settings, environ={})
    assert opts.db == "/page.db"
    assert opts.bin == "/page/musefs"
    assert opts.autoscan is False


def test_env_overrides_page():
    settings = {"musefs_db": "/page.db", "musefs_bin": "/page/musefs"}
    environ = {"MUSEFS_DB": "/env.db", "MUSEFS_BIN": "/env/musefs"}
    opts = resolve_config(settings, environ)
    assert opts.db == "/env.db"
    assert opts.bin == "/env/musefs"


def test_defaults_when_unset():
    opts = resolve_config(settings={}, environ={})
    assert opts.db is None
    assert opts.bin == "musefs"
    assert opts.autoscan is True
    assert opts.fields == {}


def test_autoscan_and_fields_have_no_env_form():
    # Only DB/BIN read env; autoscan/fields come from the page regardless of env.
    settings = {"musefs_autoscan": False, "musefs_fields": "comment=comment"}
    environ = {"MUSEFS_AUTOSCAN": "1", "MUSEFS_FIELDS": "x=y"}
    opts = resolve_config(settings, environ)
    assert opts.autoscan is False
    assert opts.fields == {"comment": "comment"}


def test_fields_accepts_dict_directly():
    opts = resolve_config({"musefs_fields": {"comment": "comment"}}, environ={})
    assert opts.fields == {"comment": "comment"}


def test_parse_field_map_variants():
    assert parse_field_map("") == {}
    assert parse_field_map("comment=comment") == {"comment": "comment"}
    # Newline-separated entries (was comma-separated):
    assert parse_field_map("a=b\nc=d") == {"a": "b", "c": "d"}
    assert parse_field_map("a=b\n c=d ") == {"a": "b", "c": "d"}
    # A comma inside a value is now literal, not a separator:
    assert parse_field_map("comment=a, b, c") == {"comment": "a, b, c"}
    # Lines without '=' or with an empty key/value are skipped:
    assert parse_field_map("noequals\n=novalue\nkey=") == {}
