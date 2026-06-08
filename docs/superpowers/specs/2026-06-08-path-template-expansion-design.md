# Path-Template Expansion — Design

Date: 2026-06-08
Status: Approved (pending spec review)

## Context

musefs renders each track's virtual path from a beets-style template string
(`$albumartist/$album/$title`). The current engine (`musefs-core/src/template.rs`)
supports only `$field` / `${field}` substitution, with a single global
`--default-fallback` string substituted for *any* missing field. Two gaps hurt
real users:

- **No inline fallback.** A user who wants "album artist, else artist" gets
  `Unknown/Album/Title` the moment `albumartist` is missing, because the global
  fallback fires before `artist` is ever consulted.
- **No conditional text.** Multi-disc handling (`CD 2/`) or a bracketed year
  (` [1999]`) leaks empty boilerplate (`CD /`, ` []`) onto single-disc or
  undated tracks.

Separately, because the store is an EAV tag table, heavy/Turing-complete path
logic can be offloaded to external plugins (beets/Picard/Lidarr): a plugin
computes the full relative path and writes it as a custom text tag, and the user
mounts with that one tag as the template. The only blocker is that the current
sanitizer flattens `/`, so a computed multi-segment path can't expand into real
directories.

This design expands the template engine to cover inline fallbacks, conditional
sections, and a slash-preserving "path field", plus documents the computed-tag
workflow that the path field unlocks.

## Goals

- Inline fallback chains so the standalone daemon is sufficient for most
  non-plugin users.
- Conditional sections that suppress surrounding literal text when fields are
  absent, composing and nesting cleanly.
- A path field that lets a single plugin-computed tag expand into a directory
  hierarchy, without a path-traversal / empty-component footgun.
- Keep `parse` infallible and `render`'s public signature unchanged (minimal
  blast radius on the two `facade.rs` call sites).

## Non-goals

- Turing-complete templating in Rust (string functions, arithmetic, regex).
  That deliberately stays in external plugins via the computed-tag workflow.
- Removing the per-field `fallbacks` map or `--default-fallback`; both stay and
  compose with the new syntax.
- Whitespace/normalization beyond what's specified (no trimming, no case
  folding beyond the existing key lowercasing).

## Grammar

| Syntax | Meaning |
| --- | --- |
| `$field`, `${field}` | Substitute a field. **Unchanged.** |
| `${a\|b\|c}` | Fallback chain: first present of `a`, `b`, `c` wins. `\|` is special only inside `${…}` / `$!{…}`. |
| `[ … ]` | Conditional section: kept iff ≥1 field referenced inside (transitively) is present; otherwise the whole section (literals + nested sections) vanishes. Nests. |
| `$!{field}`, `$!{a\|b}` | Path field: the resolved value's `/` are kept as directory separators; each segment is sanitized, and empty / `.` / `..` segments are dropped. |
| `$[`, `$]` | Escaped literal `[` / `]`. |

## Semantics

- **Presence.** A plain field (`$field`, `${field}`) or fallback chain is
  *present* iff a candidate exists in the row **and** its value is non-empty
  (`""` ⇒ absent). No whitespace trimming — only the truly empty string is
  absent. A **path field** (`$!{…}`) is *present* iff it yields **at least one
  surviving segment** after sanitization; a value of `""`, `/`, `.`, `..`, or
  `././..` therefore counts as absent (this supersedes the non-empty-value rule
  for raw fields, since such values produce zero segments).
- **Outside a section.** A missing/empty field falls through the per-field
  `fallbacks` map and then to `--default-fallback` — today's exact behavior,
  preserved. A fallback chain `${a|b}` that resolves to nothing also lands on
  `default_fallback` here.
- **Inside a section.** A missing/empty field renders blank and does **not**
  pull `default_fallback`; emptiness is what drives suppression. A section is
  kept iff at least one referenced field — including fields inside nested
  sections — is present. Each nested section is additionally evaluated on its
  own closure, so it can be dropped while its parent is kept.
- **Path field.** Resolve the value (honoring an inner fallback chain), split on
  `/`, sanitize each segment with the existing per-character rule (control chars
  and any residual illegal chars → `_`), drop empty / `.` / `..` segments, and
  rejoin with `/`. A path field is *present* iff it yields at least one
  non-empty segment.
