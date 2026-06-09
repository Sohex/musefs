import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))

import check_mutant_anchors as g  # noqa: E402


def test_parse_guard_tag_linecol():
    t = g.parse_guard_tag(' op="<" fn="probe_file" rows=3')
    assert t.op == "<"
    assert t.fn == "probe_file"
    assert t.fn_present is True
    assert t.rows == 3
    assert t.count is None


def test_parse_guard_tag_const_empty_fn():
    t = g.parse_guard_tag(' op="/" fn="" rows=2')
    assert t.op == "/"
    assert t.fn == ""
    assert t.fn_present is True
    assert t.rows == 2


def test_parse_guard_tag_count():
    t = g.parse_guard_tag(" count=3")
    assert t.count == 3
    assert t.op is None
    assert t.fn_present is False


def test_parse_guard_tag_rejects_unknown_field():
    import pytest

    with pytest.raises(ValueError):
        g.parse_guard_tag(" bogus=1")


def test_parse_mutant_binop_with_fn():
    m = g.parse_mutant("musefs-core/src/scan.rs:277:30: replace < with == in probe_file")
    assert m.site == ("musefs-core/src/scan.rs", 277, 30)
    assert m.op == "<"
    assert m.repl == "=="
    assert m.fn == "probe_file"


def test_parse_mutant_binop_const_no_fn():
    m = g.parse_mutant("musefs-core/src/reader.rs:71:60: replace / with %")
    assert m.site == ("musefs-core/src/reader.rs", 71, 60)
    assert m.op == "/"
    assert m.repl == "%"
    assert m.fn is None


def test_parse_mutant_fnvalue_is_site_only():
    m = g.parse_mutant("musefs-format/src/convert.rs:21:5: replace usize_from -> usize with 0")
    assert m.site == ("musefs-format/src/convert.rs", 21, 5)
    assert m.op is None and m.repl is None and m.fn is None


def test_parse_mutant_matchguard_is_site_only():
    m = g.parse_mutant(
        "musefs-core/src/tree.rs:641:30: replace match guard"
        " self.path_of(ino) == new_path with false"
        " in VirtualTree::apply_changes"
    )
    assert m.site == ("musefs-core/src/tree.rs", 641, 30)
    assert m.op is None and m.fn is None


def test_parse_mutant_unary_delete_is_site_only():
    m = g.parse_mutant("musefs-core/src/scan.rs:874:12: delete ! in revalidate_with")
    assert m.op is None and m.fn is None


def test_parse_mutant_rejects_no_prefix():
    import pytest

    with pytest.raises(ValueError):
        g.parse_mutant("not a mutant name")
