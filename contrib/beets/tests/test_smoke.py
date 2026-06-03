def test_core_imports_without_beets():
    # The slimmed _core depends only on musefs_common, never on beets itself.
    import beetsplug._core as core

    assert hasattr(core, "DIRECT_FIELDS")
    assert hasattr(core, "map_fields")
    assert hasattr(core, "build_records")