- Fallback chains and path fields compose anywhere (top level or inside
  sections).

### Braced-expression lexing and escaping

- Inside `${…}` and `$!{…}`, the content runs up to the first `}`; it is split
  on `|` into the candidate name list, and each candidate is taken verbatim
  (then ASCII-lowercased for lookup, matching `tags_to_fields`). A literal `|`
  or `}` cannot appear inside a braced name — this is a documented limitation,
  consistent with today's "consume until `}`" behavior.
- Unbraced `$field` keeps today's `is_field_char` charset (ASCII alphanumeric +
  `_`); `|` is not a field char, so `$a|b` is `$a` followed by the literal
  `|b`. Fallback chains therefore require braces.
- `$[` and `$]` are the only escapes for the new metacharacters. There is no
  escape for a literal `|`/`}` inside braces (see above) — outside braces both
  are ordinary literals.

### Degenerate constructs

These resolve predictably rather than erroring (parser stays infallible):

- `[]` — empty section, no field references ⇒ `any_present` is false ⇒ emits
  nothing and contributes *absent* to an enclosing section.
- `${}` / `${|}` — one or more empty candidate names, all *absent* ⇒
  `default_fallback` at top level, blank inside a section.
- `$!{}` — zero surviving segments ⇒ *absent* (same handling as above).

### Worked examples

```
$albumartist/${albumartist|artist}/$title     # albumartist, else artist
$album[ - CD $disc]                            # "Album - CD 2", or "Album" if no disc
$artist[/$[$date$] ]$album/$title              # "AC/[1999] LP/Song", date optional
$!{beets_path}                                 # "Pink Floyd/Animals/01 Pigs" -> real dirs
```

Nesting (the rule that motivated "all-empty suppresses"):

```
$artist[/[$date - ]$album]/$title
  date='', album='LP'  ->  AC/LP/Song
    outer kept (album present); inner [$date - ] dropped (date empty)
```

## Implementation

All changes are in `musefs-core/src/template.rs` plus tests and docs.

### Parser

`Part` becomes recursive:

```rust
enum Part {
    Literal(String),
    Field { names: Vec<String>, raw: bool }, // names: fallback chain; raw: path field
    Section(Vec<Part>),
}
```

`parse` stays **infallible** and becomes a small recursive/stack descent:

- `[` opens a `Section`; `]` closes the innermost open section.
- An unclosed `[` runs to EOF as a section (mirrors today's unterminated-`${`
  rule). A stray `]` with no open section is a literal `]`.
- `$[` / `$]` emit literal brackets; the existing lone-`$` rule is unchanged.
- `|` splits the name list only within `${…}` / `$!{…}`; outside braces it is a
  literal.
- `$!{…}` parses as a `Field` with `raw = true`; `$field` / `${…}` parse with
  `raw = false`.

### Render

`render` keeps its public signature
`(&self, fields, fallbacks, default_fallback, ext) -> String` — the two call
sites in `facade.rs` (`render_one`, and the test) are untouched.

Internally it recurses over parts via a helper
`render_parts(parts, …, in_section: bool) -> (String, bool)` returning
`(text, any_present)`. The `in_section` flag is what makes "default_fallback
top-level only" precise: the top-level call passes `in_section = false`; every
`Section` renders its children with `in_section = true`.

- `Literal` → push text, contributes no presence.
- `Field { names, raw }` → resolve the first present candidate (each candidate
  checked against `fields` then the per-field `fallbacks` map). If `raw`, run
  `sanitize_path_segments` and presence = ≥1 surviving segment; else
  `sanitize_into` and presence = candidate value non-empty. If no candidate is
  present: when `in_section == false`, substitute `default_fallback` (today's
  behavior); when `in_section == true`, emit nothing. A field that resolved to
  `default_fallback` still counts as *not present* for section purposes (but at
  top level there is no enclosing section to suppress).
- `Section(parts)` → render children with `in_section = true`; if their
  `any_present` is true, emit the concatenated text, else emit nothing. The
  section's own presence (for an enclosing section) is that same `any_present`,
  which is how a nested section's fields propagate transitively to the parent.

