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
  - `parse_parts` gains a `depth: usize` parameter. Each `[` that opens a section
    recurses with `depth + 1`; if `depth` would exceed `MAX_SECTION_DEPTH`, parse
    returns `Err(TemplateError::NestingTooDeep { limit: MAX_SECTION_DEPTH })`.
  - While accumulating literal text, any `char` with scalar value `< 0x20`
    (covers NUL and all C0 control bytes) returns
    `Err(TemplateError::ControlByte { byte })`. `/` remains legal in literals — it
    is the structural path separator. (`< 0x20` matches the existing
    `sanitize_into` rule exactly, so field-value and literal policy stay aligned.)
  - The `Infallible` line in `Template::parse`'s doc comment is updated.

**Why this bounds everything once:** because `parse` rejects deep nesting, a
constructed `Template` *structurally cannot* exceed `MAX_SECTION_DEPTH`. Therefore
`render_parts` and `collect_field_names`, which recurse over the same structure,
need no per-consumer depth guards — the invariant is established at the single
construction point.

**Call sites:**
- `facade.rs:255` — propagate with `?` (already in a `Result` fn).
- `facade.rs:1530`, `facade.rs:1787` (tests) — `.unwrap()` / `.expect()`.

### 2. #303 — path-field segment cap (contain)

- `sanitize_path` caps the number of **surviving** segments at
  `MAX_PATH_FIELD_SEGMENTS`. Once the cap is reached, remaining `/`-separated
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
  - A template with nesting deeper than `MAX_SECTION_DEPTH` → `NestingTooDeep`.
  - A template nested exactly at the limit parses successfully.
- **#303**
  - A `$!{field}` value with many `/`-segments renders to at most
    `MAX_PATH_FIELD_SEGMENTS` components.
  - Values at/under the cap are unaffected; `.`/`..`/empty segments still dropped;
    per-segment control sanitization unchanged.
- **Regression**
  - FUSE/`readdir` path names never contain NUL (covered by the now-enforced
    literal rejection plus existing field-value sanitization).

## Out of scope

- No change to field-value sanitization rules (`sanitize_into`) — already correct.
- No `insert_file` total-depth cap (see §2 rationale).
- No new DB-boundary limits; `MAX_TAG_VALUE_LEN` is unchanged.
