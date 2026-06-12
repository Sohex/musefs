# Template & path hardening (#275, #303, #304)

## Summary

Three adversarial-audit findings (Task 17, tracking #280) harden the template
rendering and virtual-tree insertion paths against malformed or hostile input.
They split cleanly by trust boundary:

| Issue | Concern | Trust boundary | Policy |
| ----- | ------- | -------------- | ------ |
| #275 | Control/NUL bytes in template **literal** text reach path components unsanitized | mount-time template config | **reject** at mount |
| #304 | Unbounded nested-section depth (`[[[…`) drives parse/render/reference recursion | mount-time template config | **reject** at mount |
| #303 | A 256 KiB `$!{field}` value shaped `a/a/a/…` expands into tens of thousands of path segments | hostile/external-writer SQLite rows | **contain** at render |

Config inputs are the operator's own; a malformed template should fail fast with
a clear error rather than be silently mangled. Hostile DB data arrives per-row at
render time and must not be able to fail the mount, so it is clamped.

## Background (current behaviour)

- `musefs-core/src/template.rs`
  - `Template::parse` is **infallible**: it parses any string, treating malformed
    input as graceful degradation (unterminated `${`/`[` etc. run to end of input).
  - `parse_parts` recurses once per `[`; `render_parts` and `collect_field_names`
    recurse over the same `Part::Section` structure. No depth bound.
  - Substituted **field values** are sanitized: `sanitize_into` replaces `/` and
    `char < 0x20` with `_`; `sanitize_path` (for `$!{}` path fields) splits on `/`,
    drops empty/`.`/`..` segments, sanitizes each surviving segment, and rejoins.
    No segment-count bound.
  - `Part::Literal(lit)` is emitted by `render_parts` via `out.push_str(lit)`
    **directly** — literal template text bypasses all sanitization.
- `musefs-core/src/tree.rs` `VirtualTree::insert_file` splits the rendered path on
  `/`, drops empty/`.`/`..`, truncates each component to `NAME_MAX` (255 bytes),
  but does not sanitize control bytes or bound component count.
- `Musefs::open()` (`facade.rs:255`) already returns `Result`; it is the single
  production call site that parses `config.template`.

## Design

### 1. Validation & error surface

- New `TemplateError` enum in `template.rs`:
  - `NestingTooDeep { limit: usize }`
  - `ControlByte { byte: u8 }`
- New `CoreError::InvalidTemplate(#[from] TemplateError)` in
  `musefs-core/src/error.rs`, so a malformed `--template` (or env/config-supplied
  template) fails `Musefs::open()` with a clear, actionable message.
- `Template::parse` changes signature to
  `pub fn parse(template: &str) -> Result<Template, TemplateError>`.
  - **`parse_parts` itself becomes fallible** — its return type changes from
    `Vec<Part>` to `Result<Vec<Part>, TemplateError>`. Both the recursive call in
    the `'['` arm and `Template::parse`'s top-level call propagate with `?`. The
    helpers it calls (`parse_braced_names`, `parse_unbraced_name`, `push_literal`)
    stay infallible.
  - **`depth` replaces the existing `in_section: bool` parameter** rather than
    being added alongside it: `parse_parts(chars, depth: usize)`, with
    `depth > 0` meaning "inside a section" (so every current use of `in_section`
    becomes `depth > 0`). `Template::parse` calls `parse_parts(&mut chars, 0)`.
  - Each `[` that opens a section recurses with `depth + 1`; reject with
    `Err(TemplateError::NestingTooDeep { limit: MAX_SECTION_DEPTH })` when
    `depth + 1 > MAX_SECTION_DEPTH`. Thus a template nested `MAX_SECTION_DEPTH`
    sections deep parses and one `MAX_SECTION_DEPTH + 1` deep is rejected (the
    comparison is `>`, not `>=`).
  - **Control-byte check goes in the catch-all `_ =>` ordinary-character arm of
    `parse_parts`**, before `literal.push(c)` — that is the only place a raw input
    control byte can land (the `$[`/`$]`/`$!`/`$` escape arms only push bracket and
    `$`/`!` literals). Reject the **first** offending char:
    `if (c as u32) < 0x20 { return Err(TemplateError::ControlByte { byte: c as u8 }); }`.
    The `c as u8` cast is lossless precisely because the `< 0x20` guard restricts
    `c` to scalars that fit in `u8`. `/` remains legal in literals — it is the
    structural path separator. (`< 0x20` matches the existing `sanitize_into` rule
    exactly, so field-value and literal policy stay aligned.)
  - The `Infallible` line in `impl Template`'s `parse` doc comment
    (`template.rs:28`) is updated; no other doc references infallibility.

**Why this bounds everything once:** because `parse` rejects deep nesting, a
constructed `Template` *structurally cannot* exceed `MAX_SECTION_DEPTH`. Therefore
`render_parts` and `collect_field_names`, which recurse over the same structure,
need no per-consumer depth guards — the invariant is established at the single
construction point.

**Call sites (blast radius — larger than first estimated).** `Template::parse`
is `pub` and has 34 call sites workspace-wide:
- `facade.rs:255` — the single **production** site. The concrete edit is
  `let template = Template::parse(&config.template);` →
  `let template = Template::parse(&config.template)?;` (the enclosing `open()`
  already returns `Result`, and `#[from] TemplateError` makes `?` convert into
  `CoreError::InvalidTemplate`).
- `facade.rs:1530`, `facade.rs:1787` and the one in-module `template.rs` test
  (line 315) — `.expect(...)`.
- **30 calls in `musefs-core/tests/template.rs`** — every one breaks when the
  signature becomes fallible. The pre-commit gate runs the full workspace test
  suite, so all of these must compile (and stay green) in the same commit. To keep
  the churn mechanical and readable, add a **file-scope** helper
  `fn parse(t: &str) -> Template { Template::parse(t).expect("valid template") }`
  in `tests/template.rs` alongside the existing free `fields`/`owned` helpers
  (this test file is **not** wrapped in a `mod`), and rewrite the 30 existing call
  sites to use it. The in-module `template.rs` test has a single call site — leave
  that one inline as `Template::parse(...).expect(...)` rather than adding a second
  helper. Only the new negative-path tests call `Template::parse` directly to
  assert `Err`. The implementation plan must list this rename as an explicit step,
  not leave it implicit.

### 2. #303 — path-field segment cap (contain)

- `sanitize_path` caps the number of **surviving** segments at
  `MAX_PATH_FIELD_SEGMENTS`, counted **post-filter**: only segments that pass the
  existing empty/`.`/`..` guard consume cap budget (increment the counter after the
  `continue` guards, stop once it reaches `MAX_PATH_FIELD_SEGMENTS`). So a value
  like `././a/./b` is not prematurely truncated by its dropped `.` segments, and
  the existing `path_field_neutralizes_traversal_values` test
  (`tests/template.rs:279`) stays green. Once the cap is reached, remaining
  segments are dropped. A 256 KiB `a/a/a/…` value therefore yields at most
  `MAX_PATH_FIELD_SEGMENTS` directory levels instead of tens of thousands, bounding
  the CPU/allocation/inode-map/refresh cost.
- The track stays addressable at the shallower (truncated) path — clamping, not
  dropping the row.
- **No tree-level total-depth backstop.** Literal `/` separators are now-validated
  operator config (trusted, and bounded by template length); the only depth
  amplifier across a trust boundary is the per-field DB value, which this cap
  contains directly. Adding a second cap at `insert_file` would be unused defense
  against a non-threat (YAGNI).
- **The multiplier is bounded.** A template can interleave several `$!{}` path
  fields and literal `/`s, so worst-case rendered depth is
  `(path-field count × MAX_PATH_FIELD_SEGMENTS) + (literal-slash count)`. Both
  multiplicands are properties of the now-validated template (operator config,
  bounded by template length), not of hostile DB data — so the product stays
  bounded by trusted input. That is precisely why the per-field cap suffices.

### 3. Constants

Defined in `template.rs` (these are template/path-rendering concerns, distinct
from the DB-boundary caps in `musefs-db/src/limits.rs`):

- `MAX_SECTION_DEPTH = 64`
- `MAX_PATH_FIELD_SEGMENTS = 64`

Real templates nest 2–3 sections deep and path fields are 2–4 segments, so 64
gives generous headroom for legitimate use while still bounding the adversarial
case hard.

## Testing (TDD)

Each issue's checklist drives the tests, all in `template.rs` `tests` (plus the
existing FUSE regression coverage):

