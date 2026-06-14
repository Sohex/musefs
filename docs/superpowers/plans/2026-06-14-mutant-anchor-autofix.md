# `check_mutant_anchors.py` `--fix` Auto-Re-Anchor Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a `--fix` flag to `scripts/check_mutant_anchors.py` that rewrites drifted `file:line:col` anchors in `.cargo/mutants.toml` in place, deriving each entry's current coordinates from the live cargo-mutants list and refusing to guess when a mapping is ambiguous.

**Architecture:** Pure functions layered on the existing parser: `_candidate_mutants` resolves an entry's current sites by `op`+`fn` (coordinates wildcarded out of its regex); `compute_rewrites` groups same-resolving "sibling" entries and maps them positionally by source order, leaving non-derivable cases for manual fix instead of rewriting them; `run_fix` re-derives the set of entries that *currently* fail `check()` and gates unfixable-entry reporting on it (so a valid but non-unique-op/fn anchor on a clean tree is a silent no-op), then applies the rewrites, re-parses from disk, and re-validates. `apply_rewrites` does a byte-preserving line-level coordinate swap. The pre-commit hook and CI stay read-only and merely point at `--fix`.

**Tech Stack:** Python 3 stdlib (`re`, `argparse`, `dataclasses`, `collections.Counter`), pytest. No new dependencies.

**Reference spec:** `docs/superpowers/specs/2026-06-14-mutant-anchor-autofix-design.md`

---

## File Structure

- **Modify** `scripts/check_mutant_anchors.py` — add helpers, `Rewrite` dataclass, `compute_rewrites`, `apply_rewrites`, `run_fix`, and a `--fix` argparse flag. All additive; no existing function signature changes.
- **Modify** `scripts/test_check_mutant_anchors.py` — add unit tests for each new function plus end-to-end `main(["--fix", ...])` tests.
- **Modify** `.githooks/pre-commit` — append a one-line `--fix` pointer to the existing failure message (stays read-only).
- **Modify** `CONTRIBUTING.md` — add a sentence to the "When the guard fails" paragraph pointing at `--fix`.

### Conventions to follow (already in the file)
- Tests import the script as `import check_mutant_anchors as g` and build mutants via the local `_m(name)` helper (`= g.parse_mutant(name)`).
- `g.Entry(regex, toml_line, tag)`, `g.Tag(op=, fn=, fn_present=, rows=, count=)`, `g.Mutant.site == (file, line, col)`.
- Failure strings are formatted by `_fmt(entry, msg)` → `"[mutants.toml:{toml_line}] /{regex}/ — {msg}"`.
- `toml_line` is **1-based** (`parse_toml_entries` uses `enumerate(..., start=1)`).
- `parse_toml_entries` unquotes by stripping the surrounding quote only — `entry.regex` is the exact literal text between the quotes, so it appears verbatim on its source line.

### Commit-time gotchas (apply to every Python commit below)
- The pre-commit hook runs `ruff check` **and** `ruff format --check` over the Python paths — run `ruff format scripts/check_mutant_anchors.py scripts/test_check_mutant_anchors.py` before each commit or it will be rejected as unformatted (config: `select = ["E","F","I","N","W"]`, line-length 100, `format.preview = true`).
- Staging `scripts/check_mutant_anchors.py` matches the pre-commit **mutant-anchor guard** trigger, so each of these commits also runs `cargo mutants --no-config --list --json` + the full workspace test suite (several minutes, needs `cargo-mutants` installed). This is expected; the guard should stay green because no `.cargo/mutants.toml` anchor changes. If `cargo-mutants` is absent the guard self-skips (CI still enforces it).

---

## Task 1: Coordinate helpers + `Rewrite` dataclass

**Files:**
- Modify: `scripts/check_mutant_anchors.py` (add `_COORD_RE` next to `_LITERAL_LINECOL`; add `Counter` import; add `_wildcard_coords`, `_entry_coords`, `Rewrite` after `parse_mutant`)
- Test: `scripts/test_check_mutant_anchors.py`

- [ ] **Step 1: Write the failing tests**

Add to `scripts/test_check_mutant_anchors.py` (near the other unit tests):

