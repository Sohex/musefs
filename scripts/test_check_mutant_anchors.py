import sys
from pathlib import Path

import pytest

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
    with pytest.raises(ValueError):
        g.parse_guard_tag(" bogus=1")


def test_parse_guard_tag_rejects_malformed_residue():
    # stray spaces around '=' would otherwise be silently dropped to defaults
    with pytest.raises(ValueError):
        g.parse_guard_tag(" count = 3")
    with pytest.raises(ValueError):
        g.parse_guard_tag(' op="<" garbage rows=3')


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
    with pytest.raises(ValueError):
        g.parse_mutant("not a mutant name")


def test_classify_linecol_vs_desc():
    assert g.classify(r"musefs-core/src/scan\.rs:277:30:") == "linecol"
    pat = r"musefs-format/src/convert\.rs:\d+:\d+: replace usize_from -> usize"
    assert g.classify(pat) == "desc"
    assert g.classify(r"replace < with <= in Musefs::poll_due") == "desc"


def test_validate_regex_subset_accepts_current_constructs():
    patterns = [
        r"musefs-core/src/scan\.rs:277:30:",
        r"replace < with (==|>|<=) in crc_shift_zeros",
        r"musefs-core/src/reader\.rs:71:60: replace / with [%*]",
        r"replace match guard .* with false in VirtualTree::apply_changes",
        r"replace \+ with \* in fixtures::wav",
        r'replace truncate_component -> Cow<._, str> with Cow::Borrowed\(""\)',
    ]
    for pat in patterns:
        g.validate_regex_subset(pat)  # must not raise


def test_validate_regex_subset_rejects_divergent_escape():
    with pytest.raises(ValueError):
        g.validate_regex_subset(r"replace \b foo")
    with pytest.raises(ValueError):
        g.validate_regex_subset(r"replace \w+ foo")


def test_validate_regex_subset_rejects_inline_group():
    with pytest.raises(ValueError):
        g.validate_regex_subset(r"foo(?=bar)")


SAMPLE_TOML = """\
exclude_globs = [
    "musefs-fuse/**",
    "musefs-core/src/metrics.rs",
]
exclude_re = [
    # a bare line:col entry
    # guard: op="<" fn="probe_file" rows=3
    'musefs-core/src/scan\\.rs:277:30:',

    # a description entry, multi-site (note the blank line above — must be skipped)
    # guard: count=3
    'replace \\| with \\^ in synchsafe_decode',
    # an untagged description entry (defaults count=1)
    'replace == with != in VirtualTree::ancestor_in',
]
"""


def test_parse_toml_entries_pairs_tags():
    entries, globs = g.parse_toml_entries(SAMPLE_TOML)
    assert globs == ["musefs-fuse/**", "musefs-core/src/metrics.rs"]
    assert len(entries) == 3
    assert entries[0].regex == r"musefs-core/src/scan\.rs:277:30:"
    assert entries[0].tag.op == "<" and entries[0].tag.rows == 3
    assert entries[1].regex == r"replace \| with \^ in synchsafe_decode"
    assert entries[1].tag.count == 3
    assert entries[2].tag is None  # untagged → default later


def test_parse_toml_entries_last_guard_wins():
    toml = (
        "exclude_re = [\n"
        "    # guard: count=2\n"
        "    # guard: count=5\n"
        "    'replace a with b in foo',\n"
        "]\n"
    )
    entries, _ = g.parse_toml_entries(toml)
    assert entries[0].tag.count == 5


def test_parse_toml_entries_hash_inside_regex_not_a_comment():
    toml = "exclude_re = [\n    'replace # with x in foo',\n]\n"
    entries, _ = g.parse_toml_entries(toml)
    assert entries[0].regex == "replace # with x in foo"
    assert entries[0].tag is None


def _m(name: str) -> g.Mutant:
    return g.parse_mutant(name)


def test_check_linecol_ok():
    entries = [
        g.Entry(
            r"musefs-core/src/scan\.rs:277:30:",
            1,
            g.Tag(op="<", fn="probe_file", fn_present=True, rows=3),
        )
    ]
    muts = [
        _m("musefs-core/src/scan.rs:277:30: replace < with == in probe_file"),
        _m("musefs-core/src/scan.rs:277:30: replace < with > in probe_file"),
        _m("musefs-core/src/scan.rs:277:30: replace < with <= in probe_file"),
    ]
    assert g.check(entries, muts, []) == []


def test_check_linecol_drift_to_nothing():
    entries = [
        g.Entry(
            r"musefs-core/src/scan\.rs:713:29:",
            1,
            g.Tag(op="+=", fn="run_pipeline", fn_present=True, rows=2),
        )
    ]
    fails = g.check(entries, [], [])
    assert len(fails) == 1 and "found none" in fails[0]


