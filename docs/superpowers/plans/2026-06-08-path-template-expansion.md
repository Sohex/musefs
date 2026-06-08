# Expanded Path-Template Engine — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Extend musefs's path-template engine with inline fallback chains (`${a|b}`), foobar2000-style conditional sections (`[...]`), a slash-preserving path field (`$!{field}`) for plugin-computed paths, and `$[`/`$]` bracket escapes — keeping `parse` infallible and `render`'s public signature unchanged.

**Architecture:** All engine logic lives in `musefs-core/src/template.rs`. The `Part` enum becomes recursive (`Literal`, `Field { names, raw }`, `Section`). `parse` becomes a recursive descent over a `Peekable<Chars>`; `render` delegates to a recursive `render_parts(..., in_section)` that returns `(text, any_present)`, applying `default_fallback` only at the top level and suppressing a section when none of its referenced fields are present. A new `sanitize_path` splits raw values on `/`, sanitizes each segment, and drops empty/`.`/`..` segments. The rendered string already feeds `VirtualTree::build` (`tree.rs`), which splits on `/` into the inode tree — so multi-segment path-field output becomes real directories with no tree-layer change. The single call site (`render_one` in `facade.rs`) and `MountConfig` are untouched.

**Tech Stack:** Rust (workspace edition 2024). Tests in `musefs-core/tests/template.rs` (integration tests against the public `Template`); property test via `proptest` (already a `musefs-core` dev-dependency). Docs in `README.md`, `ARCHITECTURE.md`, and the CLI doc-comment in `musefs-cli/src/lib.rs`.

**Reference:** Design spec at `docs/superpowers/specs/2026-06-08-path-template-expansion-design.md`. Work is on branch `path-template-expansion`.

**Pre-commit note:** The pre-commit hook runs `cargo fmt --check`, `cargo clippy --all-targets -D warnings`, and the **full workspace test suite**, then ruff. Every commit must be green. Run `cargo fmt` before each commit. The `fuzz/` crate is out-of-workspace and unaffected (no template fuzz target exists).

---

## Task 1: Rewrite the template engine

**Files:**
- Modify: `musefs-core/src/template.rs` (full rewrite of the `Part` enum, `parse`, `render`, and helpers)
- Test: `musefs-core/tests/template.rs` (keep the existing 5 tests; add the new cases below)

The existing tests use these helpers already present at the top of `musefs-core/tests/template.rs`:

```rust
fn fields<'a>(pairs: &[(&str, &'a str)]) -> BTreeMap<String, &'a str> {
    pairs.iter().map(|&(k, v)| (k.to_string(), v)).collect()
}
fn owned(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}
```

- [x] **Step 1: Add fallback-chain tests**

Append to `musefs-core/tests/template.rs`:

```rust
#[test]
fn fallback_chain_uses_first_present_candidate() {
    let f = fields(&[("artist", "Beck"), ("title", "Loser")]);
    let path = Template::parse("${albumartist|artist}/$title")
        .render(&f, &BTreeMap::new(), "Unknown", "flac");
    assert_eq!(path, "Beck/Loser.flac");
}

#[test]
fn fallback_chain_skips_empty_value() {
    let f = fields(&[("albumartist", ""), ("artist", "Beck"), ("title", "Loser")]);
    let path = Template::parse("${albumartist|artist}/$title")
        .render(&f, &BTreeMap::new(), "Unknown", "flac");
    assert_eq!(path, "Beck/Loser.flac");
}

#[test]
fn fallback_chain_all_empty_falls_to_default_at_top_level() {
    let f = fields(&[("title", "Loser")]);
    let path = Template::parse("${albumartist|artist}/$title")
        .render(&f, &BTreeMap::new(), "Unknown", "flac");
    assert_eq!(path, "Unknown/Loser.flac");
}

#[test]
fn field_names_are_case_insensitive() {
    let f = fields(&[("albumartist", "VA")]);
    let path = Template::parse("$AlbumArtist").render(&f, &BTreeMap::new(), "Unknown", "flac");
    assert_eq!(path, "VA.flac");
}
```

- [x] **Step 2: Add conditional-section tests**

Append:

