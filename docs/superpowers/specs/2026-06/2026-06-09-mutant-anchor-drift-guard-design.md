# Mutant-anchor drift guard — design

Date: 2026-06-09

## Problem

`.cargo/mutants.toml` suppresses a curated set of equivalent / unkillable
mutants via `exclude_re`, each matched against the mutant name cargo-mutants
prints in `--list` (`<file>:<line>:<col>: <description>`). Two anchoring styles
are in use:

- **Description anchors** — `replace | with ^ in synchsafe_decode`. Line-agnostic
  (they carry no concrete line number), so `cargo fmt` can shift code freely
  without retiring them. These are the default and the majority.
- **Line:col anchors** — `musefs-core/src/scan\.rs:713:29:`. Used only where the
  operator+function description is *not unique within the function* and so cannot
  identify a single site (e.g. `run_pipeline` has several `+=` and `>=`, only some
  of which are equivalent). The note at `.cargo/mutants.toml` lines 104–107 records
  this constraint.

The line:col anchors are fragile. The failures are:

1. **Re-point (line:col anchors).** A reformat moves an equivalent site from line
   713 to 712. If line 713 then hosts a *different, killable* mutant of a similar
   shape, the anchor silently suppresses the killable one — a real test gap passes
   the gate unnoticed.

2. **Drift-to-nothing (line:col anchors).** More commonly, the reformat leaves
   *no* mutant at the old coordinate, so the anchor matches nothing and suppresses
   nothing. This is only "loud" if the now-unsuppressed equivalent mutant happens
   to land in a PR's changed lines (the per-PR gate is `--in-diff`); otherwise it
   sits latent — the toml claims a suppression it is no longer performing.

3. **Sibling-mask (description anchors).** Today `replace | with ^ in
   synchsafe_decode` matches a fixed set of sites. If someone later adds a
   *killable* `|` to `synchsafe_decode`, the entry silently begins excluding it
   too. The toml's repeated promise that "killable siblings stay in scope" rests
   on an assumption nothing currently enforces.

**This is not hypothetical — the tree is already drifted.** Replaying every
current `exclude_re` against the live unfiltered mutant set (cargo-mutants 27.0.0)
shows **12 line:col anchors matching zero mutants today**: `scan.rs` `620:46`,
`713:29`, `715:{32,47,62}`, `723:37`, `725:{40,55,70}`, `829:25`, `833:25`,
`869:29`. A past `scan.rs` reformat moved their sites (the `run_pipeline` cluster
is now at `712/734/736/744/746`, `revalidate_with` at `850/854/861/881`), and the
toml was never re-anchored. So the gate is presently suppressing nothing at those
12 coordinates while the toml asserts it is. Re-anchoring them is folded into this
change (see Migration).

This spec adds a guard that converts all three failures into loud, actionable CI
failures, while keeping the precise line:col anchors (no churn to `scan.rs` /
`ogg_index.rs` / `reader.rs`).

## Goals

- Detect a line:col anchor that no longer suppresses exactly its documented
  mutant(s): re-pointed (different operator/function now at the coordinate),
  drifted-to-nothing, or over/under-suppressing (wrong number of replacement
  variants), with a message naming the entry and what it now hits.
- Detect a description anchor whose set of suppressed *sites* differs from its
  declared expectation (sibling-mask, or a dead entry after a rename).
- Single source of truth: the expectation lives next to the entry it governs.
- Run in CI on every PR that could shift the anchors, reusing the existing
  cargo-mutants install in the `in-diff` job. Also runnable locally on demand.
- Pure-logic core that is unit-testable without invoking cargo.

## Non-goals

- Auto-repairing or auto-updating anchors. The guard reports; a human fixes.
- Validating that each `exclude_globs` pattern still matches a file (those govern
  whole-crate scope, are not line-fragile, and a stale glob harmlessly over-excludes
  an already-out-of-scope crate). The guard *reads* `exclude_globs` only to reject an
  `exclude_re` match that lands inside a glob-excluded file — it does not check the
  globs themselves.
- Replacing the existing in-diff / full / canary mutation legs. This is an
  additional check, not a change to how mutants are run.
- Changing the anchoring strategy itself (no helper-extraction refactors). The
  guard makes the current strategy safe; it does not migrate away from it.

## The mutant grouping model

cargo-mutants emits **one mutant per (source site, replacement)**. A single
binary-operator *site* therefore yields several mutant rows — e.g.
`scan.rs:277:30:` (a `<`) yields three: `replace < with ==`, `replace < with >`,
`replace < with <=`. This one-site-many-rows fact is central; an earlier draft of
this spec wrongly assumed one site = one mutant.

