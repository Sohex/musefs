# Auto-fix mode for `check_mutant_anchors.py` (`--fix`)

Resolves [#345](https://github.com/Sohex/musefs/issues/345).

## Problem

Any edit to a mutation-tested source file (`musefs-core` / `musefs-format`
`src/*.rs`) — including a plain `cargo fmt` reflow — shifts the `file:line:col`
coordinates that `.cargo/mutants.toml`'s `exclude_re` entries are anchored on.
The pre-commit/CI guard `scripts/check_mutant_anchors.py` then fails with
"line likely shifted — re-anchor to current coordinates".

Recovery is entirely manual today: regenerate the live mutant list, then for
each failing entry cross-reference its `# guard:` tag against the new mutant
set to find the current coordinates and hand-edit each anchor string. With a
dozen-plus shifted entries from a single edit this is slow and easy to get
wrong — mis-mapping two same-operator sites in one function, or silently
re-pointing an anchor onto a killable mutant.

The script already parses the guard tags and loads the mutant list to validate
them, but it can only *report* drift, not *correct* it.

## Goal

Add a `--fix` flag that rewrites stale `linecol` coordinates in
`.cargo/mutants.toml` in place, deriving each entry's current coordinates from
the live mutant set. The fix must never re-point an anchor onto a different
logical site by guessing: when an entry cannot be mapped unambiguously, it is
left untouched and reported for manual attention.

## Background: the two anchor kinds

`check_mutant_anchors.py` validates two kinds of `exclude_re` entry, classified
by `classify()` on the presence of a literal `:line:col:` in the regex:

- **`linecol`** — e.g. `'musefs-core/src/scan\.rs:1041:32:'`, optionally with a
  trailing repl description (`'...:1212:29: replace \+ with -'`). Guarded by a
  `# guard: op=… fn=… rows=…` tag. These are the only anchors that drift on a
  line shift.
- **`desc`** — e.g. `'replace < with <= in VirtualTree::dirty_min_flip_ancestors'`.
  Coordinate-free by design (deliberately chosen so `cargo fmt` cannot retire
  them). Guarded by `# guard: count=…`. **Never touched by `--fix`.**

`--fix` operates exclusively on `linecol` entries.

## Design

### Finding an entry's current coordinates

For each `linecol` entry, the candidate live mutants are those matching **both**:

1. the entry's regex with its coordinates wildcarded. Use a **function
   replacement** so the `\d` is not parsed as a replacement-template escape:
   `_LITERAL_LINECOL.sub(lambda _: r":\d+:\d+:", regex, count=1)`. A literal
   replacement string `":\\d+:\\d+:"` raises `re.PatternError: bad escape \d`
   on Python 3.7+ — do **not** use that form. The wildcard preserves any
   `replace \+ with -` repl suffix that narrows the match; for a bare
   `file:line:col:` prefix (no op/repl in the regex) it adds no constraint. The
   file portion of the regex is left verbatim, so the wildcard only ever
   loosens `line:col`, never the path.
2. the guard tag's `op` and `fn` (`expected_fn = None if tag.fn == ""`).

Entries that lack a complete guard tag (`tag is None`, or any of `op`/`fn`/
`rows` absent) cannot be op/fn-filtered, so `--fix` **skips** them entirely and
passes them through to `check()`, which reports them as "needs op=/fn=/rows=".
(A *malformed* tag — bad residue — already aborts the whole load in
`parse_guard_tag` before `--fix` runs; that is unchanged.)

The op/fn filter is what constrains bare-prefix entries — exactly mirroring how
`_check_linecol` validates today. Candidates are grouped into distinct sites
(`(file, line, col)`), sorted by `(line, col)`.

### Disambiguating same-op/fn siblings

Entries that resolve to the **identical candidate-site set** are siblings (e.g.
`run_pipeline`'s four `>=` `rows=1` anchors at `1041:32`, `1041:62`, `1051:40`,
`1051:70`). Within such a group:

- Sort sibling entries by their **stale** `(line, col)`; sort candidate sites by
  `(line, col)`.
- If `len(entries) == len(sites)`: map positionally `i → i`, verify each mapped
  site holds exactly `rows` matching mutants, then rewrite. Positional-by-source-
  order is safe because `cargo fmt` and ordinary edits preserve textual order;
  it correctly handles partial shifts within a group (only some sites moved).
- If counts differ, or a mapped site's mutant count ≠ `rows`: the coordinate
  **cannot be auto-derived** — leave the whole group untouched and (subject to
  the failure gate below) report it.

#### Why op/fn alone is not enough — the covering-set requirement

A `file:line:col` anchor exists *precisely because* its `op`+`fn` is **not
unique in the function** (that non-uniqueness is the documented reason it is
pinned by coordinate rather than description). So an entry's `op`+`fn` typically
resolves to **every** same-operator site in the function, not the one the anchor
pins — e.g. `wav.rs:58:28` (`op="+" fn="walk_chunks"`) resolves to 9 live `+`
sites; only one is excluded. The guard tag was designed to **validate a known
coordinate**, not to **derive** one, and the literal coordinate (the only
disambiguator) is exactly what wildcarding throws away.

Positional re-derivation therefore only works for a **covering set**: a group
where *every* same-`op`/`fn` site is anchored, so `#anchors == #sites` and the
bijection is unambiguous (the `run_pipeline` `>=` cluster is one). When an anchor
pins one of many sites (`#anchors < #sites`), the coordinate is unrecoverable
from the tag — `--fix` must decline, never guess.

The **zero-candidate** case (the entry's op/fn matches *no* live site) is the
deleted-code variant: report it with deletion-oriented wording — `matches no
live mutant — code removed? delete this exclude_re entry` — mirroring
`_check_desc`'s "delete a dead rule" guidance.

#### The failure gate (only report what actually broke)

Because a non-covering anchor is the *normal* shape of a line:col entry, naively
reporting every group that fails to map would fail a **clean, valid config**.
So `--fix` first computes the set of entries that **currently fail `check()`**
and only emits a skip-note / counts a group as a problem when at least one of its
entries is in that failing set. A currently-valid non-covering anchor (its
literal coordinate still matches) is left **silent** — `--fix` is not
responsible for it until it actually drifts. This makes `--fix` on an unshifted
tree a true no-op (exit 0), and surfaces a genuinely-drifted ambiguous anchor
honestly as `can't auto-derive the coordinate, re-anchor manually` rather than
mislabelling it a "structural change".

Grouping by candidate-site set (rather than by raw `op`+`fn` strings) is what
lets repl-suffixed and bare-prefix entries share one code path: two entries are
siblings precisely when they are indistinguishable except by position.

#### Documented limitation

Positional mapping assumes source *order* is preserved. A genuine code
*reordering* (moving a function or statement) could mis-map. This is no weaker
than the validator's existing model, which already cannot distinguish two
identical-`op`/`fn` sites except by position — the `# guard:` comments in
`mutants.toml` already instruct devs to re-verify these coordinates after any
reformatting of the file. `--fix` inherits, and does not widen, that contract.

Cross-file mismapping cannot occur: every `linecol` regex pins the full path
(`musefs-core/src/scan\.rs`, …) and the coordinate wildcard touches only
`line:col`, so candidates for an entry are always confined to its own file even
though `_NAME_RE`'s file group (`[^:]+`) is in principle permissive.

### Rewrite mechanics

- Only entries whose computed coordinates **differ** from the current ones are
  rewritten — minimal toml diff, and a second `--fix` run is a no-op.
- Rewriting is line-precise: on `entry.toml_line`, substitute the old quoted
  regex with the new one. The new regex is the old with its coordinates swapped
  via `_LITERAL_LINECOL.sub(f":{line}:{col}:", regex, count=1)` (an f-string
  literal — the coordinates contain no backslash, so a plain replacement string
  is safe here, unlike the wildcard call above). Quote character, trailing
  comma, indentation, and the guard-tag comment above are byte-preserved; the
  guard tag's `op`/`fn`/`rows` are never touched (they live in the comment, not
  the rewritten regex).
- Rewrites are applied and printed in ascending `entry.toml_line` order, so the
  toml diff and the summary output are deterministic regardless of the
  set-iteration order used to discover sibling groups.
- Each rewrite prints a line of the form:
  `re-anchored [mutants.toml:42] :1039:29: -> :1041:30:`.

### CLI / UX

- New `--fix` flag on `main()`. It works with the existing `--mutants-json`
  (offline, as the pre-commit hook and CI already produce the list) or invokes
  `cargo mutants --no-config --list --json` itself when that flag is absent.
  `--fix` trusts the supplied JSON to reflect the *current* source: a stale list
  (generated before the edit that caused the drift) would re-anchor to old
  coordinates and then validate clean against that same stale list. The
  pre-commit hook always regenerates the list fresh, so this is safe in
  practice; for interactive re-anchoring, prefer running `--fix` without
  `--mutants-json` so it lists live.
- Flow under `--fix` — exactly one rewrite pass followed by one validation pass,
  no fixpoint iteration:
  1. load entries + mutants,
  2. compute the **currently-failing** set: `_failing_lines(check(entries,
     mutants, globs))` (the toml line numbers `check()` flags *before* any
     rewrite),
  3. compute rewrites (per the rules above), then **filter** the unfixable
     skip-notes / skipped-line set down to entries in the failing set (the
     failure gate — a valid non-covering anchor produces a skip-note that is
     dropped here),
  4. write the rewritten toml,
  5. print the rewrite summary (ascending `toml_line`),
  6. **re-read and re-parse the rewritten toml from disk**
     (`parse_toml_entries(args.toml.read_text())`) and run `check()` on those
     fresh entries — never the pre-rewrite in-memory `entries`, whose regex and
     `toml_line` are now stale; the gated skipped-line set suppresses `check()`'s
     generic message for entries already reported with better wording,
  7. exit `0` only if no skip-notes remain and that validation is clean.
- Remaining failures after a fix (desc-count drift, genuinely-drifted ambiguous
  `linecol` anchors, zero-candidate deleted-code entries, tagless entries) are
  printed with the per-case note from the rules above (`can't auto-derive the
  coordinate, re-anchor manually`, or the deletion-oriented wording for
  zero-candidate) plus any other `check()` failures. Exit is non-zero whenever
  any entry still fails. A clean, unshifted tree produces no skip-notes and exits
  `0`.

### Hook / CI integration

The `.githooks/pre-commit` guard and the CI `mutants.yml` job stay **read-only**
— neither runs `--fix`. The pre-commit failure message gains a one-line pointer:

> run `python3 scripts/check_mutant_anchors.py --fix` to re-anchor.

No auto-re-stage, no mutation of staged state mid-commit.

## Testing

Extend `scripts/test_check_mutant_anchors.py` with synthetic mutant lists and a
temporary toml:

- single unambiguous remap (op/fn unique in file);
- multi-site sibling group remap (the `>=` case);
- repl-suffixed entry remap (`replace \+ with -`);
- non-covering group (anchor count ≠ site count) → `can't auto-derive` note,
  reported only when failing;
- **valid non-unique-op/fn anchor on a clean tree → `--fix` is a silent no-op,
  exit 0** (the failure-gate regression test for the #345 review finding);
- drifted non-unique anchor → reported `can't auto-derive`, not rewritten,
  non-zero exit;
- zero-candidate / deleted-code entry → untouched, reported with the deletion-
  oriented message, non-zero exit;
- tagless / incomplete-guard `linecol` entry → skipped by `--fix`, surfaced by
  re-validation;
- desc entries are never rewritten;
- partial-fix: one group remapped while another (deleted) is reported;
- idempotence: a second `--fix` produces no changes;
- non-coordinate bytes (comments, guard tags, formatting) are preserved exactly,
  and a remapped entry's `op=`/`fn=`/`rows=` guard tag is byte-identical after
  the rewrite.

## Out of scope

- Hook auto-re-staging of the rewritten toml.
- A `--dry-run` flag (the `--fix` rewrite summary is printed before the commit,
  so it is already eyeball-able).
- Any change to the anchor format, the `desc` model, or the validation rules.

The change is purely additive: a new `--fix` path reusing the existing parse /
match / validate code, plus its tests and a one-line hook-message update.