```rust
#[test]
fn section_suppressed_when_field_absent() {
    let f = fields(&[("album", "LP")]);
    let path = Template::parse("$album[ - CD $disc]")
        .render(&f, &BTreeMap::new(), "Unknown", "flac");
    assert_eq!(path, "LP.flac");
}

#[test]
fn section_emitted_when_field_present() {
    let f = fields(&[("album", "LP"), ("disc", "2")]);
    let path = Template::parse("$album[ - CD $disc]")
        .render(&f, &BTreeMap::new(), "Unknown", "flac");
    assert_eq!(path, "LP - CD 2.flac");
}

#[test]
fn nested_section_outer_kept_inner_dropped() {
    let f = fields(&[("artist", "AC"), ("album", "LP"), ("title", "Song")]);
    let path = Template::parse("$artist[/[$date - ]$album]/$title")
        .render(&f, &BTreeMap::new(), "Unknown", "flac");
    assert_eq!(path, "AC/LP/Song.flac");
}

#[test]
fn nested_section_inner_present_renders_prefix() {
    let f = fields(&[("artist", "AC"), ("date", "1999"), ("album", "LP"), ("title", "Song")]);
    let path = Template::parse("$artist[/[$date - ]$album]/$title")
        .render(&f, &BTreeMap::new(), "Unknown", "flac");
    assert_eq!(path, "AC/1999 - LP/Song.flac");
}

#[test]
fn section_all_referenced_fields_empty_is_suppressed() {
    let f = fields(&[("album", "LP")]);
    let path = Template::parse("$album[ $[$date$]]")
        .render(&f, &BTreeMap::new(), "Unknown", "flac");
    assert_eq!(path, "LP.flac");
}

#[test]
fn escaped_brackets_render_literally_inside_kept_section() {
    let f = fields(&[("album", "LP"), ("date", "1999")]);
    let path = Template::parse("$album[ $[$date$]]")
        .render(&f, &BTreeMap::new(), "Unknown", "flac");
    assert_eq!(path, "LP [1999].flac");
}

#[test]
fn empty_field_inside_kept_section_renders_blank_not_default() {
    let f = fields(&[("album", "LP")]);
    let path = Template::parse("[$album$disc]")
        .render(&f, &BTreeMap::new(), "Unknown", "flac");
    assert_eq!(path, "LP.flac");
}

#[test]
fn empty_section_emits_nothing() {
    let f = fields(&[("title", "Song")]);
    let path = Template::parse("$title[]").render(&f, &BTreeMap::new(), "Unknown", "flac");
    assert_eq!(path, "Song.flac");
}
```

- [x] **Step 3: Add path-field tests**

Append:

```rust
#[test]
fn path_field_keeps_slashes_as_separators() {
    let f = fields(&[("p", "Pink Floyd/Animals/01 Pigs")]);
    let path = Template::parse("$!{p}").render(&f, &BTreeMap::new(), "Unknown", "flac");
    assert_eq!(path, "Pink Floyd/Animals/01 Pigs.flac");
}

#[test]
fn path_field_drops_empty_and_dot_segments() {
    let f = fields(&[("p", "a//../b/./c")]);
    let path = Template::parse("$!{p}").render(&f, &BTreeMap::new(), "Unknown", "flac");
    assert_eq!(path, "a/b/c.flac");
}

#[test]
fn path_field_all_segments_dropped_falls_to_default() {
    let f = fields(&[("p", "..")]);
    let path = Template::parse("$!{p}").render(&f, &BTreeMap::new(), "Unknown", "flac");
    assert_eq!(path, "Unknown.flac");
}

#[test]
fn path_field_fallback_chain() {
    let f = fields(&[("lidarr_path", "Artist/Album/Song")]);
    let path = Template::parse("$!{beets_path|lidarr_path}")
        .render(&f, &BTreeMap::new(), "Unknown", "flac");
    assert_eq!(path, "Artist/Album/Song.flac");
}

#[test]
fn path_field_sanitizes_control_chars_within_segments() {
    let f = fields(&[("p", "a\u{0001}b/c")]);
    let path = Template::parse("$!{p}").render(&f, &BTreeMap::new(), "Unknown", "flac");
    assert_eq!(path, "a_b/c.flac");
}
```

