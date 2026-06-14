#!/usr/bin/env python3
"""Validate that .cargo/mutants.toml exclude_re anchors still suppress exactly the
mutants they document. See docs/superpowers/specs/2026-06-09-mutant-anchor-drift-guard-design.md."""

from __future__ import annotations

import argparse
import fnmatch
import json
import re
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path


@dataclass
class Tag:
    op: str | None = None
    fn: str | None = None
    fn_present: bool = False
    rows: int | None = None
    count: int | None = None


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
    """Parse a cargo-mutants name into its site and (best-effort) op/repl/fn.

    The site is always extracted; op/repl/fn are populated only for binary-operator
    mutants (None for FnValue/MatchArm/UnaryOperator/etc., which the site-count
    check handles fine). Raises ValueError if the name lacks a file:line:col prefix.
    """
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


_LITERAL_LINECOL = re.compile(r":[0-9]+:[0-9]+:")
_COORD_RE = re.compile(r":([0-9]+):([0-9]+):")

_ALLOWED_ESCAPES = set(".d+|^()*")


def classify(regex: str) -> str:
    return "linecol" if _LITERAL_LINECOL.search(regex) else "desc"


def validate_regex_subset(regex: str) -> None:
    """Reject patterns outside the Rust-regex/Python-re shared subset.

    Only the escapes actually used by the toml (``.d+|^()*``) are allowed, and
    inline groups ``(?...)`` are forbidden, so a future pattern relying on a
    construct whose meaning diverges between the two engines fails loudly here
    rather than silently matching a different set. Raises ValueError on violation.
    """
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


_TAG_FIELD = re.compile(r'(\w+)=(?:"([^"]*)"|(\S+))')


def parse_guard_tag(text: str) -> Tag:
    """Parse a ``# guard:`` body into a Tag, rejecting malformed residue.

    Any non-whitespace outside the recognized ``key=value`` tokens (e.g. a typo'd
    ``count = 3`` with stray spaces) raises ValueError rather than being silently
    dropped, so a broken tag can't quietly degrade to the defaults.
    """
    tag = Tag()
    cursor = 0
    for m in _TAG_FIELD.finditer(text):
        if text[cursor : m.start()].strip():
            raise ValueError(f"malformed guard tag near: {text[cursor : m.start()]!r}")
        cursor = m.end()
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
    if text[cursor:].strip():
        raise ValueError(f"malformed guard tag near: {text[cursor:]!r}")
    return tag


@dataclass
class Entry:
    regex: str
    toml_line: int
    tag: Tag | None


@dataclass
class Rewrite:
    entry: Entry
    line: int
    col: int


def _unquote_toml_string(s: str) -> str:
    s = s.rstrip(",").strip()
    if len(s) >= 2 and s[0] == s[-1] and s[0] in "'\"":
        return s[1:-1]
    raise ValueError(f"malformed TOML string element: {s!r}")


def parse_toml_entries(text: str) -> tuple[list[Entry], list[str]]:
    """Raw-text parse of mutants.toml into (exclude_re entries, exclude_globs).

    Raw text rather than a TOML library because the ``# guard:`` expectations live
    in comments. Each entry is paired with the nearest preceding ``# guard:`` tag
    (last one wins); a ``#`` only starts a comment at line start, never inside a
    quoted regex.
    """
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
        names = [m.name for m in matched]
        fails.append(_fmt(entry, f"expected rows={t.rows}, matched {len(matched)}: {names}"))
    expected_fn = None if t.fn == "" else t.fn
    bad = [m for m in matched if m.op != t.op or m.fn != expected_fn]
    if bad:
        names = [m.name for m in bad]
        fails.append(_fmt(entry, f'expected op "{t.op}" in fn "{t.fn}", coordinate holds: {names}'))
    return fails


def _check_desc(entry: Entry, matched: list[Mutant]) -> list[str]:
    count = entry.tag.count if entry.tag and entry.tag.count is not None else 1
    if count < 1:
        msg = f"count={count} is invalid; must suppress >=1 site (delete a dead rule)"
        return [_fmt(entry, msg)]
    sites = sorted({m.site for m in matched})
    if len(sites) != count:
        return [_fmt(entry, f"expected count={count} sites, matched {len(sites)}: {sites}")]
    return []


def check(entries: list[Entry], mutants: list[Mutant], globs: list[str]) -> list[str]:
    """Validate every entry against the unfiltered mutant set; return failure lines.

    Line:col anchors are checked by operator+function+row-count; description anchors
    by distinct-site count. A match landing in an exclude_globs file is also a
    failure (the pattern has drifted onto a non-gated file).
    """
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
            names = [m.name for m in glob_hits]
            failures.append(_fmt(entry, f"matches mutant(s) in an exclude_globs file: {names}"))
            continue
        if classify(entry.regex) == "linecol":
            failures.extend(_check_linecol(entry, matched))
        else:
            failures.extend(_check_desc(entry, matched))
    return failures


def load_mutants(json_text: str) -> list[Mutant]:
    data = json.loads(json_text)
    if not data:
        raise ValueError("cargo mutants --list returned no mutants (build/feature problem?)")
    return [parse_mutant(item["name"]) for item in data]


def _run_cargo_list() -> str:
    try:
        proc = subprocess.run(
            ["cargo", "mutants", "--no-config", "--list", "--json"],
            capture_output=True,
            text=True,
            timeout=600,
        )
    except subprocess.TimeoutExpired as ex:
        raise SystemExit("cargo mutants --list timed out after 600s") from ex
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

    try:
        entries, globs = parse_toml_entries(args.toml.read_text())
        json_text = args.mutants_json.read_text() if args.mutants_json else _run_cargo_list()
        mutants = load_mutants(json_text)
    except (OSError, json.JSONDecodeError, ValueError, KeyError) as ex:
        print(f"error: failed to load toml/mutants: {ex}", file=sys.stderr)
        return 1

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
