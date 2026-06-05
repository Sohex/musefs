# Allocation cleanups: synthesis inputs (#131) and path rendering (#137)

**Date:** 2026-06-05
**Issues:** #131 — `TagInput`/`ArtInput` hold owned `String`s, forcing
per-resolve allocations; #137 — path rendering allocates per component on
every rebuild
**Status:** Approved

One spec, two independent PRs (the repo's one-issue-per-PR pattern). Both
are allocator-pressure cleanups, not measured bottlenecks — each issue
admits the win is likely masked by DB I/O, and since #150 header resolves
only happen on cache miss. No new benches; the wins are by construction.

## PR 1 — closes #131: move tag strings into synthesis inputs

### Problem

`tags_to_inputs` (`musefs-core/src/mapping.rs`) clones every tag key/value
out of the already-owned `Vec<Tag>` DB rows into `TagInput`s — a pure
second copy on every header resolve. The rows have no other consumer:
`HeaderCache::resolve` (`reader.rs`) fetches `tags` solely to feed
`tags_to_inputs`.

The `ArtInput` half of the issue is already moot: `track_art_to_inputs`
**moves** `meta.mime` and `ta.description` out of its locally fetched rows.
No change there; the PR description should note this.

### Design

Take the rows by value and move the strings:

```rust
pub(crate) fn tags_to_inputs(tags: Vec<Tag>) -> Vec<TagInput> {
    tags.into_iter()
        .map(|t| TagInput { key: t.key, value: t.value })
        .collect()
}
```

The caller becomes `let inputs = tags_to_inputs(db.get_tags(track.id)?);`.
The `Tag` rows' `id`/`track_id`/`ordinal` are dropped; key/value strings
are moved, never copied. The only remaining allocation is the unavoidable
SQLite-row materialization.

Deliberately NOT done: borrowed `TagInput<'a>`/`Cow` fields (what the issue
text implies). Borrowing saves zero further allocations — the DB rows must
be owned either way — while rippling through five format modules, ~11
format test files, and `fuzz/src/lib.rs::arb_tags`/`arb_arts` (which build
inputs from local `String`s and would not compile with plain borrows; the
fuzz crate is out-of-workspace, so breakage surfaces only in CI's smoke
job).

### Scope

`musefs-core` only: `mapping.rs` (signature + its unit tests), `reader.rs`
(call site). `TagInput`, `ArtInput`, all `musefs-format` signatures, format
tests, and the fuzz crate are untouched. `TagInput::new(&str, &str)` stays
as the allocating convenience constructor for format tests and fuzz
helpers.

## PR 2 — closes #137: compile-once template, allocation-free rendering

### Problem

Per track, per rebuild (`Musefs::render_one`, `facade.rs`):

1. `render_path` (`musefs-core/src/template.rs`) re-parses the template
   char-by-char and accumulates each field name into a fresh `String` —
   though the template is constant for the life of the mount;
2. `resolve`/`sanitize` allocate a new `String` per substituted field even
   when no character needs replacing;
3. `tags_to_fields` (`mapping.rs`) clones every first tag value into a
   fresh `BTreeMap<String, String>`.

### Design

**Compile-once `Template` type** in `template.rs`, replacing the
`render_path` free function:

```rust
pub struct Template { parts: Vec<Part> }
enum Part { Literal(String), Field(String) }

impl Template {
    pub fn parse(template: &str) -> Template  // infallible
    pub fn render(
        &self,
        fields: &BTreeMap<String, &str>,
        fallbacks: &BTreeMap<String, String>,
        default_fallback: &str,
        ext: &str,
    ) -> String
}
```

`parse` reuses the existing char-walk once, preserving current semantics
exactly: `$field` / `${field}` substitution, a lone `$` (not followed by a
field char or `{`) is a literal, an unterminated `${` consumes the rest of
the template as the field name. Infallible, like today.

`render` walks the parts: literals are `push_str`'d; fields resolve through
fields → per-field fallback → `default_fallback` and are sanitized directly
into the output via `sanitize_into(&mut out, value)` (replacing `/` and
control chars `< 0x20` with `_`) — no intermediate per-field `String`.
`.ext` is appended as today.

**Borrowed field values:** `tags_to_fields(tags: &[Tag]) ->
BTreeMap<String, &str>` — keys still lowercased (necessarily allocated;
stored tag keys are rarely already lowercase), values borrow from the tag
rows. First-value-per-key (lowest ordinal) semantics unchanged.

**Wiring:** `MountConfig.template` stays a `String` (CLI-facing config).
`Musefs::open` compiles it once into a `template: Template` field on
`Musefs`; `render_one` and the static rebuild helpers take `&Template`
alongside the config. Both rebuild paths — full (`render_entries`/
`build_full`) and incremental (the changelog path) — already funnel through
`render_one`, so the swap is contained.

**Public API:** `render_path` is removed; `lib.rs` re-exports `Template`
instead. Its only consumers are `facade.rs` and
`musefs-core/tests/template.rs` — no back-compat wrapper.

Net per-track rendering cost after: one lowercased key `String` per
distinct tag key plus the output path `String`. Gone: per-track template
re-parse, per-field name and sanitize `String`s, per-value clones.

### Scope

`musefs-core` only: `template.rs` (rewrite around `Template`),
`mapping.rs` (`tags_to_fields` signature + unit tests), `facade.rs`
(`Musefs` field, `render_one` and rebuild-helper signatures), `lib.rs`
(re-export), `tests/template.rs` (rewritten against
`Template::parse(...).render(...)`).

## Testing

- `tests/template.rs`: same behavioral assertions (substitution, fallback
  chain, sanitization) rewritten against `Template`, plus explicit
  literal-`$` and unterminated-`${` cases if not already covered — the
  parse/render split adds branches the mutation gate will probe.
- `mapping.rs` unit tests updated for the new signatures; existing
  facade/reader tests guard the wiring end to end. FUSE e2e unaffected.
- Both PRs run the standard gates: `cargo fmt --all --check`,
  `cargo clippy --all-targets` (catches the benches/tests API consumers),
  workspace tests, and the in-diff mutation gate (`-j2`, output on `/tmp`,
  non-empty-diff sanity check first).

## Out of scope

- Borrowed/`Cow` synthesis input structs (rejected above).
- `BinaryTagInput` (fetches its own rows internally; already move-built).
- Pre-sizing the rendered path `String` or interning field names.
- New benchmarks or BENCHMARKS.md entries.