- [x] **Step 4: Add escaping and parser-edge-case tests**

Append:

```rust
#[test]
fn escaped_brackets_at_top_level_render_literally() {
    let f = fields(&[("title", "Song")]);
    let path = Template::parse("$[$title$]").render(&f, &BTreeMap::new(), "Unknown", "flac");
    assert_eq!(path, "[Song].flac");
}

#[test]
fn stray_closing_bracket_is_literal() {
    let f = fields(&[("title", "Song")]);
    let path = Template::parse("$title]").render(&f, &BTreeMap::new(), "Unknown", "flac");
    assert_eq!(path, "Song].flac");
}

#[test]
fn unterminated_section_runs_to_end_of_input() {
    let f = fields(&[("album", "LP"), ("disc", "2")]);
    let path = Template::parse("$album[ CD $disc").render(&f, &BTreeMap::new(), "Unknown", "flac");
    assert_eq!(path, "LP CD 2.flac");
}

#[test]
fn dollar_bang_without_brace_stays_literal() {
    let f = fields(&[("title", "Song")]);
    let path = Template::parse("$!x/$title").render(&f, &BTreeMap::new(), "Unknown", "flac");
    assert_eq!(path, "$!x/Song.flac");
}

#[test]
fn empty_braced_field_is_absent() {
    let f = fields(&[("title", "Song")]);
    let path = Template::parse("${}/$title").render(&f, &BTreeMap::new(), "Unknown", "flac");
    assert_eq!(path, "Unknown/Song.flac");
}
```

- [x] **Step 5: Run the new tests to verify they fail**

Run: `cargo test -p musefs-core --test template`
Expected: compile error / FAIL — the new syntax (`${a|b}`, `[...]`, `$!{}`, `$[`) isn't implemented yet (e.g. `${albumartist|artist}` resolves the literal name `"albumartist|artist"` to `Unknown`, sections render their brackets literally, `$!{p}` flattens slashes).

- [x] **Step 6: Replace the `Part` enum and `Template` struct doc**

In `musefs-core/src/template.rs`, replace the struct/enum block (the current `pub struct Template { parts: Vec<Part> }` and `enum Part { Literal(String), Field(String) }`, lines ~3–15) with:

```rust
/// A parsed path template: literal runs, `$field` / `${field}` substitutions
/// (with optional `${a|b}` fallback chains and `$!{field}` slash-preserving path
/// fields), and `[...]` conditional sections. Parse once per mount; `render`
/// then costs one output `String` per call, with no re-parse.
#[derive(Debug, Clone)]
pub struct Template {
    parts: Vec<Part>,
}

#[derive(Debug, Clone)]
enum Part {
    Literal(String),
    /// `names` is the `|`-separated fallback chain (length 1 for a plain field);
    /// `raw` marks a `$!{…}` path field whose '/' are kept as separators.
    Field { names: Vec<String>, raw: bool },
    /// A `[...]` conditional section: emitted only if at least one field
    /// referenced within it (transitively) is present.
    Section(Vec<Part>),
}
```

- [x] **Step 7: Replace the imports and `impl Template` block**

Replace the top-of-file `use std::collections::BTreeMap;` with:

```rust
use std::collections::BTreeMap;
use std::iter::Peekable;
use std::str::Chars;
```

Replace the entire `impl Template { ... }` block (current `parse` and `render`) with:

