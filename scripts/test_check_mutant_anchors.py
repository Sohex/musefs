import json
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


def test_wildcard_coords_bare_prefix():
    assert g._wildcard_coords(r"musefs-core/src/scan\.rs:1041:32:") == (
        r"musefs-core/src/scan\.rs:\d+:\d+:"
    )


def test_wildcard_coords_preserves_repl_suffix():
    assert g._wildcard_coords(r"musefs-core/src/scan\.rs:1212:29: replace \+ with -") == (
        r"musefs-core/src/scan\.rs:\d+:\d+: replace \+ with -"
    )


def test_wildcard_coords_replaces_only_first_linecol():
    # the suffix's own digits must survive untouched
    assert g._wildcard_coords(r"foo/bar\.rs:10:20: replace 1 with 2") == (
        r"foo/bar\.rs:\d+:\d+: replace 1 with 2"
    )


def test_entry_coords_parses_line_col():
    assert g._entry_coords(r"musefs-core/src/scan\.rs:1041:32:") == (1041, 32)
    assert g._entry_coords(r"musefs-core/src/scan\.rs:1212:29: replace \+ with -") == (1212, 29)


def test_candidate_mutants_bare_prefix_filters_by_op_fn():
    entry = g.Entry(
        r"musefs-core/src/scan\.rs:277:30:",
        1,
        g.Tag(op="<", fn="probe_file", fn_present=True, rows=1),
    )
    muts = [
        _m("musefs-core/src/scan.rs:300:30: replace < with == in probe_file"),
        _m("musefs-core/src/scan.rs:305:10: replace + with - in probe_file"),  # wrong op
        _m("musefs-core/src/scan.rs:310:10: replace < with == in other_fn"),  # wrong fn
    ]
    got = g._candidate_mutants(entry, muts)
    assert [m.site for m in got] == [("musefs-core/src/scan.rs", 300, 30)]


def test_candidate_mutants_repl_suffix_narrows():
    entry = g.Entry(
        r"musefs-core/src/scan\.rs:1212:29: replace \+ with -",
        1,
        g.Tag(op="+", fn="revalidate_with", fn_present=True, rows=1),
    )
    muts = [
        _m("musefs-core/src/scan.rs:1220:29: replace + with - in revalidate_with"),
        _m("musefs-core/src/scan.rs:1220:29: replace + with * in revalidate_with"),
        # ^ suffix mismatch — different repl
    ]
    got = g._candidate_mutants(entry, muts)
    assert [m.repl for m in got] == ["-"]


def test_candidate_mutants_empty_fn_matches_free_function():
    entry = g.Entry(
        r"musefs-core/src/reader\.rs:71:60: replace / with [%*]",
        1,
        g.Tag(op="/", fn="", fn_present=True, rows=2),
    )
    muts = [
        _m("musefs-core/src/reader.rs:80:60: replace / with %"),
        _m("musefs-core/src/reader.rs:80:60: replace / with *"),
    ]
    got = g._candidate_mutants(entry, muts)
    assert {m.site for m in got} == {("musefs-core/src/reader.rs", 80, 60)}
    assert len(got) == 2


def test_compute_rewrites_simple_shift():
    entries = [
        g.Entry(
            r"musefs-core/src/scan\.rs:277:30:",
            3,
            g.Tag(op="<", fn="probe_file", fn_present=True, rows=1),
        )
    ]
    muts = [_m("musefs-core/src/scan.rs:300:30: replace < with == in probe_file")]
    rewrites, skips, skipped = g.compute_rewrites(entries, muts)
    assert skips == [] and skipped == set()
    assert len(rewrites) == 1
    assert (rewrites[0].entry.toml_line, rewrites[0].line, rewrites[0].col) == (3, 300, 30)


def test_compute_rewrites_no_drift_is_noop():
    entries = [
        g.Entry(
            r"musefs-core/src/scan\.rs:277:30:",
            3,
            g.Tag(op="<", fn="probe_file", fn_present=True, rows=1),
        )
    ]
    muts = [_m("musefs-core/src/scan.rs:277:30: replace < with == in probe_file")]
    rewrites, skips, skipped = g.compute_rewrites(entries, muts)
    assert rewrites == [] and skips == [] and skipped == set()


