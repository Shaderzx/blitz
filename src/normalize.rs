//! Path normalization for cache keys.
//!
//! Absolute paths differ between checkouts, machines and target directories, so every path that
//! takes part in a key, a stored output or a `.d` file is first rewritten to a placeholder. Roots
//! are matched longest first, so a target directory inside the checkout becomes `$TARGET` and not
//! `$CWD/target`. Two spellings of the same directory (the path as cargo wrote it and its
//! canonical form, which differ under `/tmp` on macOS) both map to the same placeholder.

use std::path::Path;

pub const TARGET: &str = "$TARGET";
pub const CWD: &str = "$CWD";
pub const MANIFEST: &str = "$MANIFEST";
pub const CARGO_HOME: &str = "$CARGO_HOME";

pub struct Normalizer {
    /// (path as found in the wild, placeholder), longest path first.
    roots: Vec<(String, &'static str)>,
    /// Placeholder -> the spelling to write back out; the first spelling added wins, and that is
    /// the one cargo itself passed, so writing there always lands in the right directory.
    preferred: Vec<(&'static str, String)>,
}

impl Normalizer {
    pub fn new() -> Normalizer {
        Normalizer {
            roots: Vec::new(),
            preferred: Vec::new(),
        }
    }

    /// Rewrites every occurrence of `path` to `placeholder`.
    pub fn add(&mut self, path: &Path, placeholder: &'static str) {
        let forms = variants(path);
        if let Some(first) = forms.first() {
            if !self.preferred.iter().any(|(p, _)| *p == placeholder) {
                self.preferred.push((placeholder, first.clone()));
            }
        }
        for form in forms {
            if form.is_empty() || self.roots.iter().any(|(r, _)| *r == form) {
                continue;
            }
            self.roots.push((form, placeholder));
        }
        self.roots
            .sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.1.cmp(b.1)));
    }

    pub fn normalize(&self, text: &str) -> String {
        let mut out = text.to_string();
        for (real, placeholder) in &self.roots {
            out = replace_at_boundary(&out, real, placeholder);
        }
        out
    }

    pub fn denormalize(&self, text: &str) -> String {
        let mut out = text.to_string();
        for (placeholder, real) in &self.preferred {
            out = replace_at_boundary(&out, placeholder, real);
        }
        out
    }
}

/// Replaces every occurrence of `from` that ends at a path boundary. A root only names a directory:
/// rewriting it inside a longer name would invent a path that was never there, so `/w/target-old`
/// keeps its name under a root of `/w/target` while `/w/target/debug` becomes `$TARGET/debug`.
fn replace_at_boundary(text: &str, from: &str, to: &str) -> String {
    if from.is_empty() || !text.contains(from) {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find(from) {
        let after = &rest[i + from.len()..];
        out.push_str(&rest[..i]);
        out.push_str(if ends_segment(after) { to } else { from });
        rest = after;
    }
    out.push_str(rest);
    out
}

/// True when `after` begins a new path segment: nothing at all, a separator, or a character that no
/// file name contains (a space, a quote, a colon in a dep-info rule).
fn ends_segment(after: &str) -> bool {
    match after.chars().next() {
        None => true,
        Some(c) if c == '/' || c == '\\' => true,
        Some(c) => !(c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '+' | '~' | '@')),
    }
}

/// The path as given, plus its canonical form when the two differ.
pub fn variants(path: &Path) -> Vec<String> {
    let given = path.to_string_lossy().into_owned();
    if given.is_empty() {
        return Vec::new();
    }
    let mut forms = vec![given.clone()];
    if let Ok(canonical) = std::fs::canonicalize(path) {
        let canonical = canonical.to_string_lossy().into_owned();
        if canonical != given {
            forms.push(canonical);
        }
    }
    forms
}
