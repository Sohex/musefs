from musefs_common.contract import CONTRACT_EXPECTED, normalize_rows


def test_normalize_groups_and_sorts():
    norm = normalize_rows(CONTRACT_EXPECTED)
    assert norm["genre"] == ["Pop", "Rock"]
    assert norm["composer"] == ["Carol", "Dave"]
    assert norm["title"] == ["Song"]


def test_normalize_is_order_insensitive():
    shuffled = list(reversed(CONTRACT_EXPECTED))
    assert normalize_rows(shuffled) == normalize_rows(CONTRACT_EXPECTED)