```python
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
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `python -m pytest scripts/test_check_mutant_anchors.py -k "wildcard_coords or entry_coords" -v`
Expected: FAIL with `AttributeError: module 'check_mutant_anchors' has no attribute '_wildcard_coords'`.

- [ ] **Step 3: Implement the helpers**

In `scripts/check_mutant_anchors.py`, add `Counter` to the imports — change:

```python
from dataclasses import dataclass
```

to:

```python
from collections import Counter
from dataclasses import dataclass
```

Add the capturing coordinate regex directly below the existing `_LITERAL_LINECOL` definition:

```python
_LITERAL_LINECOL = re.compile(r":[0-9]+:[0-9]+:")
_COORD_RE = re.compile(r":([0-9]+):([0-9]+):")
```

Add these helpers immediately after `parse_mutant` (before `classify`):

```python
def _wildcard_coords(regex: str) -> str:
    """Replace an entry regex's literal ``:line:col:`` with a ``:\\d+:\\d+:`` wildcard.

    Uses a function replacement, not a literal one: a string replacement of
    ``":\\d+:\\d+:"`` would have its ``\\d`` parsed as a replacement-template escape
    and raise ``re.PatternError: bad escape \\d``.
    """
    return _LITERAL_LINECOL.sub(lambda _: r":\d+:\d+:", regex, count=1)


def _entry_coords(regex: str) -> tuple[int, int]:
    """Parse the (line, col) a linecol entry currently anchors on."""
    m = _COORD_RE.search(regex)
    assert m is not None  # caller guarantees classify(regex) == "linecol"
    return int(m.group(1)), int(m.group(2))
```

Add the `Rewrite` dataclass immediately after the `Entry` dataclass:

```python
@dataclass
class Rewrite:
    entry: Entry
    line: int
    col: int
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `python -m pytest scripts/test_check_mutant_anchors.py -k "wildcard_coords or entry_coords" -v`
Expected: PASS (4 passed).

- [ ] **Step 5: Commit**

```bash
git add scripts/check_mutant_anchors.py scripts/test_check_mutant_anchors.py
git commit -m "feat(scripts): coordinate helpers for mutant-anchor --fix (#345)"
```

---

## Task 2: `_candidate_mutants` — resolve an entry's current sites

**Files:**
- Modify: `scripts/check_mutant_anchors.py` (add `_candidate_mutants` after `_entry_coords`)
- Test: `scripts/test_check_mutant_anchors.py`

- [ ] **Step 1: Write the failing tests**

```python
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
        _m("musefs-core/src/scan.rs:1220:29: replace + with * in revalidate_with"),  # excluded by suffix
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
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `python -m pytest scripts/test_check_mutant_anchors.py -k candidate_mutants -v`
Expected: FAIL with `AttributeError: ... has no attribute '_candidate_mutants'`.

- [ ] **Step 3: Implement `_candidate_mutants`**

Add after `_entry_coords`:

```python
def _candidate_mutants(entry: Entry, mutants: list[Mutant]) -> list[Mutant]:
    """Live mutants a linecol entry's op/fn currently resolve to, ignoring its stale coords.

    The entry's regex is matched with its coordinates wildcarded — preserving any
    ``replace \\+ with -`` repl suffix that narrows the match — and the result is
    further filtered by the guard tag's ``op`` and ``fn`` (``fn=""`` → free function,
    matched as ``fn is None``). Mirrors the predicate ``_check_linecol`` validates with.
    """
    t = entry.tag
    wild = re.compile(_wildcard_coords(entry.regex))
    expected_fn = None if t.fn == "" else t.fn
    return [m for m in mutants if wild.search(m.name) and m.op == t.op and m.fn == expected_fn]
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `python -m pytest scripts/test_check_mutant_anchors.py -k candidate_mutants -v`
Expected: PASS (3 passed).

- [ ] **Step 5: Commit**

```bash
git add scripts/check_mutant_anchors.py scripts/test_check_mutant_anchors.py
git commit -m "feat(scripts): resolve current mutant sites for an anchor (#345)"
```

---

## Task 3: `compute_rewrites` — group, map positionally, flag the unmappable

**Files:**
- Modify: `scripts/check_mutant_anchors.py` (add `compute_rewrites` after `_candidate_mutants`)
- Test: `scripts/test_check_mutant_anchors.py`

`compute_rewrites(entries, mutants)` returns `(rewrites, skip_notes, skipped_lines)`:
- `rewrites` — `Rewrite` objects for entries whose coordinates changed, sorted by `toml_line`.
- `skip_notes` — `_fmt`-formatted messages for entries `--fix` deliberately left alone (zero-candidate or structural).
- `skipped_lines` — the `toml_line`s of those skipped entries, so re-validation can defer to the better-worded note.

