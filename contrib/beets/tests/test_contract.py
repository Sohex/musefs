from types import SimpleNamespace

from musefs_common.contract import CONTRACT_EXPECTED, CONTRACT_VALUES, normalize_rows

from beetsplug._core import map_fields


def _beets_item():
    # beets carries multi-value tags as the list fields genres/composers.
    return SimpleNamespace(
        title=CONTRACT_VALUES["title"],
        artist=CONTRACT_VALUES["artist"],
        albumartist=CONTRACT_VALUES["albumartist"],
        album=CONTRACT_VALUES["album"],
        genres=list(CONTRACT_VALUES["genre"]),
        composers=list(CONTRACT_VALUES["composer"]),
        genre="",
        composer="",
        track=0,
        disc=0,
        year=0,
        month=0,
        day=0,
    )


def test_beets_satisfies_contract():
    assert normalize_rows(map_fields(_beets_item())) == normalize_rows(CONTRACT_EXPECTED)
