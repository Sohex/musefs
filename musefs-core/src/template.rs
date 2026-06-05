use std::collections::BTreeMap;

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

/// Replace '/' and control characters in a substituted field value so it can be a
/// single path component. The template's own '/' separators are not passed through here.
fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c == '/' || (c as u32) < 0x20 {
                '_'
            } else {
                c
            }
        })
        .collect()
}

fn is_field_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

fn resolve<'a>(
    name: &str,
    fields: &'a BTreeMap<String, String>,
    fallbacks: &'a BTreeMap<String, String>,
    default_fallback: &'a str,
) -> String {
    if let Some(v) = fields.get(name) {
        sanitize(v)
    } else if let Some(v) = fallbacks.get(name) {
        sanitize(v)
    } else {
        sanitize(default_fallback)
    }
}

/// Render a path template. `$field` and `${field}` are replaced with the field's
/// value (sanitized to a single path component). Missing fields use a per-field
/// fallback if present, otherwise `default_fallback`. The extension is appended.
pub fn render_path(
    template: &str,
    fields: &BTreeMap<String, String>,
    fallbacks: &BTreeMap<String, String>,
    default_fallback: &str,
    ext: &str,
) -> String {
    let mut out = String::new();
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
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
                out.push_str(&resolve(&name, fields, fallbacks, default_fallback));
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
                out.push_str(&resolve(&name, fields, fallbacks, default_fallback));
            }
            _ => out.push('$'), // a literal '$' not followed by a field name
        }
    }
    out.push('.');
    out.push_str(ext);
    out
}
