import pytest

pytest.importorskip("picard")


def test_plugin_imports_with_picard_present():
    import musefs

    assert musefs._PICARD is True


def test_adapter_symbols_defined():
    import musefs

    assert hasattr(musefs, "MusefsSync")
    assert hasattr(musefs, "MusefsOptionsPage")
    assert callable(musefs._do_sync)