```rust
impl Template {
    /// Parse a beets-style template. Infallible.
    ///
    /// - `$field` / `${field}` substitute a tag field; `${a|b|c}` is a fallback
    ///   chain (first present wins). Names are matched case-insensitively.
    /// - `$!{field}` is a path field: the value's '/' are kept as directory
    ///   separators (each segment sanitized; empty / `.` / `..` dropped).
    /// - `[...]` is a conditional section, suppressed when every field it
    ///   references is empty. `$[` and `$]` emit literal brackets.
    /// - A `$` not followed by a recognized form stays literal; an unterminated
    ///   `${`/`$!{` consumes the rest as the name; an unterminated `[` runs to
    ///   end of input.
    pub fn parse(template: &str) -> Template {
        let mut chars = template.chars().peekable();
        let parts = parse_parts(&mut chars, false);
        Template { parts }
    }

    /// Render one track's path. Outside a section a missing field resolves
    /// through `fallbacks` then `default_fallback`; inside a section a missing
    /// field renders blank and drives suppression. The extension follows a '.'.
    pub fn render(
        &self,
        fields: &BTreeMap<String, &str>,
        fallbacks: &BTreeMap<String, String>,
        default_fallback: &str,
        ext: &str,
    ) -> String {
        let (mut out, _) = render_parts(&self.parts, fields, fallbacks, default_fallback, false);
        out.push('.');
        out.push_str(ext);
        out
    }
}
```

- [x] **Step 8: Replace the free functions (parse + render helpers + sanitizers)**

Replace the existing free functions at the bottom of the file (`sanitize_into` and `is_field_char`) with the full set below:

```rust
/// Parse parts until a closing `]` (when `in_section`) or end of input.
fn parse_parts(chars: &mut Peekable<Chars>, in_section: bool) -> Vec<Part> {
    let mut parts = Vec::new();
    let mut literal = String::new();
    while let Some(&c) = chars.peek() {
        match c {
            ']' if in_section => {
                chars.next(); // consume the closing ']'
                break;
            }
            '[' => {
                chars.next();
                push_literal(&mut parts, &mut literal);
                let inner = parse_parts(chars, true);
                parts.push(Part::Section(inner));
            }
            '$' => {
                chars.next(); // consume '$'
                match chars.peek() {
                    Some('[') => {
                        chars.next();
                        literal.push('[');
                    }
                    Some(']') => {
                        chars.next();
                        literal.push(']');
                    }
                    Some('{') => {
                        chars.next();
                        let names = parse_braced_names(chars);
                        push_literal(&mut parts, &mut literal);
                        parts.push(Part::Field { names, raw: false });
                    }
                    Some('!') => {
                        chars.next(); // consume '!'
                        if chars.peek() == Some(&'{') {
                            chars.next(); // consume '{'
                            let names = parse_braced_names(chars);
                            push_literal(&mut parts, &mut literal);
                            parts.push(Part::Field { names, raw: true });
                        } else {
                            literal.push('$');
                            literal.push('!');
                        }
                    }
                    Some(&nc) if is_field_char(nc) => {
                        let name = parse_unbraced_name(chars);
                        push_literal(&mut parts, &mut literal);
                        parts.push(Part::Field {
                            names: vec![name],
                            raw: false,
                        });
                    }
                    _ => literal.push('$'),
                }
            }
            _ => {
                literal.push(c);
                chars.next();
            }
        }
    }
    push_literal(&mut parts, &mut literal);
    parts
}

fn push_literal(parts: &mut Vec<Part>, literal: &mut String) {
    if !literal.is_empty() {
        parts.push(Part::Literal(std::mem::take(literal)));
    }
}

/// Consume up to the next `}` (or end of input) and split on `|` into the
/// candidate name list, lowercased for case-insensitive lookup.
fn parse_braced_names(chars: &mut Peekable<Chars>) -> Vec<String> {
    let mut content = String::new();
    for nc in chars.by_ref() {
        if nc == '}' {
            break;
        }
        content.push(nc);
    }
    content.split('|').map(|s| s.to_ascii_lowercase()).collect()
}

fn parse_unbraced_name(chars: &mut Peekable<Chars>) -> String {
    let mut name = String::new();
    while let Some(&nc) = chars.peek() {
        if is_field_char(nc) {
            name.push(nc);
            chars.next();
        } else {
            break;
        }
    }
    name.to_ascii_lowercase()
}

/// Render `parts`, returning the text and whether at least one referenced field
/// was present. `in_section` gates `default_fallback`: it is substituted only at
/// the top level (outside any `[...]`).
fn render_parts(
    parts: &[Part],
    fields: &BTreeMap<String, &str>,
    fallbacks: &BTreeMap<String, String>,
    default_fallback: &str,
    in_section: bool,
) -> (String, bool) {
    let mut out = String::new();
    let mut any_present = false;
    for part in parts {
        match part {
            Part::Literal(lit) => out.push_str(lit),
            Part::Field { names, raw: false } => {
                if let Some(value) = resolve_plain(names, fields, fallbacks) {
                    sanitize_into(&mut out, value);
                    any_present = true;
                } else if !in_section {
                    sanitize_into(&mut out, default_fallback);
                }
            }
            Part::Field { names, raw: true } => {
                if let Some(path) = resolve_path(names, fields, fallbacks) {
                    out.push_str(&path);
                    any_present = true;
                } else if !in_section {
                    sanitize_into(&mut out, default_fallback);
                }
            }
            Part::Section(inner) => {
                let (text, present) =
                    render_parts(inner, fields, fallbacks, default_fallback, true);
                if present {
                    out.push_str(&text);
                    any_present = true;
                }
            }
        }
    }
    (out, any_present)
}

/// First candidate with a non-empty value, checked against `fields` then
/// `fallbacks`.
fn resolve_plain<'a>(
    names: &[String],
    fields: &BTreeMap<String, &'a str>,
    fallbacks: &'a BTreeMap<String, String>,
) -> Option<&'a str> {
    for name in names {
        if let Some(v) = fields.get(name).copied().filter(|v| !v.is_empty()) {
            return Some(v);
        }
        if let Some(v) = fallbacks
            .get(name)
            .map(String::as_str)
            .filter(|v| !v.is_empty())
        {
            return Some(v);
        }
    }
    None
}

/// First candidate that yields at least one surviving path segment, returned as
/// the sanitized multi-segment path.
fn resolve_path(
    names: &[String],
    fields: &BTreeMap<String, &str>,
    fallbacks: &BTreeMap<String, String>,
) -> Option<String> {
    for name in names {
        let value = fields
            .get(name)
            .copied()
            .or_else(|| fallbacks.get(name).map(String::as_str));
        if let Some(value) = value {
            let path = sanitize_path(value);
            if !path.is_empty() {
                return Some(path);
            }
        }
    }
    None
}

/// Append `value` with '/' and control characters replaced by '_' so it stays a
/// single path component. The template's own '/' separators are literals, not
/// passed through here.
fn sanitize_into(out: &mut String, value: &str) {
    for c in value.chars() {
        if c == '/' || (c as u32) < 0x20 {
            out.push('_');
        } else {
            out.push(c);
        }
    }
}

/// Split `value` on '/', drop empty / `.` / `..` segments, sanitize each
/// surviving segment, and rejoin with '/'. Guarantees no empty, `.`, `..`, or
/// leading/trailing-slash components reach the virtual tree.
fn sanitize_path(value: &str) -> String {
    let mut out = String::new();
    for segment in value.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            continue;
        }
        if !out.is_empty() {
            out.push('/');
        }
        sanitize_into(&mut out, segment);
    }
    out
}

fn is_field_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}
```

- [x] **Step 9: Run the full template test file to verify all pass**

Run: `cargo test -p musefs-core --test template`
Expected: PASS — all new cases plus the original 5 (`substitutes_dollar_and_braced_fields_and_appends_ext`, `missing_field_uses_per_field_fallback_then_default`, `sanitizes_path_illegal_characters_in_values`, `lone_dollar_stays_literal`, `unterminated_brace_consumes_rest_as_field_name`).

- [x] **Step 10: Lint and format**

Run: `cargo fmt && cargo clippy -p musefs-core --all-targets -- -D warnings`
Expected: no warnings, no diff from fmt.

- [x] **Step 11: Commit**

```bash
git add musefs-core/src/template.rs musefs-core/tests/template.rs
git commit -m "feat(template): fallback chains, conditional sections, path fields

Recursive Part model (Literal / Field{names,raw} / Section). Adds
\${a|b} fallback chains, [...] conditional sections (foobar2000
all-empty suppression), \$!{field} slash-preserving path fields, and
\$[/\$] bracket escapes. Field names are now matched case-insensitively.
render's public signature is unchanged.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: Path-traversal safety invariant (property test)

**Files:**
- Test: `musefs-core/tests/template.rs` (add a `proptest!` block)

`proptest` is already a `musefs-core` dev-dependency (`musefs-core/Cargo.toml`). There is no template fuzz target (the `fuzz/` crate only covers the format layer); this proptest is the invariant gate.

- [x] **Step 1: Add the proptest import**

At the top of `musefs-core/tests/template.rs`, below the existing `use` lines, add:

```rust
use proptest::prelude::*;
```

- [x] **Step 2: Add the invariant property test**

Append to `musefs-core/tests/template.rs`:

```rust
proptest! {
    // render must never panic, and a path field must never emit an empty,
    // '.', or '..' component (no traversal / no absolute path into the tree).
    #[test]
    fn render_never_panics_and_path_fields_stay_safe(tmpl in ".{0,64}", value in ".{0,64}") {
        let f = fields(&[("p", value.as_str())]);

        // arbitrary templates must not panic
        let _ = Template::parse(&tmpl).render(&f, &BTreeMap::new(), "Unknown", "flac");

        // a path field over an adversarial value yields only safe components
        let rendered = Template::parse("$!{p}").render(&f, &BTreeMap::new(), "Unknown", "flac");
        let body = rendered.strip_suffix(".flac").expect("ext appended");
        for component in body.split('/') {
            prop_assert!(!component.is_empty());
            prop_assert_ne!(component, ".");
            prop_assert_ne!(component, "..");
        }
    }
}

#[test]
fn path_field_neutralizes_traversal_values() {
    for value in ["../../etc/passwd", "/abs/path", "a/../../b", "....//", "/", "..", "."] {
        let f = fields(&[("p", value)]);
        let rendered = Template::parse("$!{p}").render(&f, &BTreeMap::new(), "Unknown", "flac");
        let body = rendered.strip_suffix(".flac").unwrap();
        for component in body.split('/') {
            assert!(!component.is_empty(), "empty component from {value:?}");
            assert_ne!(component, ".", "'.' component from {value:?}");
            assert_ne!(component, "..", "'..' component from {value:?}");
        }
    }
}
```

- [x] **Step 3: Run the tests**

Run: `cargo test -p musefs-core --test template`
Expected: PASS (proptest runs its default 256 cases; the explicit traversal test passes). Note `....//` sanitizes to the component `....` (not `.` or `..`), which is a legal filename — the assertion only forbids empty/`.`/`..`.

- [x] **Step 4: Lint, format, commit**

```bash
cargo fmt && cargo clippy -p musefs-core --all-targets -- -D warnings
git add musefs-core/tests/template.rs
git commit -m "test(template): proptest path-traversal safety invariant

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: Documentation

**Files:**
- Modify: `README.md` (template paragraph, ~lines 66–72)
- Modify: `ARCHITECTURE.md` (Virtual tree paragraph ~lines 192–200; external-writer contract ~after line 151)
- Modify: `musefs-cli/src/lib.rs` (doc-comment on the `template` arg, line 49)

- [x] **Step 1: Update the README template description**

In `README.md`, replace the paragraph that currently reads:

```markdown
`mount` blocks until the filesystem is unmounted (`fusermount -u`, or
Ctrl-C). Paths come from a beets-style template: `$field` or `${field}`
substitutes a tag field (e.g. `$artist`, `$album`, `$title`, `$tracknumber`,
`$date`, `$genre` — any tag key in the store works, matched
case-insensitively); anything else is literal. A missing field renders as
the `--default-fallback` value (default `Unknown`). Name collisions get a
deterministic `(2)`, `(3)`, … suffix. The default template is
`$artist/$title`.
```

with:

```markdown
`mount` blocks until the filesystem is unmounted (`fusermount -u`, or
Ctrl-C). Paths come from a beets-style template (matched case-insensitively;
any tag key in the store works):

- `$field` / `${field}` — substitute a tag field (e.g. `$artist`, `$album`,
  `$title`, `$tracknumber`, `$date`, `$genre`).
- `${albumartist|artist}` — **fallback chain**: the first present field wins,
  before the `--default-fallback` value (default `Unknown`) is used.
- `[ … ]` — **conditional section**: the bracketed text is emitted only when at
  least one field inside it is present. So `$album[ - CD $disc]` yields
  `Album - CD 2`, or just `Album` on a single-disc release. Write `$[` / `$]`
  for literal brackets.
- `$!{field}` — **path field**: the value's `/` are kept as directory
  separators (each segment sanitized; empty/`.`/`..` dropped). Lets an external
  tool precompute a whole relative path into one tag and mount it as
  `--template '$!{beets_path}'`.

Anything else is literal. Name collisions get a deterministic `(2)`, `(3)`, …
suffix. The default template is `$artist/$title`.
```

- [x] **Step 2: Update the ARCHITECTURE Virtual tree paragraph**

In `ARCHITECTURE.md`, replace the sentence in the **Virtual tree** section that currently reads:

```markdown
Paths come from beets-style templates
(`template.rs`): `$field` / `${field}` substitutions over the track's tag
fields, each resolving through per-field fallbacks and then a global
`default_fallback`; rendered values are sanitized to a single path component
('/' and control characters become '_').
```

with:

```markdown
Paths come from beets-style templates
(`template.rs`): `$field` / `${field}` substitutions (with `${a|b}` fallback
chains) over the track's tag fields, each resolving through per-field fallbacks
and then a global `default_fallback`; `[...]` conditional sections suppress
their literals when every field they reference is empty. Plain values are
sanitized to a single path component ('/' and control characters become '_'),
while a `$!{field}` path field keeps '/' as directory separators (sanitizing
each segment and dropping empty/`.`/`..` segments) so a precomputed multi-level
path expands into real directories.
```

- [x] **Step 3: Document the computed-path tag workflow**

In `ARCHITECTURE.md`, in **The external-writer contract** section, insert this paragraph immediately before the line that begins `Connections are mode-typed`:

```markdown
External tools can also offload path layout entirely: a plugin evaluates its own
(arbitrarily complex) path logic, writes the resulting relative path into a
custom text tag — e.g. `INSERT INTO tags (track_id, key, value, ordinal) VALUES
(?, 'beets_path', 'Pink Floyd/Animals/01 Pigs', 0)` — and the user mounts with
`--template '$!{beets_path}'`. Because the field map is just the (lowercased) tag
keys, any number of such tags (`beets_path`, `lidarr_path`, …) can back
different concurrent mounts. The path field keeps embedded `/` as directory
separators but sanitizes each segment and drops empty/`.`/`..` segments, so a
misbehaving writer cannot inject traversal or empty components into the tree.

```

- [x] **Step 4: Update the CLI doc-comment**

In `musefs-cli/src/lib.rs`, replace the doc-comment line:

```rust
    /// Path template, e.g. "$albumartist/$album/$title".
```

with:

```rust
    /// Path template, e.g. "$albumartist/$album/$title". Supports ${a|b}
    /// fallback chains, [...] conditional sections ($[/$] for literal
    /// brackets), and $!{field} path fields that keep '/' as separators.
```

- [x] **Step 5: Verify docs build cleanly and CLI still parses**

Run: `cargo build -p musefs-cli && cargo test -p musefs-cli`
Expected: PASS (the doc-comment change is a comment; the `default-fallback`/`template` default-value tests at `musefs-cli/src/lib.rs:254-255` are unaffected).

- [x] **Step 6: Format and commit**

```bash
cargo fmt
git add README.md ARCHITECTURE.md musefs-cli/src/lib.rs
git commit -m "docs(template): document fallback chains, sections, path fields

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Final verification

- [x] **Run the full workspace suite (mirrors the pre-commit gate):**

Run: `cargo fmt --all --check && cargo clippy --all-targets -- -D warnings && cargo test`
Expected: all green.

- [ ] **Manual end-to-end smoke check (optional, exercises the real mount):**

Demonstrate each feature against a real store/mount, e.g.:

```bash
# fallback chain + conditional section
musefs mount <db> <mnt> --template '${albumartist|artist}/$album[ - CD $disc]/$track $title' &
# computed-path tag (after a plugin/SQL writes a 'beets_path' tag)
musefs mount <db> <mnt2> --template '$!{beets_path}' &
```

Confirm: a track missing `albumartist` falls back to `artist`; single-disc albums show no `CD` folder; a `beets_path` value with `/` expands into nested directories; original audio bytes are untouched (the cardinal invariant — only paths change).
