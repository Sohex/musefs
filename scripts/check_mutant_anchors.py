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


_LITERAL_LINECOL = re.compile(r":[0-9]+:[0-9]+:")

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