def test_compute_rewrites_multisite_positional():
    # two >= anchors in run_pipeline, both shifted down 2 lines; map low->low, high->high
    entries = [
        g.Entry(
            r"musefs-core/src/scan\.rs:1041:32:",
            5,
            g.Tag(op=">=", fn="run_pipeline", fn_present=True, rows=1),
        ),
        g.Entry(
            r"musefs-core/src/scan\.rs:1051:40:",
            9,
            g.Tag(op=">=", fn="run_pipeline", fn_present=True, rows=1),
        ),
    ]
    muts = [
        _m("musefs-core/src/scan.rs:1043:32: replace >= with > in run_pipeline"),
        _m("musefs-core/src/scan.rs:1053:40: replace >= with > in run_pipeline"),
    ]
    rewrites, skips, _ = g.compute_rewrites(entries, muts)
    assert skips == []
    assert {(r.entry.toml_line, r.line, r.col) for r in rewrites} == {
        (5, 1043, 32),
        (9, 1053, 40),
    }


def test_compute_rewrites_repl_suffixed():
    entries = [
        g.Entry(
            r"musefs-core/src/scan\.rs:1212:29: replace \+ with -",
            3,
            g.Tag(op="+", fn="revalidate_with", fn_present=True, rows=1),
        )
    ]
    muts = [
        _m("musefs-core/src/scan.rs:1220:29: replace + with - in revalidate_with"),
        _m("musefs-core/src/scan.rs:1220:29: replace + with * in revalidate_with"),
    ]
    rewrites, skips, _ = g.compute_rewrites(entries, muts)
    assert skips == []
    assert len(rewrites) == 1 and (rewrites[0].line, rewrites[0].col) == (1220, 29)


def test_compute_rewrites_rows_two_site():
    entries = [
        g.Entry(
            r"musefs-core/src/scan\.rs:1039:29:",
            3,
            g.Tag(op="+=", fn="run_pipeline", fn_present=True, rows=2),
        )
    ]
    muts = [
        _m("musefs-core/src/scan.rs:1045:29: replace += with -= in run_pipeline"),
        _m("musefs-core/src/scan.rs:1045:29: replace += with *= in run_pipeline"),
    ]
    rewrites, skips, _ = g.compute_rewrites(entries, muts)
    assert skips == []
    assert len(rewrites) == 1 and (rewrites[0].line, rewrites[0].col) == (1045, 29)


def test_compute_rewrites_zero_candidate_reports_deletion():
    entries = [
        g.Entry(
            r"musefs-core/src/scan\.rs:277:30:",
            4,
            g.Tag(op="<", fn="probe_file", fn_present=True, rows=1),
        )
    ]
    muts = [_m("musefs-core/src/scan.rs:500:10: replace + with - in other_fn")]
    rewrites, skips, skipped = g.compute_rewrites(entries, muts)
    assert rewrites == [] and skipped == {4}
    assert len(skips) == 1 and "delete this exclude_re entry" in skips[0]


def test_compute_rewrites_structural_count_mismatch():
    # one anchor, op/fn now resolves to two sites -> refuse the whole group
    entries = [
        g.Entry(
            r"musefs-core/src/scan\.rs:1041:32:",
            4,
            g.Tag(op=">=", fn="run_pipeline", fn_present=True, rows=1),
        )
    ]
    muts = [
        _m("musefs-core/src/scan.rs:1043:32: replace >= with > in run_pipeline"),
        _m("musefs-core/src/scan.rs:1099:10: replace >= with > in run_pipeline"),
    ]
    rewrites, skips, skipped = g.compute_rewrites(entries, muts)
    assert rewrites == [] and skipped == {4}
    assert "structural change" in skips[0]


def test_compute_rewrites_rows_mismatch_skips_group():
    # equinumerous (1 anchor, 1 site) but the site holds 1 mutant while rows=2
    entries = [
        g.Entry(
            r"musefs-core/src/scan\.rs:1039:29:",
            4,
            g.Tag(op="+=", fn="run_pipeline", fn_present=True, rows=2),
        )
    ]
    muts = [_m("musefs-core/src/scan.rs:1045:29: replace += with -= in run_pipeline")]
    rewrites, skips, skipped = g.compute_rewrites(entries, muts)
    assert rewrites == [] and skipped == {4}
    assert "structural change" in skips[0]


def test_compute_rewrites_ignores_desc_and_tagless():
    entries = [
        g.Entry(r"replace \| with \^ in synchsafe_decode", 3, g.Tag(count=3)),
        g.Entry(r"musefs-core/src/scan\.rs:277:30:", 5, None),
        g.Entry(r"musefs-core/src/scan\.rs:277:30:", 7, g.Tag(op="<")),
    ]
    muts = [_m("musefs-core/src/scan.rs:277:30: replace < with == in probe_file")]
    rewrites, skips, skipped = g.compute_rewrites(entries, muts)
    assert rewrites == [] and skips == [] and skipped == set()


