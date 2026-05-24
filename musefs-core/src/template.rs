use std::collections::BTreeMap;

/// Replace '/' and control characters in a substituted field value so it can be a
/// single path component. The template's own '/' separators are not passed through here.
fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|c| if c == '/' || (c as u32) < 0x20 { '_' } else { c })
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
