def test_core_imports_without_beets():
    import beetsplug._core as core

    assert hasattr(core, "EXPECTED_USER_VERSION")
    assert core.EXPECTED_USER_VERSION == 1
    assert core.MAX_ART_BYTES == 16 * 1024 * 1024 - 64 * 1024
