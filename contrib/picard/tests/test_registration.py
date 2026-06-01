import importlib
import sys

import pytest

pytest.importorskip("picard")


def test_plugin_registers_actions_and_options_page(monkeypatch):
    import picard.ui.itemviews as itemviews
    import picard.ui.options as options

    calls = {}

    def record(name):
        def _spy(arg):
            calls.setdefault(name, []).append(arg)

        return _spy

    monkeypatch.setattr(itemviews, "register_file_action", record("file"))
    monkeypatch.setattr(itemviews, "register_track_action", record("track"))
    monkeypatch.setattr(itemviews, "register_album_action", record("album"))
    monkeypatch.setattr(itemviews, "register_cluster_action", record("cluster"))
    monkeypatch.setattr(options, "register_options_page", record("options"))

    # Force the module-level registration to re-run against the spies.
    sys.modules.pop("musefs", None)
    musefs = importlib.import_module("musefs")

    # The four item actions register exactly once, all the SAME instance.
    for kind in ("file", "track", "album", "cluster"):
        assert len(calls[kind]) == 1, kind
    action = calls["file"][0]
    assert isinstance(action, musefs.MusefsSync)
    assert all(calls[k][0] is action for k in ("track", "album", "cluster"))

    # The options page registers the class.
    assert calls["options"] == [musefs.MusefsOptionsPage]


@pytest.fixture(autouse=True)
def _reimport_clean():
    """Drop the forced re-import so later tests get a normally-imported module."""
    yield
    sys.modules.pop("musefs", None)