def test_apply_rewrites_byte_preserving():
    toml = (
        "exclude_re = [\n"
        '    # guard: op=">=" fn="run_pipeline" rows=1\n'
        "    'musefs-core/src/scan\\.rs:1041:32:',\n"
        "]\n"
    )
    entries, _ = g.parse_toml_entries(toml)
    out = g.apply_rewrites(toml, [g.Rewrite(entries[0], 1043, 32)])
    assert "scan\\.rs:1043:32:" in out
    assert "scan\\.rs:1041:32:" not in out
    # guard tag and overall structure untouched
    assert 'op=">=" fn="run_pipeline" rows=1' in out
    assert out.count("\n") == toml.count("\n")
    # only the coordinate digits changed
    assert out == toml.replace(":1041:32:", ":1043:32:")


def test_apply_rewrites_preserves_repl_suffix():
    toml = "exclude_re = [\n    'musefs-core/src/scan\\.rs:1212:29: replace \\+ with -',\n]\n"
    entries, _ = g.parse_toml_entries(toml)
    out = g.apply_rewrites(toml, [g.Rewrite(entries[0], 1220, 29)])
    assert "scan\\.rs:1220:29: replace \\+ with -" in out


def test_apply_rewrites_empty_is_identity():
    toml = "exclude_re = [\n    'musefs-core/src/scan\\.rs:1041:32:',\n]\n"
    assert g.apply_rewrites(toml, []) == toml


def _write_toml(tmp_path, body: str):
    p = tmp_path / "mutants.toml"
    p.write_text(body)
    return p


def _write_muts(tmp_path, names: list[str]):
    p = tmp_path / "muts.json"
    p.write_text(json.dumps([{"name": n} for n in names]))
    return p


_FIX_TOML = (
    "exclude_re = [\n"
    '    # guard: op="<" fn="probe_file" rows=1\n'
    "    'musefs-core/src/scan\\.rs:277:30:',\n"
    "]\n"
)


def test_main_fix_rewrites_and_passes(tmp_path, capsys):
    toml = _write_toml(tmp_path, _FIX_TOML)
    muts = _write_muts(
        tmp_path,
        ["musefs-core/src/scan.rs:300:30: replace < with == in probe_file"],
    )
    rc = g.main(["--fix", "--toml", str(toml), "--mutants-json", str(muts)])
    assert rc == 0
    assert "scan\\.rs:300:30:" in toml.read_text()
    assert "scan\\.rs:277:30:" not in toml.read_text()
    assert "re-anchored" in capsys.readouterr().out


def test_main_fix_idempotent(tmp_path):
    toml = _write_toml(tmp_path, _FIX_TOML)
    muts = _write_muts(
        tmp_path,
        ["musefs-core/src/scan.rs:300:30: replace < with == in probe_file"],
    )
    assert g.main(["--fix", "--toml", str(toml), "--mutants-json", str(muts)]) == 0
    after_first = toml.read_text()
    assert g.main(["--fix", "--toml", str(toml), "--mutants-json", str(muts)]) == 0
    assert toml.read_text() == after_first


def test_main_fix_partial_reports_and_exits_nonzero(tmp_path, capsys):
    # first entry is fixable; second resolves to no live mutant (deleted code)
    body = (
        "exclude_re = [\n"
        '    # guard: op="<" fn="probe_file" rows=1\n'
        "    'musefs-core/src/scan\\.rs:277:30:',\n"
        '    # guard: op=">" fn="gone" rows=1\n'
        "    'musefs-core/src/scan\\.rs:900:10:',\n"
        "]\n"
    )
    toml = _write_toml(tmp_path, body)
    muts = _write_muts(
        tmp_path,
        ["musefs-core/src/scan.rs:300:30: replace < with == in probe_file"],
    )
    rc = g.main(["--fix", "--toml", str(toml), "--mutants-json", str(muts)])
    assert rc == 1
    out = capsys.readouterr().out
    assert "scan\\.rs:300:30:" in toml.read_text()  # the fixable one was still applied
    assert "delete this exclude_re entry" in out


def test_main_fix_no_op_when_clean(tmp_path, capsys):
    toml = _write_toml(tmp_path, _FIX_TOML)
    muts = _write_muts(
        tmp_path,
        ["musefs-core/src/scan.rs:277:30: replace < with == in probe_file"],
    )
    rc = g.main(["--fix", "--toml", str(toml), "--mutants-json", str(muts)])
    assert rc == 0
    assert toml.read_text() == _FIX_TOML
    assert "no coordinates needed re-anchoring" in capsys.readouterr().out
