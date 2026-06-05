# Compile-Once Template Rendering (#137) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop re-parsing the path template per track per rebuild and stop allocating a `String` per substituted field — parse once into a `Template`, render by appending sanitized values straight into the output, and let the per-track field map borrow tag values.

**Architecture:** A new `Template` type (`Vec<Part>` of `Literal(String) | Field(String)`) replaces the `render_path` free function in `musefs-core/src/template.rs`. It is parsed once in `Musefs::open` from `MountConfig.template` (which stays a `String` — it's the CLI-facing config), stored as a field on `Musefs`, and threaded into `render_one`/`render_entries`/`build_full`. `tags_to_fields` returns `BTreeMap<String, &str>` so first values borrow from the tag rows; the borrowed map never escapes `render_one`. Parse semantics are preserved exactly: `$field`/`${field}`, a lone `$` stays literal, an unterminated `${` consumes the rest as the field name.

**Tech Stack:** Rust (musefs-core only). Spec: `docs/superpowers/specs/2026-06-05-allocation-cleanups-design.md` (PR 2 section).

**File map:**
- `musefs-core/src/template.rs` — `Template`/`Part`, `parse`, `render`, `sanitize_into`; `render_path`/`resolve`/`sanitize` removed at the end
- `musefs-core/tests/template.rs` — rewritten against `Template`, plus two new parse-edge tests
- `musefs-core/src/mapping.rs` — `tags_to_fields` returns borrowed values; three tests updated
- `musefs-core/src/facade.rs` — `Musefs.template` field, threading through `open`/`render_one`/`render_entries`/`build_full`/`rebuild_full`/incremental; one test updated
- `musefs-core/src/lib.rs` — re-export `Template` instead of `render_path`
- `musefs-core/src/refresh_diff.rs` — doc comment mentions `Template::render`

---

### Task 1: Branch setup

**Files:** none

- [ ] **Step 1: Create the branch from up-to-date main**

```bash
git checkout main && git pull && git checkout -b compiled-template
```

Expected: `Switched to a new branch 'compiled-template'`

### Task 2: `Template` type with tests (additive — `render_path` survives until Task 3)

**Files:**
- Test: `musefs-core/tests/template.rs` (full rewrite)
- Modify: `musefs-core/src/template.rs` (add `Template`; keep `render_path`/`resolve`/`sanitize` for now — `facade.rs` still uses them)
- Modify: `musefs-core/src/lib.rs:23`

- [ ] **Step 1: Rewrite the integration tests against `Template`**

Replace the entire contents of `musefs-core/tests/template.rs` with:

```rust
use musefs_core::Template;
use std::collections::BTreeMap;

fn fields<'a>(pairs: &[(&str, &'a str)]) -> BTreeMap<String, &'a str> {
    pairs.iter().map(|&(k, v)| (k.to_string(), v)).collect()
}

fn owned(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[test]
fn substitutes_dollar_and_braced_fields_and_appends_ext() {
    let f = fields(&[
        ("albumartist", "Pink Floyd"),
        ("album", "Animals"),
        ("title", "Pigs"),
    ]);
    let path = Template::parse("$albumartist/${album}/$title").render(
        &f,
        &BTreeMap::new(),
        "Unknown",
        "flac",
    );
    assert_eq!(path, "Pink Floyd/Animals/Pigs.flac");
}

#[test]
fn missing_field_uses_per_field_fallback_then_default() {
    let f = fields(&[("title", "Untitled Track")]);
    let fallbacks = owned(&[("albumartist", "Unknown Artist")]);
    let path = Template::parse("$albumartist/$album/$title").render(
        &f,
        &fallbacks,
        "Unknown",
        "flac",
    );
    assert_eq!(path, "Unknown Artist/Unknown/Untitled Track.flac");
}

#[test]
fn sanitizes_path_illegal_characters_in_values() {
    let f = fields(&[("artist", "AC/DC"), ("title", "Back\u{0000}In")]);
    let path = Template::parse("$artist/$title").render(&f, &BTreeMap::new(), "Unknown", "flac");
    assert_eq!(path, "AC_DC/Back_In.flac");
}

#[test]
fn lone_dollar_stays_literal() {
    let f = fields(&[("title", "Song")]);
    // '$' before a non-field char, and a trailing '$', both stay literal;
    // the non-field char after the lone '$' is kept.
    let path =
        Template::parse("100$ bill/$title$").render(&f, &BTreeMap::new(), "Unknown", "mp3");
    assert_eq!(path, "100$ bill/Song$.mp3");
}

#[test]
fn unterminated_brace_consumes_rest_as_field_name() {
    let f = fields(&[("album", "X")]);
    let path = Template::parse("${album").render(&f, &BTreeMap::new(), "Unknown", "ogg");
    assert_eq!(path, "X.ogg");
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p musefs-core --test template`
Expected: compile error — `no 'Template' in the root` (unresolved import `musefs_core::Template`).

- [ ] **Step 3: Add `Template` to `musefs-core/src/template.rs`**

Insert AFTER the existing `use std::collections::BTreeMap;` (line 1) and ABOVE the existing `sanitize` (which stays untouched for now, as do `resolve` and `render_path` — `facade.rs` still calls them until Task 3). `is_field_char` already exists below; do not duplicate it:

```rust
/// A parsed path template: literal runs interleaved with `$field` / `${field}`
/// substitutions. Parse once per mount; `render` then costs one output `String`
/// per call, with no re-parse and no per-field intermediates.
#[derive(Debug, Clone)]
pub struct Template {
    parts: Vec<Part>,
}

#[derive(Debug, Clone)]
enum Part {
    Literal(String),
    Field(String),
}

impl Template {
    /// Parse a beets-style template. Infallible: `$field` and `${field}` become
    /// substitutions, a `$` not followed by a field name stays literal, and an
    /// unterminated `${` consumes the rest of the template as the field name.
    pub fn parse(template: &str) -> Template {
        let mut parts = Vec::new();
        let mut literal = String::new();
        let mut chars = template.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '$' {
                literal.push(c);
                continue;
            }
            match chars.peek() {
                Some('{') => {
                    chars.next(); // consume '{'
                    let mut name = String::new();
                    for nc in chars.by_ref() {
                        if nc == '}' {
                            break;
                        }
                        name.push(nc);
                    }
                    if !literal.is_empty() {
                        parts.push(Part::Literal(std::mem::take(&mut literal)));
                    }
                    parts.push(Part::Field(name));
                }
                Some(&nc) if is_field_char(nc) => {
                    let mut name = String::new();
                    while let Some(&nc) = chars.peek() {
                        if is_field_char(nc) {
                            name.push(nc);
                            chars.next();
                        } else {
                            break;
                        }
                    }
                    if !literal.is_empty() {
                        parts.push(Part::Literal(std::mem::take(&mut literal)));
                    }
                    parts.push(Part::Field(name));
                }
                _ => literal.push('$'), // a literal '$' not followed by a field name
            }
        }
        if !literal.is_empty() {
            parts.push(Part::Literal(literal));
        }
        Template { parts }
    }

    /// Render one track's path. Each field resolves through `fields`, then
    /// `fallbacks`, then `default_fallback`, and is sanitized to a single path
    /// component as it is appended. The extension is appended after a '.'.
    pub fn render(
        &self,
        fields: &BTreeMap<String, &str>,
        fallbacks: &BTreeMap<String, String>,
        default_fallback: &str,
        ext: &str,
    ) -> String {
        let mut out = String::new();
        for part in &self.parts {
            match part {
                Part::Literal(lit) => out.push_str(lit),
                Part::Field(name) => {
                    let value = fields
                        .get(name)
                        .copied()
                        .or_else(|| fallbacks.get(name).map(String::as_str))
                        .unwrap_or(default_fallback);
                    sanitize_into(&mut out, value);
                }
            }
        }
        out.push('.');
        out.push_str(ext);
        out
    }
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
```

- [ ] **Step 4: Re-export `Template` from `musefs-core/src/lib.rs`**

Change line 23 from:

```rust
pub use template::render_path;
```

to:

```rust
pub use template::{render_path, Template};
```

(`render_path` is still consumed by `facade.rs` until Task 3, where this becomes `pub use template::Template;`.)

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p musefs-core --test template`
Expected: 5 tests PASS.

- [ ] **Step 6: Commit**

```bash
git add musefs-core/src/template.rs musefs-core/src/lib.rs musefs-core/tests/template.rs
git commit -m "$(cat <<'EOF'
Add compile-once Template type for path rendering (#137)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Task 3: Wire `Template` through the facade; borrow field values; remove `render_path`

`tags_to_fields`'s new return type and `Template::render`'s `fields` parameter must change together — `render_path` takes `BTreeMap<String, String>`, `render` takes `BTreeMap<String, &str>`, so the facade swap and the mapping change are one compile unit.

**Files:**
- Modify: `musefs-core/src/mapping.rs` (`tags_to_fields` ~line 20; tests at ~126-127, ~136-138, ~198-199)
- Modify: `musefs-core/src/facade.rs` (import ~13, `Musefs` struct ~117, `open` ~209, `render_one` ~248, `render_entries` ~267, `build_full` ~294, `rebuild_full` ~333, incremental ~440, test `render_entries_returns_paths_and_snapshot` ~1297)
- Modify: `musefs-core/src/template.rs` (delete `render_path`, `resolve`, `sanitize`)
- Modify: `musefs-core/src/lib.rs:23`, `musefs-core/src/refresh_diff.rs:7`

- [ ] **Step 1: Update the mapping unit tests to the borrowed-value map**

The failing tests first. In `musefs-core/src/mapping.rs` tests, the map's values become `&str`, so `.map(String::as_str)` becomes `.copied()`:

In `fields_take_first_value_per_key` (~lines 126-127):

```rust
        assert_eq!(fields.get("artist").copied(), Some("Alice"));
        assert_eq!(fields.get("album").copied(), Some("X"));
```

In `tags_to_fields_lowercases_keys_for_template_lookup` (~lines 137-138):

```rust
        assert_eq!(fields.get("myrating").copied(), Some("5"));
        assert_eq!(fields.get("albumartist").copied(), Some("VA"));
```

In `binary_rows_do_not_pollute_tags_to_fields` (~line 199):

```rust
        assert_eq!(fields.get("artist").copied(), Some("A"));
```

- [ ] **Step 2: Run the mapping tests to verify they fail**

Run: `cargo test -p musefs-core tags_to_fields`
Expected: compile error — `.copied()` on `Option<&String>` (type mismatch until the signature changes).

- [ ] **Step 3: Change `tags_to_fields` to borrow values**

Replace the function in `musefs-core/src/mapping.rs`:

```rust
/// Build the field map used for path-template rendering: the first value (lowest
/// ordinal) of each key, borrowed from the rows. Relies on `Db::get_tags` ordering
/// by `(key, ordinal)`. Keys are ASCII-lowercased so a `$field` placeholder
/// resolves regardless of the stored key's case (unlike `tags_to_inputs`, which
/// passes keys verbatim to synthesis).
pub(crate) fn tags_to_fields(tags: &[Tag]) -> BTreeMap<String, &str> {
    let mut map = BTreeMap::new();
    for t in tags {
        map.entry(t.key.to_ascii_lowercase())
            .or_insert_with(|| t.value.as_str());
    }
    map
}
```

- [ ] **Step 4: Thread `Template` through `facade.rs`**

All in `musefs-core/src/facade.rs`:

(a) Import (~line 13) — replace `use crate::template::render_path;` with:

```rust
use crate::template::Template;
```

(b) `Musefs` struct (~line 117) — add a field right after `config`:

```rust
    config: MountConfig,
    /// Compiled once from `config.template`; rendering never re-parses.
    template: Template,
```

(c) `open` (~line 209) — compile before the build, pass it down, store it. Replace:

```rust
        let (tree, snapshot) = Self::build_full(&db, &config, &mut alloc)?;
```

with:

```rust
        let template = Template::parse(&config.template);
        let (tree, snapshot) = Self::build_full(&db, &template, &config, &mut alloc)?;
```

and add `template,` to the `Ok(Musefs { ... })` initializer (next to `config`).

(d) `render_one` (~line 248) — replace the whole function:

```rust
    /// Render a single track's path from its tags + format. The one place
    /// `Template::render` is called, shared by full and incremental rebuilds.
    fn render_one(
        template: &Template,
        config: &MountConfig,
        format: musefs_db::Format,
        tags: &[musefs_db::Tag],
    ) -> String {
        let fields = tags_to_fields(tags);
        template.render(
            &fields,
            &config.fallbacks,
            &config.default_fallback,
            format.as_str(),
        )
    }
```

(e) `render_entries` (~line 267) — add the parameter and forward it. Signature becomes:

```rust
    fn render_entries<M>(
        db: &Db<M>,
        template: &Template,
        config: &MountConfig,
    ) -> Result<(Vec<(i64, String)>, HashMap<i64, TrackRenderState>)> {
```

and the call inside its loop becomes:

```rust
            let path = Self::render_one(template, config, t.format, &tags);
```

(f) `build_full` (~line 294) — add the parameter and forward it:

```rust
    fn build_full<M>(
        db: &Db<M>,
        template: &Template,
        config: &MountConfig,
        alloc: &mut InodeAllocator,
    ) -> Result<(VirtualTree, HashMap<i64, TrackRenderState>)> {
        let (entries, snapshot) = Self::render_entries(db, template, config)?;
        Ok((VirtualTree::build_with(&entries, alloc), snapshot))
    }
```

(g) `rebuild_full` (~line 333) — replace:

```rust
            .with(|db| Self::render_entries(db, &self.config))?;
```

with:

```rust
            .with(|db| Self::render_entries(db, &self.template, &self.config))?;
```

(h) Incremental path (~line 440) — replace:

```rust
                            path: Self::render_one(&self.config, fmt, &tags),
```

with:

```rust
                            path: Self::render_one(&self.template, &self.config, fmt, &tags),
```

(i) Test `render_entries_returns_paths_and_snapshot` (~line 1297) — replace:

```rust
        let (entries, snapshot) = Musefs::render_entries(&db, &cfg).unwrap();
```

with:

```rust
        let (entries, snapshot) =
            Musefs::render_entries(&db, &Template::parse(&cfg.template), &cfg).unwrap();
```

- [ ] **Step 5: Delete `render_path`, `resolve`, and `sanitize`; fix the re-export and the stale comment**

(a) In `musefs-core/src/template.rs`, delete the three now-dead items: `fn sanitize`, `fn resolve`, and `pub fn render_path` (keep `is_field_char` and everything added in Task 2).

(b) In `musefs-core/src/lib.rs:23`, replace:

```rust
pub use template::{render_path, Template};
```

with:

```rust
pub use template::Template;
```

(c) In `musefs-core/src/refresh_diff.rs` (~line 7), in the `TrackRenderState` doc comment, replace `render_path` with `Template::render`, i.e. the line becomes:

```rust
/// re-render. `(content_version, format)` is the render key (the only track-level
/// inputs to `Template::render`); `path` is the last rendered path, reused verbatim for
```

- [ ] **Step 6: Run the crate tests**

Run: `cargo test -p musefs-core`
Expected: all tests PASS — the mapping tests from Step 1, the template integration tests, and the facade/reader tests that exercise both rebuild paths end to end.

- [ ] **Step 7: Lint and format check**

Run: `cargo clippy --all-targets -p musefs-core && cargo fmt --all --check`
Expected: no warnings (dead-code included — confirms `resolve`/`sanitize` are really gone), no diff. (`--all-targets` matters: `musefs-core/benches/read_throughput.rs` and the `tests/` dirs are API consumers a plain build skips.)

- [ ] **Step 8: Commit**

```bash
git add musefs-core/src/template.rs musefs-core/src/mapping.rs musefs-core/src/facade.rs musefs-core/src/lib.rs musefs-core/src/refresh_diff.rs
git commit -m "$(cat <<'EOF'
Render paths through the compiled Template; borrow field values (#137)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>
EOF
)"
```

### Task 4: Gates and PR

**Files:** none (verification only)

- [ ] **Step 1: Workspace tests**

Run: `cargo test`
Expected: all crates pass (FUSE e2e stays `#[ignore]`d).

- [ ] **Step 2: In-diff mutation gate (CI parity)**

Always `-j2`, output on `/tmp`, do NOT set `TMPDIR`. The `grep -q` guard matters: an empty diff mutates nothing and exits 0 — a silent false pass.

```bash
git diff "$(git merge-base main HEAD)...HEAD" -- '*.rs' > mutants.diff
grep -q '^@@ ' mutants.diff
cargo mutants --in-diff mutants.diff -j2 --exclude 'musefs-latencyfs/**' --output /tmp/mutants-out/in-diff
```

Expected: exit 0, no missed mutants. `parse`'s branch arms are pinned by the five template tests (including the lone-`$` and unterminated-`${` cases added for exactly this); `render`'s fallback chain by the fallback test; `sanitize_into` by the sanitization test; `tags_to_fields` by the mapping tests.

- [ ] **Step 3: Push and open the PR**

```bash
git push -u origin compiled-template
gh pr create --title "Compile the path template once; render without per-field allocations (#137)" --body "$(cat <<'EOF'
Closes #137.

Path rendering re-parsed the template char-by-char for every track on every
rebuild, allocated a fresh `String` per substituted field (even when nothing
needed sanitizing), and cloned every first tag value into a per-track
`BTreeMap<String, String>`.

Now: a `Template` (`Vec<Part>` of literal runs and field names) is parsed
once in `Musefs::open` and stored on `Musefs`; rendering appends sanitized
values directly into the output; `tags_to_fields` borrows values from the
tag rows. Per-track rendering cost is now one lowercased key `String` per
distinct tag key plus the output path itself. Parse semantics are preserved
exactly (lone `$` literal, unterminated `${`, `$field`/`${field}`), with new
tests pinning the edge cases.

`render_path` is removed; `musefs_core` re-exports `Template` instead (its
only consumers were `facade.rs` and the template integration tests).

Spec: `docs/superpowers/specs/2026-06-05-allocation-cleanups-design.md` (PR 2 section).

🤖 Generated with [Claude Code](https://claude.com/claude-code)
EOF
)"
```

Expected: PR URL printed; CI (`ci-ok`/`coverage-ok`) goes green.
