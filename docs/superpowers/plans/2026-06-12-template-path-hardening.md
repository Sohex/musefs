# Template & Path Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Reject malformed mount-time templates (control/NUL bytes in literals #275, excessive section nesting #304) at `Musefs::open()`, and clamp hostile path-field segment counts (#303) at render time.

**Architecture:** Two independent changes. (1) `sanitize_path` gains a post-filter segment cap — fully infallible, isolated, lands first. (2) `Template::parse` becomes fallible (`Result<Template, TemplateError>`); a new `CoreError::InvalidTemplate` surfaces the error from `open()`. Because parse rejects deep nesting at the single construction point, `render_parts`/`collect_field_names` need no per-consumer guards. The fallible-parse change is **one atomic commit** (see Task 2 preamble).

**Tech Stack:** Rust workspace (musefs-core), `thiserror` for error enums, `proptest`, `cargo test`/`clippy`/`fmt`. Spec: `docs/superpowers/specs/2026-06-12-template-path-hardening-design.md`.

---

## Project constraints (read before starting)

- **Pre-commit hook runs the FULL workspace test suite + `clippy -D warnings` + `fmt`.** Every commit must compile and be green. A signature change cannot be split across commits — all call sites land together.
- Use **Serena** symbolic tools (`get_symbols_overview`, `find_symbol`, `replace_symbol_body`) for code reads/edits per the project's tooling rules; the code blocks below are the exact target bodies.
- This change is in `musefs-core`, not the format layer, so the out-of-workspace `fuzz/` crate is unaffected (no `cargo +nightly fuzz build` needed). It does not touch getattr/read paths, so the `metrics` feature is unaffected.
- Run targeted tests with: `cargo test -p musefs-core` (integration tests live in `musefs-core/tests/template.rs`; unit tests in `musefs-core/src/template.rs`).

---

## File Structure

| File | Responsibility | Change |
| ---- | -------------- | ------ |
| `musefs-core/src/template.rs` | Template parse/render; sanitizers | Add `TemplateError`, `MAX_SECTION_DEPTH`, `MAX_PATH_FIELD_SEGMENTS`; make `parse`/`parse_parts` fallible; cap `sanitize_path`; in-module tests |
| `musefs-core/src/error.rs` | `CoreError` enum | Add `InvalidTemplate(#[from] TemplateError)` |
| `musefs-core/src/lib.rs` | Crate exports | Export `TemplateError` |
| `musefs-core/src/facade.rs` | `Musefs::open` + facade tests | `?` at prod site; `.expect` at 2 test sites; new `open()`-rejection test |
| `musefs-core/tests/template.rs` | Integration tests | `parse` helper + rename 30 sites; rewrite proptest; #275/#303 tests |

---

## Task 1: #303 — Path-field segment cap (independent, infallible)

This task does not touch `parse`. It is a self-contained TDD cycle and commits on its own.

**Files:**
- Modify: `musefs-core/src/template.rs` (`sanitize_path`, new const)
- Test: `musefs-core/tests/template.rs`

- [ ] **Step 1: Write the failing test**

Add to `musefs-core/tests/template.rs` (file scope, near the other `$!{p}` tests, e.g. after `path_field_neutralizes_traversal_values`):

```rust
#[test]
fn path_field_segment_count_is_capped() {
    // MAX_PATH_FIELD_SEGMENTS is a private const = 64 in template.rs. A hostile
    // 256 KiB `a/a/a/...` tag would expand to tens of thousands of segments; the
    // cap clamps the rendered path to at most 64 directory components.
    let value: String = std::iter::repeat("a").take(200).collect::<Vec<_>>().join("/");
    let f = fields(&[("p", value.as_str())]);
    let rendered = Template::parse("$!{p}").render(&f, &BTreeMap::new(), "Unknown", "flac");
    let body = rendered.strip_suffix(".flac").expect("ext appended");
    assert_eq!(body.split('/').count(), 64, "clamped to MAX_PATH_FIELD_SEGMENTS");
}

#[test]
fn path_field_under_cap_is_unchanged() {
    let f = fields(&[("p", "a/b/c")]);
    let rendered = Template::parse("$!{p}").render(&f, &BTreeMap::new(), "Unknown", "flac");
    assert_eq!(rendered, "a/b/c.flac");
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p musefs-core --test template path_field_segment_count_is_capped`
Expected: FAIL — `path_field_segment_count_is_capped` asserts 64 but renders 200 components. (`path_field_under_cap_is_unchanged` already passes — it documents the no-regression case.)

