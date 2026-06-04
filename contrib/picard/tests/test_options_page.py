import pytest

pytest.importorskip("picard")
pytest.importorskip("pytestqt")  # qtbot fixture


def test_options_page_load_reflects_config(qtbot, picard_config):
    import musefs

    picard_config.setting["musefs_db"] = "/tmp/seed.db"
    picard_config.setting["musefs_bin"] = "musefs"
    picard_config.setting["musefs_autoscan"] = True
    picard_config.setting["musefs_fields"] = ""

    page = musefs.MusefsOptionsPage()
    qtbot.addWidget(page)
    page.load()

    assert page._db.text() == "/tmp/seed.db"
    assert page._autoscan.isChecked() is True


def test_options_page_save_writes_config(qtbot, picard_config):
    import musefs

    page = musefs.MusefsOptionsPage()
    qtbot.addWidget(page)
    page.load()
    page._db.setText("/tmp/edited.db")
    page._autoscan.setChecked(False)
    page.save()

    assert picard_config.setting["musefs_db"] == "/tmp/edited.db"
    assert picard_config.setting["musefs_autoscan"] is False
