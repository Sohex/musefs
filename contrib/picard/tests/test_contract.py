from musefs._common.contract import CONTRACT_EXPECTED, CONTRACT_VALUES, normalize_rows
from musefs._core import map_fields


def test_picard_satisfies_contract(fake_metadata):
    # FakeMetadata wraps scalars to single-element lists; getall returns them.
    md = fake_metadata(**CONTRACT_VALUES)
    assert normalize_rows(map_fields(md)) == normalize_rows(CONTRACT_EXPECTED)