def test_check_linecol_repoint_wrong_op():
    entries = [
        g.Entry(
            r"musefs-core/src/scan\.rs:277:30:",
            1,
            g.Tag(op="<", fn="probe_file", fn_present=True, rows=1),
        )
    ]
    muts = [_m("musefs-core/src/scan.rs:277:30: replace + with - in probe_file")]
    fails = g.check(entries, muts, [])
    assert any("expected op" in f for f in fails)


def test_check_linecol_over_suppress_rows():
    entries = [
        g.Entry(
            r"musefs-core/src/ogg_index\.rs:216:15:",
            1,
            g.Tag(op="<", fn="serve_ogg_window", fn_present=True, rows=1),
        )
    ]
    muts = [
        _m("musefs-core/src/ogg_index.rs:216:15: replace < with == in serve_ogg_window"),
        _m("musefs-core/src/ogg_index.rs:216:15: replace < with > in serve_ogg_window"),
        _m("musefs-core/src/ogg_index.rs:216:15: replace < with <= in serve_ogg_window"),
    ]
    fails = g.check(entries, muts, [])
    assert any("rows=1" in f for f in fails)


def test_check_linecol_const_empty_fn():
    entries = [
        g.Entry(
            r"musefs-core/src/reader\.rs:71:60: replace / with [%*]",
            1,
            g.Tag(op="/", fn="", fn_present=True, rows=2),
        )
    ]
    muts = [
        _m("musefs-core/src/reader.rs:71:60: replace / with %"),
        _m("musefs-core/src/reader.rs:71:60: replace / with *"),
    ]
    assert g.check(entries, muts, []) == []


def test_check_linecol_missing_field():
    entries = [g.Entry(r"musefs-core/src/scan\.rs:277:30:", 1, g.Tag(op="<"))]
    muts = [_m("musefs-core/src/scan.rs:277:30: replace < with == in probe_file")]
    fails = g.check(entries, muts, [])
    assert any("needs `op=`, `fn=`, and `rows=`" in f for f in fails)


def test_check_desc_site_count_ok_multisite():
    entries = [g.Entry(r"replace \| with \^ in synchsafe_decode", 1, g.Tag(count=3))]
    muts = [
        _m("musefs-format/src/mp3.rs:10:5: replace | with ^ in synchsafe_decode"),
        _m("musefs-format/src/mp3.rs:11:5: replace | with ^ in synchsafe_decode"),
        _m("musefs-format/src/mp3.rs:12:5: replace | with ^ in synchsafe_decode"),
    ]
    assert g.check(entries, muts, []) == []


def test_check_desc_sibling_mask():
    entries = [g.Entry(r"replace \| with \^ in synchsafe_decode", 1, None)]
    muts = [
        _m("musefs-format/src/mp3.rs:10:5: replace | with ^ in synchsafe_decode"),
        _m("musefs-format/src/mp3.rs:99:5: replace | with ^ in synchsafe_decode"),
    ]
    fails = g.check(entries, muts, [])
    assert any("count=1" in f for f in fails)


def test_check_desc_dead_entry_zero():
    entries = [g.Entry(r"replace == with != in VirtualTree::gone", 1, None)]
    fails = g.check(entries, [], [])
    assert any("count=1" in f for f in fails)


def test_check_desc_count_zero_rejected():
    entries = [g.Entry(r"replace == with != in VirtualTree::gone", 1, g.Tag(count=0))]
    fails = g.check(entries, [], [])
    assert any("invalid" in f for f in fails)


def test_check_desc_over_nonbinop_matches():
    entries = [
        g.Entry(r'replace truncate_component -> Cow<._, str> with Cow::Borrowed\(""\)', 1, None)
    ]
    muts = [
        _m(
            "musefs-core/src/tree.rs:848:5: replace truncate_component"
            ' -> Cow<\'_, str> with Cow::Borrowed("")'
        ),
    ]
    assert g.check(entries, muts, []) == []


def test_check_glob_excluded_match_fails():
    entries = [g.Entry(r"metrics\.rs:\d+:\d+: replace \+ with -", 1, None)]
    muts = [_m("musefs-core/src/metrics.rs:5:5: replace + with - in bump")]
    fails = g.check(entries, muts, ["musefs-core/src/metrics.rs"])
    assert any("exclude_globs" in f for f in fails)


def test_check_uncompilable_regex_reported():
    entries = [g.Entry(r"replace \q foo", 1, None)]
    fails = g.check(entries, [], [])
    assert any("regex error" in f for f in fails)


def test_load_mutants_from_json():
    payload = (
        '[{"name": "musefs-core/src/scan.rs:277:30: replace < with == in probe_file",'
        ' "file": "musefs-core/src/scan.rs"}]'
    )
    muts = g.load_mutants(payload)
    assert len(muts) == 1
    assert muts[0].site == ("musefs-core/src/scan.rs", 277, 30)


def test_load_mutants_empty_is_error():
    with pytest.raises(ValueError):
        g.load_mutants("[]")