Only **complete-tag linecol** entries are processed; desc, tagless, and incomplete-tag entries are ignored here and surfaced later by `check()`.

- [ ] **Step 1: Write the failing tests**

```python
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
    assert "can't auto-derive" in skips[0]


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
    assert "can't auto-derive" in skips[0]


def test_compute_rewrites_ignores_desc_and_tagless():
    entries = [
        g.Entry(r"replace \| with \^ in synchsafe_decode", 3, g.Tag(count=3)),
        g.Entry(r"musefs-core/src/scan\.rs:277:30:", 5, None),
        g.Entry(r"musefs-core/src/scan\.rs:277:30:", 7, g.Tag(op="<")),
    ]
    muts = [_m("musefs-core/src/scan.rs:277:30: replace < with == in probe_file")]
    rewrites, skips, skipped = g.compute_rewrites(entries, muts)
    assert rewrites == [] and skips == [] and skipped == set()
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `python -m pytest scripts/test_check_mutant_anchors.py -k compute_rewrites -v`
Expected: FAIL with `AttributeError: ... has no attribute 'compute_rewrites'`.

- [ ] **Step 3: Implement `compute_rewrites`**

Add after `_candidate_mutants`:

```python
def compute_rewrites(
    entries: list[Entry], mutants: list[Mutant]
) -> tuple[list[Rewrite], list[str], set[int]]:
    """Plan coordinate rewrites for drifted complete-tag linecol entries.

    Returns (rewrites, skip_notes, skipped_lines). Entries are grouped by the
    identical set of sites their op/fn resolves to; siblings in a group are mapped
    to those sites positionally by source order (which fmt/edits preserve). A group
    whose anchor count differs from its site count, or any of whose sites holds a
    number of mutants other than the anchor's ``rows``, is a structural change — it
    is left untouched and reported, never guessed. An entry resolving to no site is
    deleted code, reported with deletion-oriented wording.
    """
    targets = [
        e
        for e in entries
        if classify(e.regex) == "linecol"
        and e.tag is not None
        and e.tag.op is not None
        and e.tag.fn_present
        and e.tag.rows is not None
    ]

    rewrites: list[Rewrite] = []
    skip_notes: list[str] = []
    skipped: set[int] = set()

    groups: dict[tuple, list[Entry]] = {}
    matched_by_line: dict[int, list[Mutant]] = {}
    for e in targets:
        ms = _candidate_mutants(e, mutants)
        matched_by_line[e.toml_line] = ms
        sites = tuple(sorted({m.site for m in ms}))
        if not sites:
            skip_notes.append(
                _fmt(e, "matches no live mutant — code removed? delete this exclude_re entry")
            )
            skipped.add(e.toml_line)
            continue
        groups.setdefault(sites, []).append(e)

    for sites, group in groups.items():
        group = sorted(group, key=lambda e: _entry_coords(e.regex))
        counts = Counter(m.site for m in matched_by_line[group[0].toml_line])
        mappable = len(group) == len(sites) and all(
            counts[sites[i]] == group[i].tag.rows for i in range(len(group))
        )
        if not mappable:
            for e in group:
                skip_notes.append(
                    _fmt(
                        e,
                        f"op/fn resolves to {len(sites)} live site(s), {len(group)} anchor(s) "
                        "pin it — can't auto-derive the coordinate, re-anchor manually",
                    )
                )
                skipped.add(e.toml_line)
            continue
        for e, site in zip(group, sites):
            _, line, col = site
            if (line, col) != _entry_coords(e.regex):
                rewrites.append(Rewrite(e, line, col))

    rewrites.sort(key=lambda r: r.entry.toml_line)
    return rewrites, skip_notes, skipped
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `python -m pytest scripts/test_check_mutant_anchors.py -k compute_rewrites -v`
Expected: PASS (9 passed).

- [ ] **Step 5: Commit**

```bash
git add scripts/check_mutant_anchors.py scripts/test_check_mutant_anchors.py
git commit -m "feat(scripts): positional re-anchor planner for --fix (#345)"
```

---

## Task 4: `apply_rewrites` — byte-preserving coordinate swap

**Files:**
- Modify: `scripts/check_mutant_anchors.py` (add `apply_rewrites` after `compute_rewrites`)
- Test: `scripts/test_check_mutant_anchors.py`

- [ ] **Step 1: Write the failing tests**

