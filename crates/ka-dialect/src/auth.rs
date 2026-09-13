//! Token sources and the `.env` chain. Ladder: process env > `./.env` >
//! `~/.config/ka/.env` > OS keyring (`service "ka"`, user = the env var
//! name). `!command` indirection keeps secrets out of files. A missing or
//! unusable keyring backend silently falls through.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::LazyLock;

static DOTENV: LazyLock<std::sync::RwLock<HashMap<String, String>>> =
    LazyLock::new(|| std::sync::RwLock::new(scan_dotenv()));

/// Insert or update a key in the in-process dotenv layer (used by the TUI
/// key prompt so a saved key works without a restart).
pub fn set_dotenv_key(key: &str, value: &str) {
    if let Ok(mut map) = DOTENV.write() {
        map.insert(key.to_string(), value.to_string());
    }
}

/// Whether a key variable is present anywhere ka would read it: the
/// process environment, the dotenv layer (`./.env`, `~/.config/ka/.env`),
/// or the OS keyring. Reading the map forces the lazy scan if it has not
/// run yet, so a key saved to the dotenv before this process started
/// counts as set — matching what [`resolve_token`] would find.
pub fn key_is_set(env_var: &str) -> bool {
    let spec = env_var.trim();
    !spec.is_empty()
        && (std::env::var(spec).is_ok_and(|v| !v.is_empty())
            || DOTENV
                .read()
                .ok()
                .is_some_and(|map| map.get(spec).is_some_and(|v| !v.is_empty()))
            || keyring_is_set(spec))
}

/// Keyring service name ka stores credential entries under.
const KEYRING_SERVICE: &str = "ka";

/// Best-effort keyring lookup for an env-var-named entry. Any failure —
/// no backend, no entry, blank value — returns `None` so callers fall
/// through as if the layer did not exist.
fn keyring_lookup(env_var: &str) -> Option<String> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, env_var).ok()?;
    match entry.get_password() {
        Ok(v) if !v.is_empty() => Some(v),
        _ => None,
    }
}

/// Whether a keyring entry exists for the variable (best effort; a dead
/// backend counts as absent).
fn keyring_is_set(env_var: &str) -> bool {
    keyring_lookup(env_var).is_some()
}

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
    // shell-sourceable env files often carry `export KEY=...`
    let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
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

/// Resolve a token spec: an environment variable name (checked against
/// the process env first, then the lazily-scanned `.env` map, then the
/// OS keyring), or `!command` to run (trimmed stdout). The map never
/// overrides real variables.
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
    DOTENV
        .read()
        .ok()
        .and_then(|map| map.get(spec).cloned())
        .filter(|v| !v.is_empty())
        .or_else(|| keyring_lookup(spec))
}

/// Testable core of [`resolve_token`] against an explicit fallback map
/// (standing in for the dotenv layer).
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
    fallback
        .get(spec)
        .cloned()
        .filter(|v| !v.is_empty())
        .or_else(|| keyring_lookup(spec))
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
    fn key_is_set_sees_env_and_rejects_blank() {
        // HOME is present in every test environment (see the note above);
        // a var set in the process counts as set
        if std::env::var("HOME").is_ok() {
            assert!(key_is_set("HOME"));
        }
        // a unique name absent from env and from the developer's dotenv
        assert!(!key_is_set("KA_TEST_KEY_ABSENT_XYZ_9183"));
        // blank specs are never set
        assert!(!key_is_set(""));
        assert!(!key_is_set("   "));
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
        assert_eq!(
            parse_env_line("export OPENAI_API_KEY=sk-x"),
            Some(("OPENAI_API_KEY".into(), "sk-x".into()))
        );
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

    /// Skip guard for machines without a working keyring backend.
    fn seed_keyring(var: &str, secret: &str) -> bool {
        let Ok(entry) = keyring::Entry::new("ka", var) else {
            return false;
        };
        entry.set_password(secret).is_ok()
    }

    #[test]
    fn keyring_entry_resolves_as_last_layer() {
        let var = "KA_TEST_KEYRING_XYZ_1";
        if !seed_keyring(var, "from-keyring") {
            eprintln!("no usable keyring backend; skipping");
            return;
        }
        // no env, empty map → the keyring answers
        assert_eq!(
            resolve_token_with(&HashMap::new(), var).as_deref(),
            Some("from-keyring")
        );
        assert!(key_is_set(var));
        let _ = keyring::Entry::new("ka", var).unwrap().delete_credential();
    }

    #[test]
    fn fallback_map_beats_keyring() {
        let var = "KA_TEST_KEYRING_XYZ_2";
        if !seed_keyring(var, "from-keyring") {
            eprintln!("no usable keyring backend; skipping");
            return;
        }
        let fb = map(&[(var, "from-map")]);
        assert_eq!(
            resolve_token_with(&fb, var).as_deref(),
            Some("from-map"),
            "dotenv layer must shadow the keyring"
        );
        let _ = keyring::Entry::new("ka", var).unwrap().delete_credential();
    }

    #[test]
    fn keyring_absence_is_transparent() {
        // a variable with no env, no map and no entry stays unset and
        // unresolved even though the keyring layer is consulted
        assert!(!key_is_set("KA_TEST_KEYRING_ABSENT_XYZ_3"));
        assert!(resolve_token_with(&HashMap::new(), "KA_TEST_KEYRING_ABSENT_XYZ_3").is_none());
    }

    #[test]
    fn command_spec_failure_is_none() {
        assert!(
            resolve_token_with(&HashMap::new(), "!definitely-not-a-real-command-xyz").is_none()
        );
    }
}