The guard parses each mutant `name` (`<file>:<line>:<col>: replace <op> with
<repl> in <fn>`, or — for const-level mutants — the same with no ` in <fn>`
suffix) into four fields: **site** `(file,line,col)`, **op** (the source
operator), **repl** (the replacement), and **fn** (function, possibly absent).

Two distinct kinds of line:col entry exist in the current toml, and the model must
serve both:

- **Bare** — regex is `<file>:<line>:<col>:` with no replacement, suppressing
  *every* replacement at the site (e.g. `277:30:` excludes all three `<` variants;
  all are equivalent). Most line:col entries.
- **Narrowing** — regex embeds a specific replacement, suppressing a *subset* and
  deliberately leaving its same-site siblings killable (e.g. `216:15: replace <
  with <=` excludes only `<=`; the `==` and `>` mutants at `216:15` stay in
  scope). Verified narrowing entries: `ogg_index.rs` `205:41` (`+ with *`, leaving
  `+ with -`), `216:15` and `225:15` (`< with <=`), and `scan.rs:869:29`
  (`+ with -`, leaving `+ with *`).

The over-suppress failure mode is unique to this: a narrowing entry that
accidentally broadens would silently swallow a killable sibling. So the line:col
check cannot be op+fn alone — it must also pin the matched row count.

## The two checks, by anchor type

The guard auto-detects an entry's type from its regex (literal `:<digits>:<digits>:`
⇒ line:col; otherwise description) and applies the check that closes that entry's
hole:

| Anchor type | What can go wrong | Check applied |
| ----------- | ----------------- | ------------- |
| **Line:col** | re-point (coord now holds a different op/fn); drift-to-nothing (coord empty); over/under-suppress (wrong replacement set) | every matched row shares the tag's `op` + `fn`; matched rows occupy exactly one site; matched row count equals the tag's `rows` (≥1) |
| **Description** | sibling-mask (a new killable site joins the match set); dead entry (rename → 0 matches) | the number of distinct *sites* matched equals the tag's `count` (default 1). op+fn are already pinned by the regex text. |

`op`+`fn` catches re-point; `rows` catches drift-to-nothing (rows would be 0) and
over/under-suppress; *sites*-counting (not row-counting) is what makes the
description check sensitive to a new killable sibling — a new same-op site moves
the site count, whereas a row count can be confounded by per-site replacement
multiplicity.

Residual limitation (documented, accepted): the line:col check cannot distinguish
two mutants that share op, fn, and replacement at the *same* coordinate, so a
reformat that coincidentally lands a different-but-identically-shaped killable
mutant at the exact `file:line:col` would pass. This is a measure-zero edge far
smaller than today's bare-coordinate exposure; closing it would require pinning
surrounding source context, which is itself fmt-fragile.

## The structured guard tag

Each `exclude_re` array element is preceded by a comment block (already the
convention). The guard reads one machine-readable line from that block:

```
# guard: op="<" fn="probe_file" rows=3          (line:col, bare)
# guard: op="<" fn="serve_ogg_window" rows=1    (line:col, narrowing)
# guard: op="/" fn="" rows=2                     (line:col, const-level: no fn)
# guard: count=3                                 (description, multi-site)
```

Grammar: `# guard:` prefix (after optional leading whitespace), then
space-separated `key=value` fields; string values are double-quoted, integers
bare.

- `op=` — the source operator the matched rows must all share (line:col only).
- `fn=` — the function the matched rows must all be in; empty string `""` for
  const-level mutants whose name has no ` in <fn>` suffix (line:col only). The
  guard normalizes the tag's `fn=""` to the parsed `fn=None`, so the two compare
  equal.
- `rows=` — the exact number of mutant rows the regex must match (line:col only).
- `count=` — the number of distinct sites the regex must match (description only;
  default 1).

Resolution rules:

- **Line:col anchor:** `op`, `fn`, and `rows` are **all required** (guard errors
  if any is missing). The guard asserts: the regex matches `rows` mutant rows; all
  of them parse to operator `op` and function `fn`; and they occupy exactly one
  `(file,line,col)` site.
- **Description anchor:** `count=` is optional, **default 1**. The guard asserts
  the regex matches mutants spanning exactly `count` distinct sites.
