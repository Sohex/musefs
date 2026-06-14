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
- If counts differ, or a mapped site's mutant count ≠ `rows`: this is a
  **structural change, not a shift** (a site was added/removed, or the operator
  count changed). Leave the whole group untouched and report it.

The **zero-candidate** case (the entry's op/fn now matches *no* live site) is a
distinct sub-case of "counts differ": the code was deleted, not moved, so a
re-anchor is impossible. Report it with deletion-oriented wording — `matches no
live mutant — code removed? delete this exclude_re entry` — rather than the
generic "re-anchor" note, mirroring `_check_desc`'s "delete a dead rule"
guidance.

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
  2. compute rewrites (per the rules above),
  3. write the rewritten toml,
  4. print the rewrite summary (ascending `toml_line`),
  5. **re-read and re-parse the rewritten toml from disk**
     (`parse_toml_entries(args.toml.read_text())`) and run `check()` on those
     fresh entries — never the pre-rewrite in-memory `entries`, whose regex and
     `toml_line` are now stale,
  6. exit `0` only if that validation is clean.
- Remaining failures after a fix (desc-count drift, structural `linecol`
  groups, zero-candidate deleted-code entries, tagless entries) are printed
  exactly as `check()` reports them, plus the per-case note from the rules above
  ("left untouched — needs manual re-anchor", or the deletion-oriented wording
  for zero-candidate). Exit is non-zero whenever any entry still fails.

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
- structural-change group (site count changed) → untouched, reported, non-zero
  exit;
- zero-candidate / deleted-code entry → untouched, reported with the deletion-
  oriented message, non-zero exit;
- tagless / incomplete-guard `linecol` entry → skipped by `--fix`, surfaced by
  re-validation;
- desc entries are never rewritten;
- partial-fix: one group remapped while another (structural) is reported;
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
