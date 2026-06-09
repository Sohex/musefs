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
