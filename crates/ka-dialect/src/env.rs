//! Environment lookup ladder: process env first, then `.env` files in
//! precedence order (earlier files win). No `set_var` — edition 2024 keeps
//! that unsafe, and an explicit lookup is cleaner anyway.

use std::collections::HashMap;
use std::path::Path;

/// Ordered key/value bag consulted after the real environment.
#[derive(Debug, Clone, Default)]
pub struct EnvLookup {
    extra: HashMap<String, String>,
}

impl EnvLookup {
    /// Empty bag (process env only).
    pub fn empty() -> Self {
        Self::default()
    }

    /// Load `.env` files in precedence order (first file that defines a key
    /// wins). Missing files are skipped silently.
    pub fn from_files<I: AsRef<Path>>(paths: &[I]) -> Self {
        let mut extra = HashMap::new();
        for path in paths {
            let Ok(text) = std::fs::read_to_string(path) else {
                continue;
            };
            for line in text.lines() {
                if let Some((k, v)) = parse_env_line(line) {
                    extra.entry(k).or_insert(v);
                }
            }
        }
        Self { extra }
    }

    /// Resolve a variable: process env, then bag.
    pub fn get(&self, key: &str) -> Option<String> {
        std::env::var(key)
            .ok()
            .filter(|v| !v.is_empty())
            .or_else(|| self.extra.get(key).cloned())
    }
}

/// `KEY=VALUE`, skipping blanks/comments; strips matching quotes.
fn parse_env_line(line: &str) -> Option<(String, String)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let (k, v) = line.split_once('=')?;
    let k = k.trim();
    if k.is_empty() {
        return None;
    }
    let mut v = v.trim().to_string();
    if v.len() >= 2
        && ((v.starts_with('"') && v.ends_with('"')) || (v.starts_with('\'') && v.ends_with('\'')))
    {
        v = v[1..v.len() - 1].to_string();
    }
    Some((k.to_string(), v))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn parses_quotes_and_skips_noise() {
        assert_eq!(parse_env_line("A=b"), Some(("A".into(), "b".into())));
        assert_eq!(
            parse_env_line("  Q = \"x y\" "),
            Some(("Q".into(), "x y".into()))
        );
        assert_eq!(parse_env_line("# comment"), None);
        assert_eq!(parse_env_line(""), None);
        assert_eq!(parse_env_line("NOVALUE"), None);
    }

    #[test]
    fn first_file_wins_for_duplicate_keys() {
        let dir = std::env::temp_dir().join(format!("ka-env-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.env");
        let b = dir.join("b.env");
        std::fs::write(&a, "KA_TEST_DUP=first\nKA_TEST_ONLY=a\n").unwrap();
        std::fs::write(&b, "KA_TEST_DUP=second\n").unwrap();
        let env = EnvLookup::from_files(&[&a, &b]);
        assert_eq!(env.get("KA_TEST_DUP").as_deref(), Some("first"));
        assert_eq!(env.get("KA_TEST_ONLY").as_deref(), Some("a"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_files_are_silent() {
        let env = EnvLookup::from_files(&["/nonexistent/ka/env-test/.env"]);
        assert_eq!(env.get("KA_TEST_MISSING"), None);
    }
}
