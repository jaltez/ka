//! `ka import claude` (roadmap 9.3): one-shot conversion of Claude
//! Code's `settings.json` permission rules into ka `[[rules]]` TOML.
//! Explicit and auditable — ka never live-reads another agent's config;
//! the conversion prints everything it will do (and everything it
//! skipped, with reasons), asks before writing, and validates the
//! final file through the strict-TOML parser before it lands.
//!
//! Semantics notes: claude evaluates deny-anywhere-beats-allow; ka is
//! first-match-wins — so imported rules are emitted deny → ask → allow
//! in that order, preserving the claude outcome. Because rules are
//! APPENDED, any pre-existing [[rules]] entries in the target file
//! still win over the imported ones (a WARNING is printed; the user's
//! existing rules are never reordered or rewritten). Untranslatable
//! entries (tools ka does not have) and restrictions that cannot be
//! carried (mcp tool patterns) are reported, never dropped silently.

use std::io::IsTerminal;
use std::path::PathBuf;

use ka_engine::config::{Config, Rule, Verdict};

/// Where the converted rules land.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layer {
    /// `<project root>/.ka/ka.toml` (nearest .git ancestor, else cwd).
    Project,
    /// `~/.config/ka/ka.toml`.
    User,
}

impl Layer {
    fn label(self) -> &'static str {
        match self {
            Layer::Project => "project",
            Layer::User => "user",
        }
    }
}

/// Run `ka import <format> <path>`. `format` is `claude` today.
pub fn run(
    format: &str,
    path: &std::path::Path,
    layer: Option<Layer>,
    dry_run: bool,
) -> Result<std::process::ExitCode, String> {
    if format != "claude" {
        return Err(format!(
            "unknown import format {format:?} (supported: claude; codex/gemini later)"
        ));
    }
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let conversion = convert_claude(&text)?;
    print_conversion(&conversion);
    if conversion.rules.is_empty() {
        println!("nothing to import");
        return Ok(std::process::ExitCode::SUCCESS);
    }
    if dry_run {
        println!("\n(dry run — nothing written)");
        return Ok(std::process::ExitCode::SUCCESS);
    }
    let layer = match layer {
        Some(l) => l,
        None => prompt_layer()?,
    };
    let target = match layer {
        Layer::Project => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            ka_engine::project_root(&cwd).join(".ka/ka.toml")
        }
        Layer::User => ka_engine::config::user_config_path(),
    };
    write_rules(&target, &conversion)?;
    println!(
        "\nwrote {} rule(s) → the {} layer ({})",
        conversion.rules.len(),
        layer.label(),
        target.display()
    );
    Ok(std::process::ExitCode::SUCCESS)
}

/// A conversion result: the rules to append + the entries that were
/// skipped (entry, reason) so nothing disappears silently.
pub struct Conversion {
    pub rules: Vec<Rule>,
    pub skipped: Vec<(String, String)>,
}

/// Parse a Claude Code settings.json and convert its permission lists.
fn convert_claude(text: &str) -> Result<Conversion, String> {
    let v: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("invalid JSON: {e}"))?;
    let perms = v
        .get("permissions")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let lists = [
        ("allow", Verdict::Allow),
        ("ask", Verdict::Ask),
        ("deny", Verdict::Deny),
    ];
    // claude: deny anywhere beats allow everywhere; ka: first match
    // wins — emit deny first, then ask, then allow to preserve outcomes
    let ordered = [lists[2], lists[1], lists[0]];
    let mut rules = Vec::new();
    let mut skipped = Vec::new();
    for (list, verdict) in ordered {
        let Some(entries) = perms.get(list).and_then(|l| l.as_array()).map(|a| {
            a.iter()
                .filter_map(|e| e.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        }) else {
            continue;
        };
        for entry in entries {
            match map_entry(&entry, verdict) {
                Ok(rule) => rules.push(rule),
                Err(reason) => skipped.push((entry.clone(), reason)),
            }
        }
    }
    Ok(Conversion { rules, skipped })
}

