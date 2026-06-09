# Mutant-anchor Drift Guard Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a CI guard that validates every `.cargo/mutants.toml` `exclude_re` entry still suppresses exactly the mutant(s) it documents, and re-anchor the 12 entries that have already silently drifted to zero matches.

**Architecture:** A stdlib-only Python script (`scripts/check_mutant_anchors.py`) runs `cargo mutants --no-config --list --json` to get the full *unfiltered* mutant set, replays each `exclude_re` pattern itself, and checks it against a machine-readable `# guard:` tag in the toml comment. Line:col entries are checked by operator+function+row-count; description entries by distinct-site count. The guard runs in the `in-diff` job of `mutants.yml`; its pure-logic core is unit-tested in ci.yml's `python-musefs` job.

**Tech Stack:** Python 3 (stdlib `re`, `json`, `subprocess`, `argparse`, `dataclasses`, `fnmatch`), pytest, ruff; cargo-mutants 27.0.0; GitHub Actions.

**Spec:** `docs/superpowers/specs/2026-06-09-mutant-anchor-drift-guard-design.md`

---

## Background the engineer needs

A cargo-mutants mutant is named `<file>:<line>:<col>: replace <op> with <repl> in <fn>` (the ` in <fn>` suffix is absent for const-level mutants). cargo-mutants emits **one mutant per (site, replacement)** — a single `<` site yields three rows (`< with ==`, `< with >`, `< with <=`). `exclude_re` patterns in `.cargo/mutants.toml` are matched (unanchored) against that name string.

Two anchoring styles exist, auto-detected from the regex:
- **line:col** — the regex contains literal digits `:NNN:CC:` (e.g. `scan\.rs:277:30:`). Fragile to `cargo fmt`.
- **description** — line-agnostic (`replace | with ^ in synchsafe_decode`, or a `:\d+:\d+:` prefix). Survives reformat.

The guard gets the unfiltered set with `cargo mutants --no-config --list --json` (`--no-config` ignores `.cargo/mutants.toml` entirely, so excluded mutants reappear and can be validated).

## File structure

- **Create** `scripts/check_mutant_anchors.py` — the guard. One file: pure-logic functions (`parse_mutant`, `parse_guard_tag`, `parse_toml_entries`, `classify`, `validate_regex_subset`, `check`) plus a thin `main()` that shells out to cargo. No third-party imports.
- **Create** `scripts/test_check_mutant_anchors.py` — pytest unit tests over synthetic fixtures (no cargo invocation), mirroring `scripts/test_bump_python_version.py`.
- **Modify** `.cargo/mutants.toml` — re-anchor 12 drifted entries; add `# guard:` tags to all line:col entries and the 4 multi-site description entries.
- **Modify** `.github/workflows/mutants.yml` — add a guard step to the `in-diff` job.
- **Modify** `.github/workflows/ci.yml` — add a pytest step to the `python-musefs` job.

## Conventions

