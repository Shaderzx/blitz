//! Dep-info parsing: the `.d` files rustc writes list every source file a unit read.
//!
//! rustc writes one rule per file (`output: prerequisite prerequisite ...`, wrapped over several
//! lines with backslash-escaped spaces) followed by `# env-dep:NAME=VALUE` comments for every
//! environment variable the unit actually read.

/// Every prerequisite of the first rule in `text`, unescaped. The rule's own target is skipped.
pub fn parse_prerequisites(text: &str) -> Vec<String> {
    let Some(body) = first_rule_body(text) else {
        return Vec::new();
    };
    split_escaped(body)
}

/// The `env-dep` comments a `.d` file records, in order: the variable name and the value seen at
/// compile time (`None` when the variable was unset).
pub fn parse_env_deps(text: &str) -> Vec<(String, Option<String>)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let Some(rest) = line.trim_start().strip_prefix("# env-dep:") else {
            continue;
        };
        // rustc writes the value verbatim; only a CRLF line ending is not part of it.
        let rest = rest.trim_end_matches(['\r', '\n']);
        let (name, value) = match rest.split_once('=') {
            Some((name, value)) => (name, Some(unescape(value))),
            None => (rest, None),
        };
        if !name.is_empty() {
            out.push((name.to_string(), value));
        }
    }
    out
}

/// The text after the first unescaped colon of the first rule, up to the blank line that ends it.
fn first_rule_body(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b':' if bytes.get(i + 1).is_none_or(u8::is_ascii_whitespace) => {
                let body = &text[i + 1..];
                let end = body.find("\n\n").unwrap_or(body.len());
                return Some(&body[..end]);
            }
            _ => i += 1,
        }
    }
    None
}

/// Splits a prerequisite list on unescaped whitespace.
fn split_escaped(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            c if c.is_whitespace() => {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(next) = chars.next() {
                    out.push(next);
                }
            }
            c => out.push(c),
        }
    }
    out
}