```python
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
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `python -m pytest scripts/test_check_mutant_anchors.py -k apply_rewrites -v`
Expected: FAIL with `AttributeError: ... has no attribute 'apply_rewrites'`.

- [ ] **Step 3: Implement `apply_rewrites`**

Add after `compute_rewrites`:

```python
def apply_rewrites(toml_text: str, rewrites: list[Rewrite]) -> str:
    """Return ``toml_text`` with each rewrite's ``:line:col:`` swapped in place.

    Only the entry's own source line is touched; the new coordinates are an
    f-string (no backslash, so a literal replacement is safe here, unlike the
    wildcard in ``_wildcard_coords``). Quote char, comma, indentation, and the
    guard-tag comment above are byte-preserved.
    """
    lines = toml_text.splitlines(keepends=True)
    for rw in rewrites:
        new_regex = _LITERAL_LINECOL.sub(f":{rw.line}:{rw.col}:", rw.entry.regex, count=1)
        idx = rw.entry.toml_line - 1
        lines[idx] = lines[idx].replace(rw.entry.regex, new_regex, 1)
    return "".join(lines)
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `python -m pytest scripts/test_check_mutant_anchors.py -k apply_rewrites -v`
Expected: PASS (3 passed).

- [ ] **Step 5: Commit**

```bash
git add scripts/check_mutant_anchors.py scripts/test_check_mutant_anchors.py
git commit -m "feat(scripts): byte-preserving anchor rewrite for --fix (#345)"
```

---

## Task 5: `run_fix` + `--fix` CLI wiring

**Files:**
- Modify: `scripts/check_mutant_anchors.py` (add `run_fix` after `apply_rewrites`; add `--fix` arg and branch in `main`)
- Test: `scripts/test_check_mutant_anchors.py`

- [ ] **Step 1: Write the failing tests**

Add `import json` near the top of `scripts/test_check_mutant_anchors.py` (below `import sys`), then add:

```python
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
    muts = _write_muts(tmp_path, ["musefs-core/src/scan.rs:300:30: replace < with == in probe_file"])
    rc = g.main(["--fix", "--toml", str(toml), "--mutants-json", str(muts)])
    assert rc == 0
    assert "scan\\.rs:300:30:" in toml.read_text()
    assert "scan\\.rs:277:30:" not in toml.read_text()
    assert "re-anchored" in capsys.readouterr().out


def test_main_fix_idempotent(tmp_path):
    toml = _write_toml(tmp_path, _FIX_TOML)
    muts = _write_muts(tmp_path, ["musefs-core/src/scan.rs:300:30: replace < with == in probe_file"])
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
    muts = _write_muts(tmp_path, ["musefs-core/src/scan.rs:300:30: replace < with == in probe_file"])
    rc = g.main(["--fix", "--toml", str(toml), "--mutants-json", str(muts)])
    assert rc == 1
    out = capsys.readouterr().out
    assert "scan\\.rs:300:30:" in toml.read_text()  # the fixable one was still applied
    assert "delete this exclude_re entry" in out


def test_main_fix_no_op_when_clean(tmp_path, capsys):
    toml = _write_toml(tmp_path, _FIX_TOML)
    muts = _write_muts(tmp_path, ["musefs-core/src/scan.rs:277:30: replace < with == in probe_file"])
    rc = g.main(["--fix", "--toml", str(toml), "--mutants-json", str(muts)])
    assert rc == 0
    assert toml.read_text() == _FIX_TOML
    assert "no coordinates needed re-anchoring" in capsys.readouterr().out


# A line:col anchor exists precisely because op+fn is NOT unique in the function
# (several same-op/fn sites, only one excluded). On a clean tree such an entry is
# valid and --fix must leave it silent — never re-derive its coordinate from the
# tag (impossible) and false-flag it. Regression for the #345 review finding.
_NONUNIQUE_TOML = (
    "exclude_re = [\n"
    '    # guard: op="+" fn="walk" rows=1\n'
    "    'musefs-format/src/wav\\.rs:58:28:',\n"
    "]\n"
)


def test_main_fix_clean_nonunique_op_fn_is_noop(tmp_path, capsys):
    toml = _write_toml(tmp_path, _NONUNIQUE_TOML)
    muts = _write_muts(
        tmp_path,
        [
            "musefs-format/src/wav.rs:58:28: replace + with - in walk",  # excluded site (valid)
            "musefs-format/src/wav.rs:60:10: replace + with - in walk",  # killable sibling
            "musefs-format/src/wav.rs:62:14: replace + with - in walk",
        ],
    )
    rc = g.main(["--fix", "--toml", str(toml), "--mutants-json", str(muts)])
    out = capsys.readouterr().out
    assert rc == 0
    assert toml.read_text() == _NONUNIQUE_TOML
    assert "auto-derive" not in out and "manual attention" not in out


def test_main_fix_drifted_nonunique_reports_manual(tmp_path, capsys):
    body = (
        "exclude_re = [\n"
        '    # guard: op="+" fn="walk" rows=1\n'
        "    'musefs-format/src/wav\\.rs:58:28:',\n"  # stale: no live mutant at 58:28
        "]\n"
    )
    toml = _write_toml(tmp_path, body)
    muts = _write_muts(
        tmp_path,
        [
            "musefs-format/src/wav.rs:70:28: replace + with - in walk",
            "musefs-format/src/wav.rs:72:10: replace + with - in walk",
        ],
    )
    rc = g.main(["--fix", "--toml", str(toml), "--mutants-json", str(muts)])
    out = capsys.readouterr().out
    assert rc == 1
    assert "auto-derive" in out
    assert toml.read_text() == body  # ambiguous → not rewritten
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `python -m pytest scripts/test_check_mutant_anchors.py -k main_fix -v`
Expected: FAIL — `--fix` is an unrecognized argument (argparse `SystemExit`), or `run_fix` is missing.

- [ ] **Step 3: Implement `run_fix` and wire `--fix`**

First add a small helper after `_fmt` (used to map `check()`/skip-note strings
back to the toml line they refer to):

```python
_FAIL_LINE_RE = re.compile(r"^\[mutants\.toml:(\d+)\]")