- An entry with **no** `# guard:` line is treated as a description anchor with
  `count=1`. So a bare description anchor that already matches a single site needs
  no tag; only multi-site description anchors and all line:col anchors must be
  annotated.

`op`/`fn` for line:col anchors live in the tag (not parsed back out of the regex)
to keep one authoritative source: a bare regex carries no description at all, and
for a narrowing regex the tag is checked *against* the rows the regex selects, so
a typo that desynchronizes the two surfaces as a guard failure rather than passing
silently.

## How the guard gets the unfiltered mutant set

`cargo mutants --list` honours the config's `exclude_re`, so excluded mutants are
absent from its output — which would make it impossible to confirm an anchor still
hits a real mutant. The guard therefore lists with the config disabled:

```
cargo mutants --no-config --list --json
```

`--no-config` (cargo-mutants 27.0.0) ignores `.cargo/mutants.toml` entirely, so the
output is the complete pre-exclusion mutant set across the workspace. The guard
then *replays* each toml `exclude_re` pattern itself (in Python) against the
`name` field of every listed mutant. This is the inversion that makes the checks
meaningful: cargo applies the exclusions and hides the result; the guard sees
everything and verifies the exclusions land where documented.

`--no-config` also drops `exclude_globs`, so the list includes the 191 mutants
from `musefs-latencyfs` / `musefs-fuse` / `musefs-cli` / `musefs` / `metrics.rs`.
Today **zero** `exclude_re` patterns match any of them (verified), so they are
inert. But the guard's job is to catch drift, so it does not merely ignore them:
the guard reads `exclude_globs` from the toml and **fails** if any `exclude_re`
match lands in a glob-excluded file. Such a match means an `exclude_re` is
"suppressing" a mutant that is not a real gate participant — a sign the pattern has
drifted onto the wrong file, which would otherwise be a silent hole.

**Feature set.** The guard runs `--no-config --list` with the *default* feature
set — the same set the per-PR `in-diff` gate builds with (no `--features metrics`,
no `--features fuzzing`). This is deliberate and load-bearing: several entries'
equivalence is premised on that exact config (`metrics.rs` is glob-excluded
precisely because its counters aren't compiled there; the `fuzz_check.rs::fixtures`
mutants are present because cargo-mutants builds under `cfg(test)`). Any future
change to the gate's feature set must be mirrored here, and any entry whose
equivalence depends on a feature flag must be reasoned about in this config.

### Regex-replay fidelity

cargo-mutants matches `exclude_re` with the Rust `regex` crate; the guard replays
with Python `re`. All 44 current patterns were checked to compile under Python `re`
and use only a small shared subset — `\.`, `\d+`, literal alternation `(==|>|<=)`,
character classes `[%*]`, escaped operators (`\+`, `\|`, `\^`). The guard compiles
each pattern with `re.search` (cargo-mutants uses unanchored search). A pattern
that fails to compile under Python `re` is a guard error naming the entry.

Scope and honest limits: the compile-check catches Python-incompatible patterns
(and Rust `regex` lacks backreferences/look-around, so Python-only features cannot
appear in a *working* toml). It does **not** detect *semantic divergence within a
pattern valid in both engines* — e.g. `\b` word-boundary or `\d` Unicode-vs-ASCII
differences. To make that detectable rather than merely "documented," the guard
also validates each pattern against an **allowlist of regex tokens** (literals plus
the subset above) and fails on any token outside it. This keeps future entries
provably within the shared subset instead of relying on a prose constraint.

## Components

### `scripts/check_mutant_anchors.py`

The guard. Single-file, stdlib-only (no third-party imports; `tomllib` is 3.11+
stdlib but is **not** used — see below). Structure:

- `parse_toml_entries(text) -> list[Entry]` — reads `.cargo/mutants.toml` as raw
  text (not via a TOML parser, because the `# guard:` expectations live in
  comments, which a TOML parser discards). Walks the `exclude_re = [ … ]` array,
  pairing each string element with the nearest preceding `# guard:` line in its
  comment block. Returns `Entry { regex, line_in_toml, tag }`.
- `classify(entry) -> "linecol" | "desc"` — detects a literal `:[0-9]+:[0-9]+:`
  in the regex.
