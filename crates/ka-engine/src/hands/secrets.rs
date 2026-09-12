//! One-way secret redaction in tool results: values of environment
//! variables with secret-ish names are replaced before anything reaches
//! the model. Best-effort, conservative (name-gated, length-gated).

use std::sync::LazyLock;

static SECRETS: LazyLock<Vec<(String, String)>> = LazyLock::new(|| {
    let mut found = Vec::new();
    for (name, value) in std::env::vars() {
        let looks_secret = [
            "KEY",
            "SECRET",
            "TOKEN",
            "PASSWORD",
            "PASSWD",
            "AUTH",
            "CREDENTIAL",
            "PRIVATE",
        ]
        .iter()
        .any(|m| name.to_uppercase().contains(m));
        if looks_secret && value.len() >= 8 && !value.contains(' ') {
            found.push((name, value));
        }
    }
    found
});

/// Redact known secret values inside `text`, replacing each occurrence
/// with `[redacted:NAME]`. Values are matched longest-first so overlapping
/// prefixes don't leave fragments.
pub fn redact(text: &str) -> String {
    redact_from(text, &SECRETS)
}

/// Redaction core against explicit pairs (testable without env mutation).
pub fn redact_from(text: &str, pairs: &[(String, String)]) -> String {
    let mut out = text.to_string();
    let mut sorted = pairs.to_vec();
    sorted.sort_by_key(|(_, v)| std::cmp::Reverse(v.len()));
    for (name, value) in sorted {
        if out.contains(&value) {
            out = out.replace(&value, &format!("[redacted:{name}]"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn redacts_injected_secret() {
        // pure-function shape: redact_from with explicit pairs
        let text = "curl -H 'Authorization: Bearer sk-supersecret-123456' http://x";
        let pairs = vec![(
            "KA_TEST_API_KEY_XYZ".to_string(),
            "sk-supersecret-123456".to_string(),
        )];
        let out = redact_from(text, &pairs);
        assert!(out.contains("[redacted:KA_TEST_API_KEY_XYZ]"), "{out}");
        assert!(!out.contains("sk-supersecret"), "{out}");
    }

    #[test]
    fn ignores_benign_values() {
        assert_eq!(
            redact("plain text with no secrets"),
            "plain text with no secrets"
        );
    }

    #[test]
    fn longest_first_ordering() {
        let pairs = vec![
            ("K_A".to_string(), "prefix".to_string()),
            ("K_B".to_string(), "prefix-longer".to_string()),
        ];
        let out = redact_from("has prefix-longer inside", &pairs);
        assert!(out.contains("[redacted:K_B]"), "{out}");
        assert!(!out.contains("prefix"), "{out}");
    }
}
