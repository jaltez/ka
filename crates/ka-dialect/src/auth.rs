//! Token sources and the `.env` chain. Ladder: process env > `./.env` >
//! `~/.config/ka/.env`. `!command` indirection keeps secrets out of files.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::LazyLock;

static DOTENV: LazyLock<HashMap<String, String>> = LazyLock::new(scan_dotenv);

fn scan_dotenv() -> HashMap<String, String> {
    let mut map = HashMap::new();
    let mut candidates: Vec<PathBuf> = vec![PathBuf::from(".env")];
    if let Ok(home) = std::env::var("HOME") {
        candidates.push(PathBuf::from(home).join(".config/ka/.env"));
    }
    for path in candidates {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        for line in text.lines() {
            if let Some((k, v)) = parse_env_line(line) {
                map.entry(k).or_insert(v);
            }
        }
    }
    map
}

fn parse_env_line(line: &str) -> Option<(String, String)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let (key, value) = line.split_once('=')?;
    let key = key.trim().to_string();
    if key.is_empty() {
        return None;
    }
    let value = value.trim();
    let unquoted = value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
        .unwrap_or(value);
    Some((key, unquoted.to_string()))
}

/// Resolve a token spec: an environment variable name (checked against the
/// process env first, then the lazily-scanned `.env` map), or `!command` to
/// run (trimmed stdout). The map never overrides real variables.
pub fn resolve_token(spec: &str) -> Option<String> {
    let spec = spec.trim();
    if let Some(cmd) = spec.strip_prefix('!') {
        return run_command(cmd);
    }
    if let Ok(v) = std::env::var(spec) {
        if !v.is_empty() {
            return Some(v);
        }
    }
    DOTENV.get(spec).cloned().filter(|v| !v.is_empty())
}

/// Testable core of [`resolve_token`] against an explicit fallback map.
pub fn resolve_token_with(fallback: &HashMap<String, String>, spec: &str) -> Option<String> {
    let spec = spec.trim();
    if let Some(cmd) = spec.strip_prefix('!') {
        return run_command(cmd);
    }
    if let Ok(v) = std::env::var(spec) {
        if !v.is_empty() {
            return Some(v);
        }
    }
    fallback.get(spec).cloned().filter(|v| !v.is_empty())
}

fn run_command(cmd: &str) -> Option<String> {
    let mut parts = cmd.split_whitespace();
    let program = parts.next()?;
    let output = std::process::Command::new(program)
        .args(parts)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!stdout.is_empty()).then_some(stdout)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn dotenv_lines_parse() {
        assert_eq!(parse_env_line("A=1"), Some(("A".into(), "1".into())));
        assert_eq!(
            parse_env_line(" B = \"two\" "),
            Some(("B".into(), "two".into()))
        );
        assert_eq!(parse_env_line("# comment"), None);
        assert_eq!(parse_env_line("noequals"), None);
        assert_eq!(parse_env_line("=value"), None);
    }

    #[test]
    fn process_env_beats_fallback_map() {
        // HOME is present in every test environment; the fallback map must
        // lose to the real variable without us mutating the environment
        // (set_var is unsafe in edition 2024).
        let fb = map(&[("HOME", "from-file")]);
        let resolved = resolve_token_with(&fb, "HOME");
        let real = std::env::var("HOME").ok();
        if real.is_some() {
            assert_eq!(resolved, real);
        } else {
            assert_eq!(resolved.as_deref(), Some("from-file"));
        }
    }

    #[test]
    fn fallback_map_used_when_env_missing() {
        let fb = map(&[("KA_TEST_TOK_FILE_XYZ", "from-file")]);
        assert_eq!(
            resolve_token_with(&fb, "KA_TEST_TOK_FILE_XYZ").as_deref(),
            Some("from-file")
        );
    }

    #[test]
    fn missing_is_none() {
        assert!(resolve_token_with(&HashMap::new(), "KA_TEST_TOK_NONE_XYZ").is_none());
        assert!(resolve_token_with(&HashMap::new(), "").is_none());
    }

    #[test]
    fn command_spec_runs() {
        assert_eq!(
            resolve_token_with(&HashMap::new(), "!echo ka-test-token").unwrap(),
            "ka-test-token"
        );
    }

    #[test]
    fn command_spec_failure_is_none() {
        assert!(
            resolve_token_with(&HashMap::new(), "!definitely-not-a-real-command-xyz").is_none()
        );
    }
}