New helper `sanitize_path_segments(out, value)`: split on `/`, sanitize each
segment via the existing per-char logic, drop empty / `.` / `..` segments, join
surviving segments with `/`. Because empty segments are dropped, a path field
never emits a leading, trailing, or doubled `/`, and never a `.`/`..`
component.

### Extension append and trailing separators

The `.` + `ext` append is unchanged: it is concatenated to the final rendered
string. A path field cannot introduce a trailing `/` (empty trailing segment is
dropped), so `$!{beets_path}` → `…/01 Pigs.flac`. A trailing **literal** `/` in
the template (e.g. `$album/`) still yields `Album/.flac` exactly as today — this
is pre-existing template-author responsibility and is not changed by this work.

## Backward compatibility

`[` and `]` become metacharacters. Any existing template containing **literal
square brackets** changes meaning — e.g. `$album [$date]` previously rendered
`Album [1999]` literally and now treats `[$date]` as a conditional section
(`Album 1999` / `Album `). The escape hatch is `$[` / `$]`. The default template
(`$artist/$title`) and all current tests are unaffected. This is an accepted,
documented breaking change (approved during brainstorming).

## Testing

Unit tests in `musefs-core/tests/template.rs` (extending the existing 5, which
must stay green). Each case asserts an exact output string. Representative
acceptance cases (`default_fallback = "Unknown"`, `ext = "flac"`):

| Template | Fields | Expected |
| --- | --- | --- |
| `${albumartist\|artist}/$title` | `artist=Beck, title=Loser` | `Beck/Loser.flac` |
| `${albumartist\|artist}/$title` | `albumartist="", artist=Beck, title=Loser` | `Beck/Loser.flac` |
| `${albumartist\|artist}/$title` | `title=Loser` | `Unknown/Loser.flac` |
| `$album[ - CD $disc]` | `album=LP` | `LP.flac` |
| `$album[ - CD $disc]` | `album=LP, disc=2` | `LP - CD 2.flac` |
| `$artist[/[$date - ]$album]/$title` | `artist=AC, album=LP, title=Song` | `AC/LP/Song.flac` |
| `$artist[/[$date - ]$album]/$title` | `artist=AC, date=1999, album=LP, title=Song` | `AC/1999 - LP/Song.flac` |
| `$album[ $[$date$]]` | `album=LP, date=1999` | `LP [1999].flac` |
| `$album[ $[$date$]]` | `album=LP` | `LP.flac` |
| `$!{p}` | `p=Pink Floyd/Animals/01 Pigs` | `Pink Floyd/Animals/01 Pigs.flac` |
| `$!{p}` | `p=a//../b` | `a/b.flac` |
| `$!{p}` | `p=..` | `Unknown.flac` (absent ⇒ top-level default) |

Plus coverage for: all-empty chain suppressing a section; empty-but-kept field
renders blank; fallback chain *inside* a path field; degenerate `[]`, `${}`,
`$!{}`; `$[`/`$]` literal brackets and unchanged lone `$`; parser edge cases
(unterminated `[`, unterminated `${`, stray `]`).

Invariants (assert in tests, and extend the `fuzz/` template target if one
exists — `fuzz/` is out-of-workspace per CLAUDE.md):

- `render` never panics on any template/field combination.
- A non-raw substitution never introduces `/` into a path component.
- **Path-traversal safety:** no `$!{…}` value can produce a path component equal
  to `.` or `..`, a leading `/` (absolute path), an empty component, or a `..`
  that would escape the mount root. A fuzz/property assertion feeds adversarial
  values (`../../etc/passwd`, `/abs`, `a/../../b`, `....//`) and confirms every
  emitted component is non-empty and `∉ {".", ".."}`.

## Documentation

- README template section and CLI `--help` text: document fallback chains,
  conditional sections (with the `$[` / `$]` escape), and the path field.
- ARCHITECTURE.md external-writer contract: document the computed-tag workflow —
  a plugin (beets/Picard/Lidarr) computes the full relative path and writes it
  as a custom text tag; the user mounts with `$!{that_tag}`. Note the
  segment-sanitization rules so plugin authors know what is dropped.