/// Map one claude permission entry to a ka rule; `Err` carries the
/// skip reason (reported, never dropped silently).
fn map_entry(entry: &str, verdict: Verdict) -> Result<Rule, String> {
    let (tool_expr, pattern) = match entry.split_once('(') {
        Some((tool, rest)) => {
            let inner = rest.strip_suffix(')').unwrap_or(rest);
            (tool.to_string(), Some(inner.to_string()))
        }
        None => (entry.to_string(), None),
    };
    let rule_for = |tool: &str, pattern: Option<String>| {
        Ok(Rule {
            tool: tool.to_string(),
            pattern,
            verdict,
        })
    };
    match tool_expr.as_str() {
        // Bash(pattern): claude `prefix:*` is prefix-match → ka `prefix*`;
        // other globs pass through as ka globs
        "Bash" => {
            let pattern =
                pattern.map(|p| p.trim().trim_matches('"').trim_matches('\'').to_string());
            let pattern = pattern.map(|p| {
                if let Some(prefix) = p.strip_suffix(":*") {
                    // claude `git commit:*` = prefix match → ka `git commit*`
                    format!("{prefix}*")
                } else if let Some(exact) = p.strip_suffix(':') {
                    // `git commit:` = the bare prefix itself
                    exact.to_string()
                } else {
                    p
                }
            });
            let pattern = pattern.filter(|p| !p.is_empty());
            rule_for("bash", pattern)
        }
        "Edit" => rule_for("edit", clean_pattern(pattern)),
        "Write" => rule_for("write", clean_pattern(pattern)),
        "Read" => rule_for("read", clean_pattern(pattern)),
        "WebFetch" => {
            // claude `WebFetch(domain:example.com)` → ka's web_fetch
            // domain semantics take the bare host
            let pattern = pattern.map(|p| {
                p.trim()
                    .strip_prefix("domain:")
                    .unwrap_or(p.trim())
                    .to_string()
            });
            let pattern = pattern.filter(|p| !p.is_empty());
            rule_for("web_fetch", pattern)
        }
        "WebSearch" => rule_for("web_search", clean_pattern(pattern)),
        // mcp__server__tool → ka's `<server>.<tool>` hand names; ka
        // mcp rules carry no pattern, so a claude restriction like
        // mcp__github__create_issue(args) is reported as skipped
        // rather than silently dropping the restriction
        other if other.starts_with("mcp__") => {
            let parts: Vec<&str> = other.trim_start_matches("mcp__").splitn(2, "__").collect();
            if parts.len() == 2 {
                if let Some(p) = clean_pattern(pattern) {
                    Err(format!(
                        "ka mcp rules carry no pattern — {other:?} loses its ({p}) restriction"
                    ))
                } else {
                    rule_for(&format!("{}.{}", parts[0], parts[1]), None)
                }
            } else {
                Err(format!("unrecognized MCP tool form {entry:?}"))
            }
        }
        // tools ka does not carry (Task, Grep, Glob, ...) or unknown
        other => Err(format!("no ka counterpart for {other:?}")),
    }
}

fn clean_pattern(pattern: Option<String>) -> Option<String> {
    pattern
        .map(|p| p.trim().trim_matches('"').trim_matches('\'').to_string())
        .filter(|p| !p.is_empty())
}

/// Print the conversion: every rule as it will land, then skips.
fn print_conversion(c: &Conversion) {
    println!("converted rules (emitted deny → ask → allow; first match wins):");
    for r in &c.rules {
        match &r.pattern {
            Some(p) => println!("  {}({}) = {:?}", r.tool, p, verdict_str(r.verdict)),
            None => println!("  {} = {:?}", r.tool, verdict_str(r.verdict)),
        }
    }
    if !c.skipped.is_empty() {
        println!("\nskipped (no ka counterpart — reported, never dropped silently):");
        for (entry, reason) in &c.skipped {
            println!("  {entry} — {reason}");
        }
    }
}

fn verdict_str(v: Verdict) -> &'static str {
    match v {
        Verdict::Allow => "allow",
        Verdict::Ask => "ask",
        Verdict::Deny => "deny",
    }
}

/// Interactively choose the target layer (TTY only; non-TTY callers
/// must pass --project/--user).
fn prompt_layer() -> Result<Layer, String> {
    if !std::io::stdin().is_terminal() {
        return Err("choose a layer with --project or --user (no TTY to ask)".to_string());
    }
    loop {
        print!("write to [p]roject .ka/ka.toml, [u]ser config, or [n]othing? ");
        use std::io::Write;
        std::io::stdout().flush().map_err(|e| e.to_string())?;
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .map_err(|e| format!("stdin: {e}"))?;
        match line.trim() {
            "p" | "project" => return Ok(Layer::Project),
            "u" | "user" => return Ok(Layer::User),
            "n" | "no" | "" => return Err("aborted — nothing written".to_string()),
            _ => continue,
        }
    }
}

/// Append the rules to `target`, preserving existing content, and
/// re-validate the whole file through the strict parser before the
/// write lands (a file that would not parse is never written).
fn write_rules(target: &std::path::Path, conversion: &Conversion) -> Result<(), String> {
    let existing = std::fs::read_to_string(target).unwrap_or_default();
    if existing.contains("[[rules]]") {
        // ka is first-match-wins and these rules are appended, so any
        // pre-existing [[rules]] entry beats them. Warn; never reorder
        // or rewrite the user's existing rules.
        println!(
            "\nWARNING: {target} already has [[rules]] entries; ka is first-match-wins, so your existing rules win over the appended ones",
            target = target.display()
        );
    }
    let mut text = existing.clone();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str("\n# imported from claude code settings.json (ka import claude)\n");
    for r in &conversion.rules {
        text.push_str("\n[[rules]]\n");
        text.push_str(&format!("tool = {}\n", toml_quote(&r.tool)));
        if let Some(p) = &r.pattern {
            text.push_str(&format!("pattern = {}\n", toml_quote(p)));
        }
        text.push_str(&format!("verdict = \"{}\"\n", verdict_str(r.verdict)));
    }
    // strict validation before writing: the merged file must parse
    Config::parse_layer(&text, &target.display().to_string()).map_err(|e| e.to_string())?;
    if let Some(dir) = target.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
    }
    std::fs::write(target, &text).map_err(|e| format!("write {}: {e}", target.display()))
}

