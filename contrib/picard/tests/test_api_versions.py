from musefs import PLUGIN_API_VERSIONS


def test_declares_only_the_api_floor():
    """Picard's loader intersects this list with picard.api_versions, which
    every 2.x release keeps back-filled to "2.0" — so the floor alone loads
    everywhere. A hand-extended list would reintroduce the per-release
    maintenance issue #140 complains about."""
    assert PLUGIN_API_VERSIONS == ["2.0"]
