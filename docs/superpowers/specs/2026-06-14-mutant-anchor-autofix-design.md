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

1. the entry's regex with its coordinates wildcarded —
   `_LITERAL_LINECOL.sub(":\\d+:\\d+:", regex, count=1)`. This preserves any
   `replace \+ with -` repl suffix that narrows the match; for a bare
   `file:line:col:` prefix (no op/repl in the regex) the wildcard adds no
   constraint.
2. the guard tag's `op` and `fn` (`expected_fn = None if tag.fn == ""`).

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

### Rewrite mechanics

- Only entries whose computed coordinates **differ** from the current ones are
  rewritten — minimal toml diff, and a second `--fix` run is a no-op.
- Rewriting is line-precise: on `entry.toml_line`, substitute the old quoted
  regex with the new one. The new regex is the old with its coordinates swapped
  via `_LITERAL_LINECOL.sub(":{line}:{col}:", regex, count=1)`. Quote character,
  trailing comma, indentation, and the guard-tag comment above are byte-
  preserved.
- Each rewrite prints a line of the form:
  `re-anchored [mutants.toml:42] :1039:29: -> :1041:30:`.

### CLI / UX

- New `--fix` flag on `main()`. It works with the existing `--mutants-json`
  (offline, as the pre-commit hook and CI already produce the list) or invokes
  `cargo mutants --no-config --list --json` itself when that flag is absent.
- Flow under `--fix`:
  1. load entries + mutants,
  2. compute rewrites (per the rules above),
  3. write the rewritten toml,
  4. print the rewrite summary,
  5. **re-validate the rewritten toml** via the existing `check()`,
  6. exit `0` only if validation is clean.
- Remaining failures after a fix (desc-count drift, structural `linecol`
  groups) are printed exactly as `check()` reports them today, plus a per-group
  "left untouched — needs manual re-anchor" note. Exit is non-zero whenever any
  entry still fails.

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
- desc entries are never rewritten;
- partial-fix: one group remapped while another (structural) is reported;
- idempotence: a second `--fix` produces no changes;
- non-coordinate bytes (comments, guard tags, formatting) are preserved exactly.

## Out of scope

- Hook auto-re-staging of the rewritten toml.
- A `--dry-run` flag (the `--fix` rewrite summary is printed before the commit,
  so it is already eyeball-able).
- Any change to the anchor format, the `desc` model, or the validation rules.

The change is purely additive: a new `--fix` path reusing the existing parse /
match / validate code, plus its tests and a one-line hook-message update.