fn toml_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    const SAMPLE: &str = r#"{
      "permissions": {
        "allow": [
          "Bash(npm run *)",
          "Bash(git commit:*)",
          "Edit(src/**)",
          "Read(./.env)",
          "WebFetch(domain:example.com)",
          "mcp__github__create_issue",
          "Task"
        ],
        "ask": ["Bash(cargo publish*)"],
        "deny": ["Read(./secrets/*)", "Bash(rm -rf *)"]
      }
    }"#;

    #[test]
    fn converts_rules_preserving_claude_precedence() {
        let c = convert_claude(SAMPLE).unwrap();
        // deny first, then ask, then allow
        let order: Vec<Verdict> = c.rules.iter().map(|r| r.verdict).collect();
        let first_allow = order.iter().position(|v| *v == Verdict::Allow).unwrap();
        let last_deny = order.iter().rposition(|v| *v == Verdict::Deny).unwrap();
        let ask_pos = order.iter().position(|v| *v == Verdict::Ask).unwrap();
        assert!(last_deny < ask_pos && ask_pos < first_allow, "{order:?}");
        // pattern mapping: prefix:* → prefix*, domain: passthrough
        let find = |tool: &str, pat: Option<&str>| {
            c.rules
                .iter()
                .find(|r| r.tool == tool && r.pattern.as_deref() == pat)
        };
        assert!(find("bash", Some("git commit*")).is_some(), "{:?}", c.rules);
        assert!(find("bash", Some("npm run *")).is_some());
        assert!(find("bash", Some("rm -rf *")).is_some());
        assert!(find("edit", Some("src/**")).is_some());
        assert!(find("read", Some("./secrets/*")).is_some());
        assert!(find("web_fetch", Some("example.com")).is_some());
        assert!(find("github.create_issue", None).is_some());
        // untranslatable reported, not dropped
        assert!(
            c.skipped.iter().any(|(e, _)| e == "Task"),
            "{:?}",
            c.skipped
        );
    }

    #[test]
    fn writes_strict_validating_layer_and_preserves_existing() {
        let dir = std::env::temp_dir().join(format!("ka-import-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("ka.toml");
        std::fs::write(&target, "model = \"a/x\"\n").unwrap();
        let c = convert_claude(SAMPLE).unwrap();
        write_rules(&target, &c).unwrap();
        let text = std::fs::read_to_string(&target).unwrap();
        // existing keys survive, new rules parse back
        let layer = Config::parse_layer(&text, "check").unwrap();
        assert_eq!(layer.model.as_deref(), Some("a/x"));
        assert!(layer.rules.len() >= 8, "{}", layer.rules.len());
        assert!(text.contains("# imported from claude code settings.json"));
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn mcp_pattern_restrictions_are_reported_not_dropped() {
        let c = convert_claude(
            r#"{"permissions": {"allow": [
                "mcp__github__create_issue",
                "mcp__github__list_issues(repo:acme/*)"
            ]}}"#,
        )
        .unwrap();
        // the bare tool converts; the restricted one is skipped with the
        // dropped restriction named — never silently converted
        assert!(c.rules.iter().any(|r| r.tool == "github.create_issue"));
        let skipped: Vec<_> = c.skipped.iter().map(|(e, _)| e.as_str()).collect();
        assert!(
            skipped.contains(&"mcp__github__list_issues(repo:acme/*)"),
            "{skipped:?}"
        );
        assert!(
            c.skipped
                .iter()
                .any(|(e, r)| e.contains("list_issues") && r.contains("acme/*")),
            "{:?}",
            c.skipped
        );
    }

    #[test]
    fn appending_after_existing_rules_warns_and_keeps_them_first() {
        let dir = std::env::temp_dir().join(format!("ka-import-warn-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("ka.toml");
        // a pre-existing allow rule: first-match-wins beats the appended deny
        std::fs::write(
            &target,
            "[[rules]]\ntool = \"bash\"\npattern = \"rm *\"\nverdict = \"allow\"\n",
        )
        .unwrap();
        let c = convert_claude(SAMPLE).unwrap();
        write_rules(&target, &c).unwrap();
        let text = std::fs::read_to_string(&target).unwrap();
        // the user's rule stays ahead of the appended block
        let existing = text.find("pattern = \"rm *\"").unwrap();
        let appended = text.find("# imported from claude").unwrap();
        assert!(existing < appended, "{text}");
        let layer = Config::parse_layer(&text, "check").unwrap();
        assert!(layer.rules.len() >= 9);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn import_rejects_unknown_formats_and_bad_json() {
        assert!(run("codex", std::path::Path::new("/nonexistent"), None, true).is_err());
        assert!(convert_claude("{nope").is_err());
        // empty settings: nothing to do, no error
        let c = convert_claude("{}").unwrap();
        assert!(c.rules.is_empty() && c.skipped.is_empty());
    }
}
