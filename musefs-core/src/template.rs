use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::iter::Peekable;
use std::str::Chars;

/// Max surviving segments a single `$!{}` path field may expand into. A hostile
/// 256 KiB tag shaped `a/a/a/...` would otherwise build tens of thousands of
/// directory levels (depth amplification across the DB trust boundary, #303).
const MAX_PATH_FIELD_SEGMENTS: usize = 64;

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
    Field {
        names: Vec<String>,
        raw: bool,
    },
    /// A `[...]` conditional section: emitted only if at least one field
    /// referenced within it (transitively) is present.
    Section(Vec<Part>),
}

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

    /// The set of field names this template references, across plain fields, `$!{}`
    /// path fields, `|` fallback chains, and `[...]` sections. Names are already
    /// ASCII-lowercased at parse time, matching `tags_to_fields`'s key folding, so a
    /// key-filtered tag load (`Db::tags_grouped_for_keys`) fetches exactly what
    /// rendering consumes.
    pub fn referenced_fields(&self) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        collect_field_names(&self.parts, &mut out);
        out
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
    content.split('|').map(str::to_ascii_lowercase).collect()
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

fn collect_field_names(parts: &[Part], out: &mut BTreeSet<String>) {
    for part in parts {
        match part {
            Part::Literal(_) => {}
            Part::Field { names, .. } => {
                for name in names {
                    out.insert(name.clone());
                }
            }
            Part::Section(inner) => collect_field_names(inner, out),
        }
    }
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

fn is_field_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn referenced_fields_collects_plain_path_section_and_fallback_names() {
        let t = Template::parse("$artist/$!{beets_path}/[$disc - ]${title|name}");
        let f = t.referenced_fields();
        assert!(f.contains("artist"));
        assert!(f.contains("beets_path"));
        assert!(f.contains("disc"));
        assert!(f.contains("title"));
        assert!(f.contains("name"));
        // No spurious entries from literals.
        assert_eq!(f.len(), 5);
    }
}