- `parse_mutant(name) -> Mutant` — parses a mutant `name` into
  `{site:(file,line,col), op, repl, fn}`. **The `site` is always extracted** from
  the `<file>:<line>:<col>:` prefix that *every* mutant name carries, regardless of
  genre. `op`/`repl`/`fn` are **best-effort**: they are populated only for the
  binary-operator shape `<site>: replace <op> with <repl> in <fn>` (and `fn=None`
  for the const-level binop variant with no ` in <fn>` suffix, e.g.
  `reader.rs:71:60: replace / with %`); for every other genre — FnValue
  (`replace usize_from -> usize with 0`), MatchArm/MatchArmGuard
  (`replace match guard … with false in …`), UnaryOperator (`delete ! …`),
  StructField — `op`/`repl`/`fn` are `None`. This matters because ~40% of the
  unfiltered corpus is non-binop, and 8 description anchors deliberately target
  those names; the description check needs only `site`, so leaving op/fn `None` for
  them is correct, not a parse failure.
- `load_mutants(json_text) -> list[Mutant]` — extracts every mutant `name` from
  `cargo mutants --no-config --list --json` and runs it through `parse_mutant`. It
  never errors on a name shape (site extraction is total); an unparseable
  *prefix* (no `file:line:col:`) is the only hard error.
- `check(entries, mutants, exclude_globs) -> list[Failure]` — the unit-tested
  core, pure, no I/O. For each entry: compile the regex (allowlist + compile
  check); collect matching mutants; fail if any match lands in an `exclude_globs`
  file; then apply the type-specific check — line:col: matched count == `rows`,
  all share `op`+`fn` (line:col entries only ever match binop-shaped names, so
  op/fn are always present there), single site; description: distinct-site count
  == `count` (uses `site` only — never op/fn — so non-binop genres are handled
  uniformly).
- `main()` — shells out to `cargo mutants --no-config --list --json`, reads
  `.cargo/mutants.toml`, runs `check`, prints each failure with the entry's toml
  line number, the regex, expected vs actual, and the offending mutant name(s).
  Exits non-zero on any failure.

`main()` accepts `--mutants-json <file>` to read a pre-generated list instead of
invoking cargo (used by the CI step to avoid a redundant second `--list`, and
handy locally). Default behaviour invokes cargo itself.

Raw-text parsing rationale: the expectations are in TOML comments, which any TOML
library drops. Reading the file as text and pairing comment→element is the only way
to keep the expectation co-located with its entry (the chosen single-source-of-
truth design). The parser is small and unit-tested.

### `scripts/test_check_mutant_anchors.py`

pytest unit tests for the pure core, mirroring
`scripts/test_bump_python_version.py` (same directory, same `sys.path` insert
idiom, ruff-clean). Covers, against synthetic toml + JSON fixtures:

- `parse_mutant`: binary-operator name; const-level binop name (no `in <fn>` →
  `fn=None`); **non-binop names** — FnValue (`replace usize_from -> usize with 0`),
  MatchArmGuard (`replace match guard … with false in …`), UnaryOperator
  (`delete ! …`) — each yields a valid `site` with `op`/`repl`/`fn` all `None`; a
  name with no `file:line:col:` prefix is the one hard error.
- description anchor over **non-binop** matches (e.g. the `truncate_component ->
  Cow` or `force_apply_failure_for_test with ()` entries): site-counting works with
  op/fn `None`.
- description anchor: site count matches / too high (sibling-mask — a new same-op
  site joins) / zero (dead entry after a rename);
- multi-site description anchor (`count=3`, the `synchsafe_decode` shape: one repl
  across three `|` sites);
- line:col **bare**: exact (`rows=3`, all `<`/`probe_file`, one site); re-point
  (coord now holds a different op → op mismatch) and (different fn → fn mismatch);
  drift-to-nothing (`rows` expected ≥1, matched 0); over-suppress (regex broadened,
  matched > `rows`);
- line:col **narrowing**: `rows=1` exact; broadened regex swallows a killable
  sibling → matched 2 ≠ `rows=1` → caught;
- line:col const-level entry with `fn=""`;
- line:col entry missing a required field (`op`/`fn`/`rows`) → guard error;
- a pattern that doesn't compile, or uses a token outside the allowlist → reported;
- an `exclude_re` whose match lands in an `exclude_globs` file → reported;
- empty unfiltered mutant list → hard error (not a flood of count=0 failures);
- entry with no `# guard:` line defaults to description/count=1;
- the comment→element pairing parser (blank lines, multi-line comment blocks,
  multiple `# guard:` lines in one block — last wins, inline `#` inside a regex
  string must not be read as a tag).

No cargo invocation in the unit tests — they exercise `check`, `parse_toml_entries`,
`classify`, `parse_mutant`, and `load_mutants` directly on fixtures, so they run in
milliseconds in the normal Python test path.