def _failing_lines(failures: list[str]) -> set[int]:
    """Extract the toml line numbers a list of `_fmt` failure strings refers to."""
    return {int(m.group(1)) for f in failures if (m := _FAIL_LINE_RE.match(f))}
```

Then add `run_fix` after `apply_rewrites`. The **failure gate** is the key
correctness point: a `file:line:col` anchor's `op`+`fn` resolves to *every*
same-operator site in its function (that non-uniqueness is why it is pinned by
coordinate), so `compute_rewrites` will mark most valid anchors "can't
auto-derive". Reporting those on a clean tree would fail a valid config — so we
only surface an unfixable entry when it **actually fails `check()` now**:

```python
def run_fix(
    toml_path: Path, entries: list[Entry], globs: list[str], mutants: list[Mutant]
) -> int:
    """Re-anchor drifted linecol coordinates in ``toml_path``, then re-validate.

    Exactly one rewrite pass followed by one validation pass — no fixpoint. Entries
    that could not be safely re-anchored are reported with their dedicated wording
    *only when they actually fail validation now* — an op/fn that resolves to many
    sites is normal for a line:col anchor (that non-uniqueness is why it is pinned by
    coordinate), so a currently-valid such entry is left silent rather than false-
    flagged. Re-validation handles the rest (desc drift, untagged).
    """
    failing = _failing_lines(check(entries, mutants, globs))
    rewrites, skip_notes, skipped = compute_rewrites(entries, mutants)
    skip_notes = [n for n in skip_notes if _failing_lines([n]) <= failing]
    skipped &= failing
    if rewrites:
        toml_path.write_text(apply_rewrites(toml_path.read_text(), rewrites))
        for rw in rewrites:
            old_line, old_col = _entry_coords(rw.entry.regex)
            print(
                f"re-anchored [mutants.toml:{rw.entry.toml_line}] "
                f":{old_line}:{old_col}: -> :{rw.line}:{rw.col}:"
            )
    else:
        print("no coordinates needed re-anchoring.")

    fresh_entries, fresh_globs = parse_toml_entries(toml_path.read_text())
    remaining = [
        f
        for f in check(fresh_entries, mutants, fresh_globs)
        if not any(f"[mutants.toml:{ln}]" in f for ln in skipped)
    ]
    problems = skip_notes + remaining
    if problems:
        print(f"\n{len(problems)} entry(ies) still need manual attention:\n")
        for p in problems:
            print(f"  {p}")
        return 1
    print(f"OK: {len(fresh_entries)} exclude_re entries validated against {len(mutants)} mutants.")
    return 0
```

In `main`, add the flag after the existing `--toml` argument:

```python
    ap.add_argument(
        "--fix",
        action="store_true",
        help="rewrite drifted file:line:col anchors in --toml in place, then re-validate",
    )