- **#275**
  - Template literal with an embedded NUL → `Template::parse` returns
    `Err(ControlByte { byte: 0 })`; via `open()`, surfaces as
    `CoreError::InvalidTemplate`.
  - Template literal with another control byte (e.g. `0x01`) → rejected.
  - `$[` / `$]` escapes and ordinary `/` separators still parse successfully.
  - Field-derived hostile values remain sanitized as today (unchanged behaviour).
- **#304**
  - Boundary pair: a template nested exactly `MAX_SECTION_DEPTH` (64) sections
    deep parses successfully; one nested `MAX_SECTION_DEPTH + 1` (65) deep returns
    `NestingTooDeep` (pins the `>` comparison, not `>=`).
- **#303**
  - A `$!{field}` value with many `/`-segments renders to at most
    `MAX_PATH_FIELD_SEGMENTS` components.
  - Values at/under the cap are unaffected; `.`/`..`/empty segments still dropped;
    per-segment control sanitization unchanged.
  - Path-field **values** remain *sanitized* (control bytes → `_`, never
    rejected) — only template *literals* are rejected. So the proptest's
    `value in ".{0,64}"` strategy needs no change; the rewrite below touches only
    the `tmpl` half.
- **Existing proptest adaptation** (`render_never_panics_and_path_fields_stay_safe`,
  `tests/template.rs:261`). Its `tmpl in ".{0,64}"` strategy generates control
  bytes and `[` runs, which `Template::parse` now legitimately *rejects*. The
  chained `Template::parse(&tmpl).render(...)` therefore breaks both at compile
  time (`Result` has no `.render`) and semantically. Rewrite the property as: a
  template either fails to parse **or** renders without panicking —
  `if let Ok(t) = Template::parse(&tmpl) { let _ = t.render(...); }`. The fixed
  `Template::parse("$!{p}")` half uses the test `parse` helper (`.expect`). The
  path-field safety assertions are unchanged.
- **Regression**
  - FUSE/`readdir` path names never contain NUL (covered by the now-enforced
    literal rejection plus existing field-value sanitization).

## Out of scope

- No change to field-value sanitization rules (`sanitize_into`) — already correct.
- No `insert_file` total-depth cap (see §2 rationale).
- No new DB-boundary limits; `MAX_TAG_VALUE_LEN` is unchanged.