## CI integration

### Live guard — `in-diff` job in `.github/workflows/mutants.yml`

The drift the guard catches is introduced by source edits/reformats, which land in
the same PRs the `in-diff` job already runs on, and that job already installs
cargo-mutants and has the toolchain warm. Add a step there, after
`Install cargo-mutants` and independent of `mutants.diff` (the guard validates the
whole anchor set, not just changed lines):

```yaml
- name: Check mutant-exclusion anchors
  run: |
    cargo mutants --no-config --list --json > mutants-list.json
    python3 scripts/check_mutant_anchors.py --mutants-json mutants-list.json
```

Placed before the diff build so a drift failure is reported even when there are no
in-scope changed lines to mutate. It runs on every PR (`in-diff` is
`if: github.event_name == 'pull_request'`), so a reformatting PR is caught in the
same PR that introduces the drift.

### Unit tests — `python-musefs` job in `.github/workflows/ci.yml`

The per-PR home for `scripts/` Python tests is the `python-musefs` job in ci.yml,
which already runs `python -m pytest scripts/test_bump_python_version.py -v` (the
"Test bump script" step) on every PR and lints `scripts/` with ruff.
(`release-python.yml` runs the same bump test but only fires on `py-v*` tags, so it
is not a per-PR gate.) Add a sibling step:

```yaml
- name: Test mutant-anchor guard
  run: python -m pytest scripts/test_check_mutant_anchors.py -v
```

The new script is covered by that job's existing `ruff check scripts/` /
`ruff format --check scripts/` steps and must pass both. Mirroring the step into
release-python.yml is optional and not required for the gate.

## Migration: re-anchor and annotate existing entries

The migration is a substantial part of this change, not a footnote — and its first
step fixes a pre-existing bug. The verified current state (from replaying every
pattern against the live unfiltered set) drives it:

**Step 0 — re-anchor the 12 drifted line:col entries (do this first).** These
match zero mutants today and must be re-pointed to current coordinates *and* their
equivalence re-judged, because the surrounding `scan.rs` code was reformatted (a
human must confirm each still names a genuinely-equivalent mutant, not merely
update digits):

| Drifted entry | Now at | Notes |
| ------------- | ------ | ----- |
| `scan.rs:620:46` (sync_channel `jobs * 2`) | re-derive | bare |
| `713:29`, `715:{32,47,62}`, `723:37`, `725:{40,55,70}` (`run_pipeline` flush cadence) | `712/734/736/744/746` cluster | bare; `+=` and `>=`/`||` |
| `829:25`, `833:25` (`revalidate_with` `skip_failed += 1`) | `850/854/861/881` cluster | bare; `+=` |
| `869:29` (`revalidate_with` `failed = … + skip_failed`) | re-derive | **narrowing** (`+ with -` only; `+ with *` stays killable) |

**Step 1 — add `op`/`fn`/`rows` to every line:col entry.** Derived from the live
data; the live-and-correct ones are: `scan.rs:270:31` (`op=+ fn=probe_file
rows=2`), `277:30` / `288:21` / `293:17` (`probe_file`, `rows=3` each — confirm op
per entry; `293:17` is a `>` diagnostic, not `<`), `ogg_index.rs:205:41`
(`op=+ fn=serve_ogg_window rows=1`, narrowing), `216:15` / `225:15`
(`op=< fn=serve_ogg_window rows=1`, narrowing), `reader.rs:71:60`
(`op=/ fn="" rows=2`, const-level). The re-anchored Step-0 entries get theirs from
the new coordinates.

**Step 2 — add `count=` to the four multi-site description entries.** Only these
span more than one site (verified): `synchsafe_decode` `| with ^` (**count=3**, one
repl across three `|` sites), `poll_due` `< with <=` (**count=2**),
`poll_refresh_notify` `< with <=` (**count=2**), `fixtures::wav` `+ with *`
(**count=2**). Note `crc_shift_zeros < (==|>|<=)` is **count=1** (one site, three
repl rows) — site-counting, not row-counting, is why it needs no tag despite three
matches.

A subtlety to preserve: some description anchors carry a `<file>:\d+:\d+:` prefix
that is doing *load-bearing site-narrowing*, not decoration. `usize_from -> usize`
matches **three** sites bare (`musefs-db`, `musefs-format`, `musefs-latencyfs`);
the real entry is `musefs-format/src/convert\.rs:\d+:\d+: replace usize_from ->
usize`, whose file prefix narrows it to the one intended site (count=1). Dropping
such a prefix would change the site count and the guard would (correctly) fail —
do not "simplify" these to the bare description form.