- The pre-commit hook runs `cargo fmt`, `clippy -D warnings`, the **full Rust workspace test suite**, and `ruff` over the Python paths. It does **not** run pytest. So every commit must keep Rust green (these changes don't touch Rust, so it stays green) and every Python file must be **ruff-clean** (`ruff check scripts/` and `ruff format --check scripts/`).
- Run pytest manually during TDD: `python -m pytest scripts/test_check_mutant_anchors.py -v`.
- The repo `ruff.toml` enables isort (`select = [..., "I", ...]`), so imports must be alphabetically ordered (`import` statements before `from` imports, each sorted). `ruff format` does NOT reorder imports — only `ruff check --fix` does. So each commit step runs `ruff check --fix` (sorts + autofixes) THEN `ruff format`. The pre-commit hook then runs `ruff check` (no fix) and passes because imports are already sorted. Keep the import block alphabetical as you add imports across tasks.

---

## Task 1: Script skeleton — data types + `parse_guard_tag`

**Files:**
- Create: `scripts/check_mutant_anchors.py`
- Test: `scripts/test_check_mutant_anchors.py`

- [ ] **Step 1: Write the failing test**

Create `scripts/test_check_mutant_anchors.py`:

```python
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
```

- [ ] **Step 2: Run test to verify it fails**

Run: `python -m pytest scripts/test_check_mutant_anchors.py -v`
Expected: FAIL — `ModuleNotFoundError: No module named 'check_mutant_anchors'`.

- [ ] **Step 3: Write minimal implementation**

Create `scripts/check_mutant_anchors.py`:

```python
#!/usr/bin/env python3
"""Validate that .cargo/mutants.toml exclude_re anchors still suppress exactly the
mutants they document. See docs/superpowers/specs/2026-06-09-mutant-anchor-drift-guard-design.md."""

from __future__ import annotations

import re
from dataclasses import dataclass


@dataclass
class Tag:
    op: str | None = None
    fn: str | None = None
    fn_present: bool = False
    rows: int | None = None
    count: int | None = None


_TAG_FIELD = re.compile(r'(\w+)=(?:"([^"]*)"|(\S+))')


def parse_guard_tag(text: str) -> Tag:
    tag = Tag()
    for m in _TAG_FIELD.finditer(text):
        key = m.group(1)
        val = m.group(2) if m.group(2) is not None else m.group(3)
        if key == "op":
            tag.op = val
        elif key == "fn":
            tag.fn = val
            tag.fn_present = True
        elif key == "rows":
            tag.rows = int(val)
        elif key == "count":
            tag.count = int(val)
        else:
            raise ValueError(f"unknown guard tag field: {key}")
    return tag
```

- [ ] **Step 4: Run test to verify it passes**

Run: `python -m pytest scripts/test_check_mutant_anchors.py -v`
Expected: PASS (4 passed).

- [ ] **Step 5: Lint + commit**

```bash
ruff check --fix scripts/check_mutant_anchors.py scripts/test_check_mutant_anchors.py
ruff format scripts/check_mutant_anchors.py scripts/test_check_mutant_anchors.py
git add scripts/check_mutant_anchors.py scripts/test_check_mutant_anchors.py
git commit -m "feat(mutants): guard skeleton + parse_guard_tag

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: `parse_mutant` — name → site + best-effort op/repl/fn

**Files:**
- Modify: `scripts/check_mutant_anchors.py`
- Test: `scripts/test_check_mutant_anchors.py`

The site is always extractable from the `file:line:col:` prefix; `op`/`repl`/`fn` are populated only for the binary-operator shape (else `None`). ~40% of real mutants are non-binop (FnValue, MatchArmGuard, UnaryOperator); the description check needs only the site, so `None` op/fn for them is correct.

- [ ] **Step 1: Write the failing test**

Append to `scripts/test_check_mutant_anchors.py`:

```python
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
        "musefs-core/src/tree.rs:641:30: replace match guard self.path_of(ino) == new_path with false in VirtualTree::apply_changes"
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
```

- [ ] **Step 2: Run test to verify it fails**

Run: `python -m pytest scripts/test_check_mutant_anchors.py -k parse_mutant -v`
Expected: FAIL — `AttributeError: module ... has no attribute 'parse_mutant'`.

- [ ] **Step 3: Write minimal implementation**

Add to `scripts/check_mutant_anchors.py` (after the `Tag` block):

```python
@dataclass(frozen=True)
class Mutant:
    name: str
    file: str
    line: int
    col: int
    op: str | None
    repl: str | None
    fn: str | None

    @property
    def site(self) -> tuple[str, int, int]:
        return (self.file, self.line, self.col)


_NAME_RE = re.compile(r"^(?P<file>[^:]+):(?P<line>\d+):(?P<col>\d+): (?P<body>.*)$")
_BINOP_RE = re.compile(r"^replace (?P<op>\S+) with (?P<repl>\S+)(?: in (?P<fn>.+))?$")


def parse_mutant(name: str) -> Mutant:
    m = _NAME_RE.match(name)
    if not m:
        raise ValueError(f"unparseable mutant name (no file:line:col prefix): {name!r}")
    op = repl = fn = None
    b = _BINOP_RE.match(m.group("body"))
    if b:
        op, repl, fn = b.group("op"), b.group("repl"), b.group("fn")
    return Mutant(
        name=name,
        file=m.group("file"),
        line=int(m.group("line")),
        col=int(m.group("col")),
        op=op,
        repl=repl,
        fn=fn,
    )
```

- [ ] **Step 4: Run test to verify it passes**

Run: `python -m pytest scripts/test_check_mutant_anchors.py -k parse_mutant -v`
Expected: PASS (6 passed).

- [ ] **Step 5: Lint + commit**

```bash
ruff check --fix scripts/check_mutant_anchors.py scripts/test_check_mutant_anchors.py
ruff format scripts/check_mutant_anchors.py scripts/test_check_mutant_anchors.py
git add scripts/check_mutant_anchors.py scripts/test_check_mutant_anchors.py
git commit -m "feat(mutants): parse_mutant (site always; op/fn best-effort)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: `classify` + `validate_regex_subset`

**Files:**
- Modify: `scripts/check_mutant_anchors.py`
- Test: `scripts/test_check_mutant_anchors.py`

`classify` distinguishes a literal `:277:30:` (line:col) from an escaped `:\d+:\d+:` (description). `validate_regex_subset` allowlists escape characters so a future pattern using a Rust/Python-divergent construct (`\b`, `\w`, `\s`, inline `(?...)`) fails loudly instead of silently mismatching.

- [ ] **Step 1: Write the failing test**

Append:

```python
def test_classify_linecol_vs_desc():
    assert g.classify(r"musefs-core/src/scan\.rs:277:30:") == "linecol"
    assert g.classify(r"musefs-format/src/convert\.rs:\d+:\d+: replace usize_from -> usize") == "desc"
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
    import pytest

    with pytest.raises(ValueError):
        g.validate_regex_subset(r"replace \b foo")
    with pytest.raises(ValueError):
        g.validate_regex_subset(r"replace \w+ foo")


def test_validate_regex_subset_rejects_inline_group():
    import pytest

    with pytest.raises(ValueError):
        g.validate_regex_subset(r"foo(?=bar)")
```

- [ ] **Step 2: Run test to verify it fails**

Run: `python -m pytest scripts/test_check_mutant_anchors.py -k "classify or subset" -v`
Expected: FAIL — attributes `classify` / `validate_regex_subset` missing.

- [ ] **Step 3: Write minimal implementation**

Add:

```python
_LITERAL_LINECOL = re.compile(r":[0-9]+:[0-9]+:")

# Escape chars proven equivalent between Rust `regex` and Python `re` and actually
# used by current patterns: \.  \d  \+  \|  \^  \(  \)  \*
_ALLOWED_ESCAPES = set(".d+|^()*")


def classify(regex: str) -> str:
    return "linecol" if _LITERAL_LINECOL.search(regex) else "desc"


def validate_regex_subset(regex: str) -> None:
    i = 0
    while i < len(regex):
        c = regex[i]
        if c == "\\":
            if i + 1 >= len(regex):
                raise ValueError("trailing backslash in regex")
            nxt = regex[i + 1]
            if nxt not in _ALLOWED_ESCAPES:
                raise ValueError(
                    rf"disallowed escape \{nxt} (outside the Rust/Python shared subset)"
                )
            i += 2
            continue
        if regex[i : i + 2] == "(?":
            raise ValueError("inline group/flag (?...) not in the shared subset")
        i += 1
```

- [ ] **Step 4: Run test to verify it passes**

Run: `python -m pytest scripts/test_check_mutant_anchors.py -k "classify or subset" -v`
Expected: PASS.

- [ ] **Step 5: Lint + commit**

```bash
ruff check --fix scripts/check_mutant_anchors.py scripts/test_check_mutant_anchors.py
ruff format scripts/check_mutant_anchors.py scripts/test_check_mutant_anchors.py
git add scripts/check_mutant_anchors.py scripts/test_check_mutant_anchors.py
git commit -m "feat(mutants): classify + regex subset allowlist

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: `parse_toml_entries` — raw-text parser pairing `# guard:` to elements

**Files:**
- Modify: `scripts/check_mutant_anchors.py`
- Test: `scripts/test_check_mutant_anchors.py`

Parses the toml as raw text (a TOML library would discard the `# guard:` comments). Returns the `exclude_re` entries (each paired with the nearest preceding `# guard:` tag, last wins) and the `exclude_globs` list.

- [ ] **Step 1: Write the failing test**

Append:

```python
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
```

Note: `SAMPLE_TOML` is a non-raw `"""..."""` string, so `\\` in the source is **one** backslash in the toml text — matching the real toml's literal `scan\.rs`. After parsing, `entries[0].regex` holds `musefs-core/src/scan\.rs:277:30:` (one backslash), equal to the `r"..."` assertion. The blank line inside the array exercises the blank-line skip in `parse_toml_entries`.

- [ ] **Step 2: Run test to verify it fails**

Run: `python -m pytest scripts/test_check_mutant_anchors.py -k toml_entries -v`
Expected: FAIL — `parse_toml_entries` missing.

- [ ] **Step 3: Write minimal implementation**

Add:

```python
@dataclass
class Entry:
    regex: str
    toml_line: int
    tag: Tag | None


def _unquote_toml_string(s: str) -> str:
    s = s.rstrip(",").strip()
    if len(s) >= 2 and s[0] == s[-1] and s[0] in "'\"":
        return s[1:-1]
    raise ValueError(f"malformed TOML string element: {s!r}")


def parse_toml_entries(text: str) -> tuple[list[Entry], list[str]]:
    entries: list[Entry] = []
    globs: list[str] = []
    section: str | None = None  # "re" | "globs" | None
    pending: Tag | None = None
    for lineno, raw in enumerate(text.splitlines(), start=1):
        s = raw.strip()
        if s.startswith("exclude_re"):
            section, pending = "re", None
            continue
        if s.startswith("exclude_globs"):
            section = "globs"
            continue
        if s == "]":
            section, pending = None, None
            continue
        if section is None:
            continue
        if not s:
            continue
        if s.startswith("#"):
            body = s[1:].strip()
            if body.startswith("guard:"):
                pending = parse_guard_tag(body[len("guard:") :])
            continue
        if s[:1] in "'\"":
            value = _unquote_toml_string(s)
            if section == "re":
                entries.append(Entry(regex=value, toml_line=lineno, tag=pending))
                pending = None
            else:
                globs.append(value)
    return entries, globs
```

- [ ] **Step 4: Run test to verify it passes**

Run: `python -m pytest scripts/test_check_mutant_anchors.py -k toml_entries -v`
Expected: PASS.

- [ ] **Step 5: Lint + commit**

```bash
ruff check --fix scripts/check_mutant_anchors.py scripts/test_check_mutant_anchors.py
ruff format scripts/check_mutant_anchors.py scripts/test_check_mutant_anchors.py
git add scripts/check_mutant_anchors.py scripts/test_check_mutant_anchors.py
git commit -m "feat(mutants): raw-text toml parser pairing guard tags

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 5: `check` — the core validation

**Files:**
- Modify: `scripts/check_mutant_anchors.py`
- Test: `scripts/test_check_mutant_anchors.py`

For each entry: compile (with allowlist), collect matching mutants, fail if any match lands in an `exclude_globs` file, then apply the type-specific check. Line:col: all matched share `op`+`fn` (tag `fn=""` ≡ parsed `fn=None`), one site, count == `rows`. Description: distinct-site count == `count` (default 1).

- [ ] **Step 1: Write the failing test**

Append:

```python
def _m(name):
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
    # narrowing entry (rows=1) accidentally matches 3 siblings
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
    # count=1 default, but a new same-op site joined → 2 sites
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
    # an explicit count=0 tag is always a failure, even with zero matches
    entries = [g.Entry(r"replace == with != in VirtualTree::gone", 1, g.Tag(count=0))]
    fails = g.check(entries, [], [])
    assert any("invalid" in f for f in fails)


def test_check_desc_over_nonbinop_matches():
    # description anchor whose matches are FnValue mutants (op/fn None) — site-count works
    entries = [
        g.Entry(r'replace truncate_component -> Cow<._, str> with Cow::Borrowed\(""\)', 1, None)
    ]
    muts = [
        _m('musefs-core/src/tree.rs:848:5: replace truncate_component -> Cow<\'_, str> with Cow::Borrowed("")'),
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
```

- [ ] **Step 2: Run test to verify it fails**

Run: `python -m pytest scripts/test_check_mutant_anchors.py -k check_ -v`
Expected: FAIL — `check` missing.

- [ ] **Step 3: Write minimal implementation**

Add:

```python
import fnmatch


def _glob_match(glob: str, file: str) -> bool:
    return fnmatch.fnmatch(file, glob)


def _fmt(entry: Entry, msg: str) -> str:
    return f"[mutants.toml:{entry.toml_line}] /{entry.regex}/ — {msg}"


def _check_linecol(entry: Entry, matched: list[Mutant]) -> list[str]:
    t = entry.tag
    if t is None or t.op is None or not t.fn_present or t.rows is None:
        return [_fmt(entry, "line:col entry needs `op=`, `fn=`, and `rows=` in its # guard: tag")]
    if not matched:
        return [
            _fmt(
                entry,
                f"expected {t.rows} mutant(s), found none "
                "(line likely shifted — re-anchor to current coordinates)",
            )
        ]
    fails = []
    sites = {m.site for m in matched}
    if len(sites) != 1:
        fails.append(_fmt(entry, f"expected one site, matched {len(sites)}: {sorted(sites)}"))
    if len(matched) != t.rows:
        fails.append(
            _fmt(entry, f"expected rows={t.rows}, matched {len(matched)}: {[m.name for m in matched]}")
        )
    expected_fn = None if t.fn == "" else t.fn
    bad = [m for m in matched if m.op != t.op or m.fn != expected_fn]
    if bad:
        fails.append(
            _fmt(entry, f'expected op "{t.op}" in fn "{t.fn}", coordinate holds: {[m.name for m in bad]}')
        )
    return fails


def _check_desc(entry: Entry, matched: list[Mutant]) -> list[str]:
    count = entry.tag.count if entry.tag and entry.tag.count is not None else 1
    if count < 1:
        return [_fmt(entry, f"count={count} is invalid; an entry must suppress >=1 site (delete a dead rule)")]
    sites = sorted({m.site for m in matched})
    if len(sites) != count:
        return [_fmt(entry, f"expected count={count} sites, matched {len(sites)}: {sites}")]
    return []


def check(entries: list[Entry], mutants: list[Mutant], globs: list[str]) -> list[str]:
    failures: list[str] = []
    for entry in entries:
        try:
            validate_regex_subset(entry.regex)
            rx = re.compile(entry.regex)
        except (ValueError, re.error) as ex:
            failures.append(_fmt(entry, f"regex error: {ex}"))
            continue
        matched = [m for m in mutants if rx.search(m.name)]
        glob_hits = [m for m in matched if any(_glob_match(g, m.file) for g in globs)]
        if glob_hits:
            failures.append(
                _fmt(entry, f"matches mutant(s) in an exclude_globs file: {[m.name for m in glob_hits]}")
            )
            continue
        if classify(entry.regex) == "linecol":
            failures.extend(_check_linecol(entry, matched))
        else:
            failures.extend(_check_desc(entry, matched))
    return failures
```

Move `import fnmatch` to the top import block (don't leave it inline — ruff flags `E402`/`I001`). Alphabetical stdlib order at this point is `import fnmatch` then `import re`, followed by `from dataclasses import dataclass`. (`ruff check --fix` in Step 5 enforces this automatically.)

- [ ] **Step 4: Run test to verify it passes**

Run: `python -m pytest scripts/test_check_mutant_anchors.py -v`
Expected: PASS (all tests).

- [ ] **Step 5: Lint + commit**

```bash
ruff check --fix scripts/check_mutant_anchors.py scripts/test_check_mutant_anchors.py
ruff format scripts/check_mutant_anchors.py scripts/test_check_mutant_anchors.py
git add scripts/check_mutant_anchors.py scripts/test_check_mutant_anchors.py
git commit -m "feat(mutants): check core (line:col op/fn/rows, desc site-count, globs)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 6: `load_mutants` + `main()` (CLI)

**Files:**
- Modify: `scripts/check_mutant_anchors.py`
- Test: `scripts/test_check_mutant_anchors.py`

`load_mutants` turns `--list --json` output into `Mutant`s. `main()` reads the toml, gets the mutant list (from `--mutants-json <file>` or by invoking cargo), runs `check`, prints failures, and exits non-zero on any failure (or on an empty mutant list, which is a hard error).

- [ ] **Step 1: Write the failing test**

Append:

```python
def test_load_mutants_from_json():
    payload = (
        '[{"name": "musefs-core/src/scan.rs:277:30: replace < with == in probe_file",'
        ' "file": "musefs-core/src/scan.rs"}]'
    )
    muts = g.load_mutants(payload)
    assert len(muts) == 1
    assert muts[0].site == ("musefs-core/src/scan.rs", 277, 30)


def test_load_mutants_empty_is_error():
    import pytest

    with pytest.raises(ValueError):
        g.load_mutants("[]")
```

- [ ] **Step 2: Run test to verify it fails**

Run: `python -m pytest scripts/test_check_mutant_anchors.py -k load_mutants -v`
Expected: FAIL — `load_mutants` missing.

- [ ] **Step 3: Write minimal implementation**

Add the remaining imports at the top (`import argparse`, `import json`, `import subprocess`, `import sys`, `from pathlib import Path`) and append:

```python
def load_mutants(json_text: str) -> list[Mutant]:
    data = json.loads(json_text)
    if not data:
        raise ValueError("cargo mutants --list returned no mutants (build/feature problem?)")
    return [parse_mutant(item["name"]) for item in data]


def _run_cargo_list() -> str:
    proc = subprocess.run(
        ["cargo", "mutants", "--no-config", "--list", "--json"],
        capture_output=True,
        text=True,
    )
    if proc.returncode != 0:
        raise SystemExit(f"cargo mutants --list failed:\n{proc.stderr}")
    return proc.stdout


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument(
        "--mutants-json",
        type=Path,
        help="read the mutant list from this file instead of invoking cargo "
        "(must be `cargo mutants --no-config --list --json` output)",
    )
    ap.add_argument(
        "--toml",
        type=Path,
        default=Path(".cargo/mutants.toml"),
        help="path to mutants.toml (default: .cargo/mutants.toml)",
    )
    args = ap.parse_args(argv)

    entries, globs = parse_toml_entries(args.toml.read_text())
    json_text = args.mutants_json.read_text() if args.mutants_json else _run_cargo_list()
    mutants = load_mutants(json_text)

    failures = check(entries, mutants, globs)
    if failures:
        print(f"{len(failures)} mutant-anchor failure(s):\n")
        for f in failures:
            print(f"  {f}")
        return 1
    print(f"OK: {len(entries)} exclude_re entries validated against {len(mutants)} mutants.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
```

- [ ] **Step 4: Run test to verify it passes**

Run: `python -m pytest scripts/test_check_mutant_anchors.py -v`
Expected: PASS (all tests).

- [ ] **Step 5: Lint + commit**

```bash
ruff check --fix scripts/check_mutant_anchors.py scripts/test_check_mutant_anchors.py
ruff format scripts/check_mutant_anchors.py scripts/test_check_mutant_anchors.py
git add scripts/check_mutant_anchors.py scripts/test_check_mutant_anchors.py
git commit -m "feat(mutants): load_mutants + main CLI

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 7: Migrate `.cargo/mutants.toml` — re-anchor + tag

**Files:**
- Modify: `.cargo/mutants.toml`

This is the verified data. Generate the live list once for reference:

```bash
cargo mutants --no-config --list --json > /tmp/mutants-list.json
```

- [ ] **Step 1: Re-anchor the 12 drifted line:col entries and add their tags.**

Apply these exact regex coordinate changes and insert the `# guard:` line directly above each entry (keep the existing explanatory comment block; the `# guard:` line goes as the last comment line before the `'...'`):

| Current regex (drifted) | New regex | `# guard:` line to add |
| --- | --- | --- |
| `'musefs-core/src/scan\.rs:620:46:'` | `'musefs-core/src/scan\.rs:641:46:'` | `# guard: op="*" fn="run_pipeline" rows=2` |
| `'musefs-core/src/scan\.rs:713:29:'` | `'musefs-core/src/scan\.rs:734:29:'` | `# guard: op="+=" fn="run_pipeline" rows=2` |
| `'musefs-core/src/scan\.rs:715:32:'` | `'musefs-core/src/scan\.rs:736:32:'` | `# guard: op=">=" fn="run_pipeline" rows=1` |
| `'musefs-core/src/scan\.rs:715:47:'` | `'musefs-core/src/scan\.rs:736:47:'` | `# guard: op="\|\|" fn="run_pipeline" rows=1` |
| `'musefs-core/src/scan\.rs:715:62:'` | `'musefs-core/src/scan\.rs:736:62:'` | `# guard: op=">=" fn="run_pipeline" rows=1` |
| `'musefs-core/src/scan\.rs:723:37:'` | `'musefs-core/src/scan\.rs:744:37:'` | `# guard: op="+=" fn="run_pipeline" rows=2` |
| `'musefs-core/src/scan\.rs:725:40:'` | `'musefs-core/src/scan\.rs:746:40:'` | `# guard: op=">=" fn="run_pipeline" rows=1` |
| `'musefs-core/src/scan\.rs:725:55:'` | `'musefs-core/src/scan\.rs:746:55:'` | `# guard: op="\|\|" fn="run_pipeline" rows=1` |
| `'musefs-core/src/scan\.rs:725:70:'` | `'musefs-core/src/scan\.rs:746:70:'` | `# guard: op=">=" fn="run_pipeline" rows=1` |
| `'musefs-core/src/scan\.rs:829:25:'` | `'musefs-core/src/scan\.rs:850:25:'` | `# guard: op="+=" fn="revalidate_with" rows=2` |
| `'musefs-core/src/scan\.rs:833:25:'` | `'musefs-core/src/scan\.rs:854:25:'` | `# guard: op="+=" fn="revalidate_with" rows=2` |
| `'musefs-core/src/scan\.rs:869:29: replace \+ with -'` | `'musefs-core/src/scan\.rs:890:29: replace \+ with -'` | `# guard: op="+" fn="revalidate_with" rows=1` |

The `op` values use the **mutant-name** spelling of the operator (`\|\|` in the tag is the literal `||`; the double-quote value preserves it verbatim — write `op="||"`). The new coordinates are confirmed correct: the `run_pipeline` cluster shifted +21 lines (identical columns) and `scan.rs:712:22` / `861:27` are the *killable* `*scanned += 1` / `unchanged += 1` and remain out of scope (do not anchor onto them).

> Equivalence note (required human check, per spec Step 0): each re-anchored coordinate still names the same equivalent mutant described in its existing comment — `734/744` are the two `batch_bytes += unit.weight` accumulations, `736/746` the two `len >= BATCH_FILES || batch_bytes >= cap` flush conditions, `850/854` the two `skip_failed += 1` defensive counters, `890` the `failed: scan.failed + skip_failed` sum, `641` the `sync_channel(jobs * 2)` capacity. Confirm by eye against `musefs-core/src/scan.rs` before committing; the surrounding logic is unchanged (pure reformat).

- [ ] **Step 2: Add tags to the 8 already-correct line:col entries.**

Insert the `# guard:` line above each (regex unchanged):

| Regex | `# guard:` line |
| --- | --- |
| `'musefs-core/src/scan\.rs:270:31:'` | `# guard: op="+" fn="probe_file" rows=2` |
| `'musefs-core/src/scan\.rs:277:30:'` | `# guard: op="<" fn="probe_file" rows=3` |
| `'musefs-core/src/scan\.rs:288:21:'` | `# guard: op=">" fn="probe_file" rows=3` |
| `'musefs-core/src/scan\.rs:293:17:'` | `# guard: op=">" fn="probe_file" rows=3` |
| `'musefs-core/src/ogg_index\.rs:205:41: replace \+ with \* in serve_ogg_window'` | `# guard: op="+" fn="serve_ogg_window" rows=1` |
| `'musefs-core/src/ogg_index\.rs:216:15: replace < with <= in serve_ogg_window'` | `# guard: op="<" fn="serve_ogg_window" rows=1` |
| `'musefs-core/src/ogg_index\.rs:225:15: replace < with <= in serve_ogg_window'` | `# guard: op="<" fn="serve_ogg_window" rows=1` |
| `'musefs-core/src/reader\.rs:71:60: replace / with [%*]'` | `# guard: op="/" fn="" rows=2` |

- [ ] **Step 3: Add `count=` tags to the 4 multi-site description entries.**

Insert above each (regex unchanged):

| Regex | `# guard:` line |
| --- | --- |
| `'replace \| with \^ in synchsafe_decode'` | `# guard: count=3` |
| `'musefs-core/src/facade\.rs:\d+:\d+: replace < with <= in Musefs::poll_due'` | `# guard: count=2` |
| `'replace < with <= in Musefs::poll_refresh_notify'` | `# guard: count=2` |
| `'replace \+ with \* in fixtures::wav'` | `# guard: count=2` |

All other description entries are single-site and need no tag (default `count=1`).

- [ ] **Step 4: Run the guard against the live tree to verify it now passes.**

Run:
```bash
cargo mutants --no-config --list --json > /tmp/mutants-list.json
python3 scripts/check_mutant_anchors.py --mutants-json /tmp/mutants-list.json
```
Expected: `OK: 44 exclude_re entries validated against <N> mutants.` and exit 0.

If any failure prints, fix the offending tag/coordinate per the message and re-run until exit 0.

- [ ] **Step 5: Commit**

```bash
git add .cargo/mutants.toml
git commit -m "fix(mutants): re-anchor 12 drifted exclusions + add guard tags

The scan.rs run_pipeline/revalidate_with clusters were reformatted and the
line:col exclusions silently drifted to zero matches, suppressing nothing.
Re-anchor to current coordinates and annotate every line:col and multi-site
description entry with a machine-checkable # guard: tag.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 8: Wire CI

**Files:**
- Modify: `.github/workflows/mutants.yml`
- Modify: `.github/workflows/ci.yml`

- [ ] **Step 1: Add the live guard step to the `in-diff` job in `.github/workflows/mutants.yml`.**

Insert immediately **after** the `Install cargo-mutants` step (line ~74-77) and **before** the `Build the merge-base diff` step:

```yaml
      - name: Check mutant-exclusion anchors
        run: |
          cargo mutants --no-config --list --json > mutants-list.json
          python3 scripts/check_mutant_anchors.py --mutants-json mutants-list.json
```

(It is placed before the diff build so a drift failure is reported even when there are no changed lines to mutate. `actions/setup-python` is not needed — ubuntu-latest runners ship a `python3`, and the guard is stdlib-only.)

- [ ] **Step 2: Add the unit-test step to the `python-musefs` job in `.github/workflows/ci.yml`.**

Insert immediately **after** the `Test bump script` step (`run: python -m pytest scripts/test_bump_python_version.py -v`, line ~146):

```yaml
      - name: Test mutant-anchor guard
        run: python -m pytest scripts/test_check_mutant_anchors.py -v
```

The new script is already covered by that job's existing `ruff check scripts/` / `ruff format --check scripts/` steps.

- [ ] **Step 3: Validate the workflow YAML parses.**

Run: `python3 -c "import yaml,sys; yaml.safe_load(open('.github/workflows/mutants.yml')); yaml.safe_load(open('.github/workflows/ci.yml')); print('yaml ok')"`
Expected: `yaml ok`.

- [ ] **Step 4: Commit**

```bash
git add .github/workflows/mutants.yml .github/workflows/ci.yml
git commit -m "ci(mutants): run anchor guard in in-diff job + unit tests in ci

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 9: Acceptance — full local run + negative check

**Files:** none (verification only)

- [ ] **Step 1: Full unit suite green.**

Run: `python -m pytest scripts/test_check_mutant_anchors.py -v`
Expected: all PASS.

- [ ] **Step 2: Guard exits 0 against the real tree.**

Run:
```bash
cargo mutants --no-config --list --json > /tmp/mutants-list.json
python3 scripts/check_mutant_anchors.py --mutants-json /tmp/mutants-list.json; echo "exit: $?"
```
Expected: `OK: 44 exclude_re entries validated...` and `exit: 0`.

- [ ] **Step 3: Negative check — perturb one anchor, confirm a loud failure.**

```bash
sed -i 's#scan\\.rs:277:30:#scan\\.rs:277:31:#' .cargo/mutants.toml
python3 scripts/check_mutant_anchors.py --mutants-json /tmp/mutants-list.json; echo "exit: $?"
git checkout .cargo/mutants.toml
```
Expected: a `found none (line likely shifted...)` failure for the `277:31` entry and `exit: 1`. The `git checkout` reverts the perturbation.

- [ ] **Step 4: Lint clean.**

Run: `ruff check scripts/ && ruff format --check scripts/`
Expected: no errors.

- [ ] **Step 5: Final ruff format check / no uncommitted changes.**

Run: `git status --porcelain`
Expected: empty (all work committed across Tasks 1-8).

---

## Self-review notes (for the implementer)

- The guard is intentionally NOT added to the pre-commit hook or the normal `cargo test` suite — it needs `cargo mutants --list` (a build), which is too heavy there. It lives in the `in-diff` mutants job (runs on every PR).
- `op` in a tag is the operator as it appears in the mutant **name** (`||`, `+=`, `>=`, `<`, `+`, `*`, `/`). The tag value is a literal double-quoted string; do not regex-escape it.
- A description entry's `file:\d+:\d+:` prefix can be load-bearing site-narrowing (e.g. `usize_from` matches 3 sites bare but 1 with the `convert.rs` prefix). Do not strip such prefixes.
- If a future cargo-mutants version changes the per-site replacement set, `rows=`/`count=` will fail loudly — update the tags and re-confirm equivalence; that is the gate working as intended.