- [ ] **Step 3: Add the constant and cap logic**

In `musefs-core/src/template.rs`, add the constant near the top of the file (after the imports, before `pub struct Template`):

```rust
/// Max surviving segments a single `$!{}` path field may expand into. A hostile
/// 256 KiB tag shaped `a/a/a/...` would otherwise build tens of thousands of
/// directory levels (depth amplification across the DB trust boundary, #303).
const MAX_PATH_FIELD_SEGMENTS: usize = 64;
```

Replace the body of `sanitize_path` with the post-filter-capped version:

```rust
fn sanitize_path(value: &str) -> String {
    let mut out = String::new();
    let mut count = 0usize;
    for segment in value.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            continue;
        }
        if count == MAX_PATH_FIELD_SEGMENTS {
            break;
        }
        if !out.is_empty() {
            out.push('/');
        }
        sanitize_into(&mut out, segment);
        count += 1;
    }
    out
}
```

Note: `count` increments only *after* the empty/`.`/`..` guard, so dropped segments do not consume cap budget — `././a/./b` keeps both real segments.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p musefs-core --test template`
Expected: PASS — both new tests pass, and the existing `path_field_neutralizes_traversal_values` + `render_never_panics_and_path_fields_stay_safe` stay green.

- [ ] **Step 5: Commit**

```bash
git add musefs-core/src/template.rs musefs-core/tests/template.rs
git commit -m "$(cat <<'EOF'
fix(template): cap path-field segment count at render (#303)

A 256 KiB $!{field} value shaped a/a/a/... expanded into tens of thousands
of virtual-tree path segments. sanitize_path now stops after
MAX_PATH_FIELD_SEGMENTS (64) surviving segments, counted post-filter.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Fallible `Template::parse` — reject malformed config (#275, #304)

**This is one atomic commit.** A Rust signature change does not compile until the error type, both `parse`/`parse_parts` signatures, all 34 call sites, and the proptest are consistent — and the pre-commit hook rejects any uncompilable/red commit. So all edits below land together; `cargo build`/`test` is run once at the end. The negative tests (Steps 9–11) are this task's acceptance criteria; they exercise the new API in the same commit that introduces it.

**Files:**
- Modify: `musefs-core/src/template.rs` (TemplateError, MAX_SECTION_DEPTH, `parse`, `parse_parts`, doc, in-module tests)
- Modify: `musefs-core/src/error.rs` (`CoreError::InvalidTemplate`)
- Modify: `musefs-core/src/lib.rs` (export `TemplateError`)
- Modify: `musefs-core/src/facade.rs` (prod `?`, 2 test `.expect`, new open() test)
- Modify: `musefs-core/tests/template.rs` (`parse` helper, 30-site rename, proptest rewrite, #275 tests)

- [ ] **Step 1: Add `TemplateError` and `MAX_SECTION_DEPTH` to `template.rs`**

At the top of `musefs-core/src/template.rs`, add the import alongside the existing `use` lines:

```rust
use thiserror::Error;
```

Add the error type and constant near the top (next to `MAX_PATH_FIELD_SEGMENTS` from Task 1, before `pub struct Template`):

```rust
/// Max `[...]` section nesting depth accepted by [`Template::parse`]. Beyond this
/// the parser rejects the template rather than recursing further (#304). Real
/// templates nest 2–3 deep; 64 is generous headroom that still bounds the
/// adversarial `[[[…` case.
const MAX_SECTION_DEPTH: usize = 64;

/// Why a template was rejected at parse time. Surfaced to the operator via
/// [`crate::CoreError::InvalidTemplate`] when `Musefs::open` parses a bad
/// `--template`.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TemplateError {
    /// `[...]` sections nested deeper than `limit`.
    #[error("template nesting exceeds the maximum depth of {limit}")]
    NestingTooDeep { limit: usize },
    /// A literal run contains a control byte (`< 0x20`, includes NUL), which is
    /// not a valid POSIX path-component byte.
    #[error("template literal contains control byte {byte:#04x}")]
    ControlByte { byte: u8 },
}
```

- [ ] **Step 2: Make `parse_parts` fallible**

Replace the body of `parse_parts` in `musefs-core/src/template.rs`. Changes: return type `Result<Vec<Part>, TemplateError>`; parameter `in_section: bool` → `depth: usize`; `']' if in_section` → `']' if depth > 0`; nesting check + `?` on recursion in the `'['` arm; control-byte check in the catch-all arm; `Ok(parts)` at the end.

```rust
/// Parse parts until a closing `]` (when `depth > 0`) or end of input. `depth`
/// is the current `[...]` nesting level (0 = top level).
fn parse_parts(chars: &mut Peekable<Chars>, depth: usize) -> Result<Vec<Part>, TemplateError> {
    let mut parts = Vec::new();
    let mut literal = String::new();
    while let Some(&c) = chars.peek() {
        match c {
            ']' if depth > 0 => {
                chars.next(); // consume the closing ']'
                break;
            }
            '[' => {
                chars.next();
                push_literal(&mut parts, &mut literal);
                if depth + 1 > MAX_SECTION_DEPTH {
                    return Err(TemplateError::NestingTooDeep {
                        limit: MAX_SECTION_DEPTH,
                    });
                }
                let inner = parse_parts(chars, depth + 1)?;
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
                if (c as u32) < 0x20 {
                    // The only place a raw input control byte can reach a literal
                    // run; the $[ / $] / $! escape arms push only brackets and
                    // '$'/'!'. Cast is lossless under the < 0x20 guard.
                    return Err(TemplateError::ControlByte { byte: c as u8 });
                }
                literal.push(c);
                chars.next();
            }
        }
    }
    push_literal(&mut parts, &mut literal);
    Ok(parts)
}
```

- [ ] **Step 3: Make `Template::parse` fallible and update its doc**

In `impl Template`, replace `parse`. Change the signature to return `Result`, call `parse_parts(&mut chars, 0)?`, wrap in `Ok`, and change the doc line `/// Parse a beets-style template. Infallible.` to note it now returns `Result`:

```rust
    /// Parse a beets-style template. Returns `Err` for a template that cannot
    /// produce valid path components: control/NUL bytes in literal text
    /// (#275) or `[...]` nesting deeper than [`MAX_SECTION_DEPTH`] (#304).
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
    pub fn parse(template: &str) -> Result<Template, TemplateError> {
        let mut chars = template.chars().peekable();
        let parts = parse_parts(&mut chars, 0)?;
        Ok(Template { parts })
    }
```

- [ ] **Step 4: Add the `CoreError::InvalidTemplate` variant**

In `musefs-core/src/error.rs`, add a variant to `CoreError` (place it near the other `#[error(transparent)]` `#[from]` variants, e.g. after `Format`):

```rust
    #[error(transparent)]
    InvalidTemplate(#[from] crate::template::TemplateError),
```

- [ ] **Step 5: Export `TemplateError` from the crate root**

In `musefs-core/src/lib.rs`, change line 25 from:

```rust
pub use template::Template;
```

to:

```rust
pub use template::{Template, TemplateError};
```

- [ ] **Step 6: Update the production call site in `facade.rs`**

In `Musefs::open` (`musefs-core/src/facade.rs:255`), add `?`:

```rust
        let template = Template::parse(&config.template)?;
```

(`open` returns `Result<Musefs>`; `#[from]` converts `TemplateError` → `CoreError::InvalidTemplate`.)

- [ ] **Step 7: Update the two facade test call sites**

`musefs-core/src/facade.rs:1530` — inside `render_entries` call:

```rust
        let (entries, snapshot) =
            Musefs::render_entries(&db, &Template::parse(&cfg.template).expect("valid template"), &cfg)
                .unwrap();
```

`musefs-core/src/facade.rs:1787` — in `full_rebuild_gives_bare_colliding_name_to_lower_id`:

```rust
        let template = Template::parse(&config.template).expect("valid template");
```

- [ ] **Step 8: Update the in-module `template.rs` test call site**

`musefs-core/src/template.rs:315` (single site in `mod tests`):

```rust
        let t = Template::parse("$artist/$!{beets_path}/[$disc - ]${title|name}")
            .expect("valid template");
```

- [ ] **Step 9: Rename existing integration call sites, then add the `parse` helper**

Do the mechanical rename **first** (no helper exists yet, so nothing self-corrupts). This rewrites every existing `Template::parse(` site in the file — the ~28 original `.render`/`.referenced_fields` chains plus the 2 from Task 1 — to the bare `parse(` helper call. The proptest's `parse(&tmpl)` line is fixed up in Step 10; the new `Err`-asserting tests are added afterward in Step 11, so the sed never touches them:

```bash
sed -i 's/Template::parse(/parse(/g' musefs-core/tests/template.rs
```

Then add the file-scope helper next to `fields`/`owned` (this file is NOT wrapped in a `mod`), using the fully-qualified `Template::parse` so it is not self-recursive:

```rust
fn parse(t: &str) -> Template {
    Template::parse(t).expect("valid template")
}
```

Verify no stray self-reference remains: `grep -n 'Template::parse' musefs-core/tests/template.rs` should now show only the helper body (one line).

- [ ] **Step 10: Rewrite the proptest for the fallible signature**

Replace the whole `render_never_panics_and_path_fields_stay_safe` proptest body in `musefs-core/tests/template.rs`. The `tmpl` strategy now generates control bytes and deep `[` runs that `parse` legitimately rejects, so call `Template::parse` directly and only render on `Ok`; the fixed `$!{p}` half uses the `parse` helper:

```rust
proptest! {
    // An arbitrary template either fails to parse or renders without panicking;
    // a path field over an adversarial value yields only safe components.
    #[test]
    fn render_never_panics_and_path_fields_stay_safe(tmpl in ".{0,64}", value in ".{0,64}") {
        let f = fields(&[("p", value.as_str())]);

        // arbitrary templates must parse-or-reject, never panic on render
        if let Ok(t) = Template::parse(&tmpl) {
            let _ = t.render(&f, &BTreeMap::new(), "Unknown", "flac");
        }

        // a path field over an adversarial value yields only safe components
        let rendered = parse("$!{p}").render(&f, &BTreeMap::new(), "Unknown", "flac");
        let body = rendered.strip_suffix(".flac").expect("ext appended");
        for component in body.split('/') {
            prop_assert!(!component.is_empty());
            prop_assert_ne!(component, ".");
            prop_assert_ne!(component, "..");
        }
    }
}
```

(The `value in ".{0,64}"` half is unchanged: path-field *values* are still sanitized — control bytes → `_` — never rejected.)

- [ ] **Step 11: Add the #275 / #304 acceptance tests**

Add the #275 parse-rejection tests to `musefs-core/tests/template.rs` (needs `TemplateError` in scope — add `use musefs_core::TemplateError;` to the imports):

```rust
#[test]
fn template_literal_nul_is_rejected() {
    assert!(matches!(
        Template::parse("a\0b/$title"),
        Err(TemplateError::ControlByte { byte: 0 })
    ));
}

#[test]
fn template_literal_control_byte_is_rejected() {
    assert!(matches!(
        Template::parse("a\u{01}b"),
        Err(TemplateError::ControlByte { byte: 1 })
    ));
}

#[test]
fn bracket_escapes_and_slashes_still_parse() {
    // $[ / $] escapes and ordinary '/' separators are valid literal bytes.
    let path = parse("$[$title$]/a/b").render(
        &fields(&[("title", "X")]),
        &BTreeMap::new(),
        "Unknown",
        "flac",
    );
    assert_eq!(path, "[X]/a/b.flac");
}
```

Add the #304 nesting-boundary test to the in-module `mod tests` in `musefs-core/src/template.rs` (it can see the private `MAX_SECTION_DEPTH`):

```rust
    #[test]
    fn nesting_at_limit_parses_one_past_limit_rejected() {
        let at_limit = "[".repeat(MAX_SECTION_DEPTH);
        assert!(Template::parse(&at_limit).is_ok(), "{MAX_SECTION_DEPTH} deep parses");

        let past_limit = "[".repeat(MAX_SECTION_DEPTH + 1);
        assert!(matches!(
            Template::parse(&past_limit),
            Err(TemplateError::NestingTooDeep { limit }) if limit == MAX_SECTION_DEPTH
        ));
    }
```

- [ ] **Step 12: Add the `open()`-surfacing test in `facade.rs`**

Add to the `mod tests` in `musefs-core/src/facade.rs` (reuses the `Db::open_in_memory()` fixture pattern; no tracks needed — parse precedes build):

```rust
    #[test]
    fn open_rejects_template_with_control_byte() {
        let db = musefs_db::Db::open_in_memory().unwrap();
        let config = MountConfig {
            template: "a\0b/$title".to_string(),
            fallbacks: BTreeMap::new(),
            default_fallback: "Unknown".to_string(),
            mode: Mode::Synthesis,
            poll_interval: std::time::Duration::ZERO,
            case_insensitive: false,
        };
        assert!(matches!(
            Musefs::open(db, config),
            Err(crate::CoreError::InvalidTemplate(_))
        ));
    }
```

(If `BTreeMap`/`std::time::Duration` are not already imported in the facade `mod tests`, use fully-qualified paths or add the `use` — match the surrounding tests, several of which build `MountConfig` the same way.)

- [ ] **Step 13: Build, lint, format, test the whole workspace**

Run each and confirm success:

```bash
cargo build
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test -p musefs-core
cargo test
```

Expected: clean build; no clippy warnings; all tests pass, including the new #275/#303/#304 tests, the rewritten proptest, and every renamed site.

- [ ] **Step 14: Commit**

```bash
git add musefs-core/src/template.rs musefs-core/src/error.rs musefs-core/src/lib.rs \
        musefs-core/src/facade.rs musefs-core/tests/template.rs
git commit -m "$(cat <<'EOF'
feat(template): reject malformed templates at mount (#275 #304)

Template::parse is now fallible: literal control/NUL bytes (#275) and
[...] nesting past MAX_SECTION_DEPTH (#304) return TemplateError, surfaced
via CoreError::InvalidTemplate so a bad --template fails Musefs::open with
a clear message. Bounding nesting at the single parse construction point
keeps render/referenced_fields recursion safe without per-consumer guards.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Self-Review notes (verification of plan against spec)

- **#275** → Task 2 Steps 2 (control-byte reject in catch-all arm), 11 (NUL + 0x01 reject, escapes still parse), 12 (open() surfaces InvalidTemplate). ✔
- **#304** → Task 2 Steps 2 (depth check `depth + 1 > MAX_SECTION_DEPTH`), 11 (64 ok / 65 Err boundary). ✔
- **#303** → Task 1 (post-filter segment cap; clamp test + under-cap no-regression test). ✔
- **30-site rename + proptest** → Task 2 Steps 9–10, explicit per spec §1. ✔
- **Doc/infallibility update** → Task 2 Step 3. ✔
- **Out of scope** honored: `sanitize_into` unchanged; no `insert_file` backstop; no DB-limit changes. ✔