**Step 3 — leave the remaining single-site description anchors untagged**
(they default to `count=1`).

The pass is complete when `scripts/check_mutant_anchors.py` exits 0 against the
tree — the acceptance test for the whole change. Expect the guard's *first* run to
fail loudly on the 12 Step-0 drifts; that failure is the feature demonstrating its
value, and is the strongest argument for the change.

## Failure output

Each failure is one block naming: the entry's `.cargo/mutants.toml` line number,
the regex string, the check that failed, and the specifics:

- Line:col re-point — `expected op "<op>" in fn "<fn>", coordinate now holds
  "<actual mutant name(s)>"`.
- Line:col drift-to-nothing — `expected <rows> mutant(s) at <file>:<line>:<col>,
  found none (line likely shifted — re-anchor to the current coordinates)`.
- Line:col rows mismatch — `expected rows=<n>, matched <m>: [<names…>]` (an over-
  suppress reads as m>n and lists the swallowed siblings).
- Description site-count mismatch — `expected count=<n> sites, matched <m>:
  [<site → name(s)>]` (lists matched sites so a sibling-mask is obvious).
- Missing required `op`/`fn`/`rows` on a line:col entry, an uncompilable or
  non-allowlisted regex, or a match in an `exclude_globs` file — names the entry.

The exit is non-zero if any block is emitted, failing the `in-diff` job.

## Edge cases

- **`--no-config` listing is empty / cargo errors** — guard exits non-zero with
  the cargo stderr; an empty list is treated as a hard error (it would otherwise
  make every count=0 and report spurious dead-entry failures, masking the real
  cargo problem).
- **A description anchor legitimately wants count=0** — not supported; count=0 is
  always a failure (a rule that excludes nothing is dead and should be deleted, not
  kept). If a future case needs it, it is added explicitly then.
- **Inline `#` inside a regex string** (e.g. a pattern containing `#`) — the parser
  only treats a `#` as a comment when it begins a line (after whitespace), never
  inside a quoted array element. Covered by a unit test.
- **Multiple `# guard:` lines in one comment block** — the last one wins; a unit
  test pins this so a stale tag left above a fresh one can't be silently honoured.

## Testing strategy

- Unit: `scripts/test_check_mutant_anchors.py` over synthetic fixtures (above) —
  the correctness proof for the parser and checker, cargo-free, runs in the Python
  test leg.
- Integration / acceptance: `scripts/check_mutant_anchors.py` exits 0 against the
  real `.cargo/mutants.toml` + live `cargo mutants --no-config --list --json` after
  the migration pass. This is run once during implementation and continuously by
  the new CI step.
- Negative integration (manual, documented in the PR): temporarily perturb one
  line:col anchor by ±1 line and confirm the guard fails loudly; revert.

## Risks

- **cargo-mutants `--list --json` shape changes across versions.** `load_mutants`
  depends only on the top-level `name` field, the most stable part of the schema;
  the known-good version (27.0.0) is already pinned-by-documentation in
  `scripts/mutants.sh`. A schema break surfaces as a guard failure, not a silent
  pass.
- **Rust-vs-Python regex divergence on a future exotic pattern.** Mitigated by the
  token allowlist (rejects anything outside the proven shared subset) plus the
  compile-check; the current 44 entries are all within the subset. Semantic
  divergence is detectable, not merely documented.
- **`--no-config --list` cost.** Verified ~2s, parse-time only (2867 mutants
  enumerated, no tests run) — far cheaper than the mutation run the same job
  already performs. On a cold `rust-cache` it also pays a baseline build; the cost
  note assumes the warm cache the `in-diff` job normally has.
- **Replacement-table change on a cargo-mutants upgrade.** `rows=`/`count=` pin the
  current per-site mutant multiplicity; a future cargo-mutants that adds or removes
  a replacement variant would shift those counts and fail the guard loudly. That is
  correct-to-investigate (the version is documentation-pinned at 27.0.0), not a
  silent pass — but it is a maintenance cost to note.
- **`--mutants-json` feature/flag skew.** The file-input mode trusts the caller to
  have generated the JSON with the exact documented command
  (`cargo mutants --no-config --list --json`, default features). The guard cannot
  detect a file produced with different flags; the CI step and docs use the canonical
  command, and `main()`'s default (self-invoking) path avoids the risk locally.