```

and branch right after `mutants = load_mutants(json_text)`'s `try/except` block, replacing:

```python
    failures = check(entries, mutants, globs)
```

with:

```python
    if args.fix:
        try:
            return run_fix(args.toml, entries, globs, mutants)
        except OSError as ex:
            print(f"error: failed to rewrite {args.toml}: {ex}", file=sys.stderr)
            return 1

    failures = check(entries, mutants, globs)
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `python -m pytest scripts/test_check_mutant_anchors.py -k main_fix -v`
Expected: PASS (6 passed).

- [ ] **Step 5: Run the full script test suite to confirm no regressions**

Run: `python -m pytest scripts/test_check_mutant_anchors.py -v`
Expected: PASS (all existing + new tests green).

- [ ] **Step 6: Commit**

```bash
git add scripts/check_mutant_anchors.py scripts/test_check_mutant_anchors.py
git commit -m "feat(scripts): --fix re-anchors mutants.toml and re-validates (#345)"
```

---

## Task 6: Point the pre-commit hook and CONTRIBUTING at `--fix`

**Files:**
- Modify: `.githooks/pre-commit` (the mutant-anchor guard failure message, ~line 66)
- Modify: `CONTRIBUTING.md` (the "When the guard fails" paragraph, ~line 343)

No automated test — this is a message/doc change. Verify by reading the diff and running shellcheck (the pre-commit hook runs it automatically on commit).

- [ ] **Step 1: Update the pre-commit failure message**

In `.githooks/pre-commit`, replace:

```sh
        if ! python3 scripts/check_mutant_anchors.py --mutants-json "$MUTANTS_LIST"; then
            echo "✗ mutant-anchor guard: re-anchor .cargo/mutants.toml to the current coordinates." >&2
            exit 1
        fi
```

with:

```sh
        if ! python3 scripts/check_mutant_anchors.py --mutants-json "$MUTANTS_LIST"; then
            echo "✗ mutant-anchor guard: re-anchor .cargo/mutants.toml to the current coordinates." >&2
            echo "  auto-fix shifted line:col anchors with: python3 scripts/check_mutant_anchors.py --fix" >&2
            exit 1
        fi
```

- [ ] **Step 2: Update CONTRIBUTING.md**

In `CONTRIBUTING.md`, find the sentence ending the "When the guard fails" paragraph:

```
    — re-anchor it to the current coordinates from the listing **and re-confirm
    the mutant there is still genuinely equivalent** (a reformat can change
    surrounding logic, not just line numbers). A `count`/`rows` mismatch means a
    sibling appeared or disappeared — investigate before bumping the number.
```

Append a sentence after it (still inside that paragraph):

```
    Pure `cargo fmt`/line-shift drift can be repaired automatically with
    `python3 scripts/check_mutant_anchors.py --fix`, which re-points each
    `file:line:col` anchor to its current coordinates by operator+function and
    refuses to guess when a site was added or removed; always eyeball the
    resulting diff before committing.
```

- [ ] **Step 3: Verify shellcheck passes on the hook**

Run: `shellcheck .githooks/pre-commit`
Expected: no output (exit 0).

- [ ] **Step 4: Commit**

```bash
git add .githooks/pre-commit CONTRIBUTING.md
git commit -m "docs: point mutant-anchor guard failures at --fix (#345)"
```

---

## Final verification

- [ ] **Run the full script test suite:**

Run: `python -m pytest scripts/test_check_mutant_anchors.py -v`
Expected: all tests pass.

- [ ] **Smoke-test `--fix` against the real config (no-op expected on a clean tree):**

Run:
```bash
cargo mutants --no-config --list --json > /tmp/mutants-list.json
python3 scripts/check_mutant_anchors.py --fix --mutants-json /tmp/mutants-list.json
git diff --stat .cargo/mutants.toml
```
Expected: prints `no coordinates needed re-anchoring.` then `OK: …`, and `git diff` shows **no** change to `.cargo/mutants.toml` (the committed anchors are already correct). If it does rewrite, inspect the diff — that means the checked-in anchors had drifted.

- [ ] **Confirm ruff is clean (CI `python-musefs` gate):**

Run: `ruff check scripts/check_mutant_anchors.py scripts/test_check_mutant_anchors.py && ruff format --check scripts/check_mutant_anchors.py scripts/test_check_mutant_anchors.py`
Expected: `All checks passed!` and formatting clean.
