def test_core_imports_without_beets():
    import beetsplug._core as core

    assert hasattr(core, "EXPECTED_USER_VERSION")
    assert core.EXPECTED_USER_VERSION == 1
