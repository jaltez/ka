//! Markdown agents: user-defined subagents discovered from `.md` files.
//! One file = one agent: an optional `---` frontmatter block (`name`,
//! `description`, `max-steps`) and a body that becomes the subagent's
//! system prompt. The model delegates via the `delegate` tool.

use std::path::PathBuf;

/// One parsed agent definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentDef {
    /// Slug the model references in `delegate`.
    pub name: String,
    /// When to delegate to this agent (shown in the tool description).
    pub description: String,
    /// The markdown body: the subagent's system prompt.
    pub system: String,
    /// Step budget for the nested voice.
    pub max_steps: u32,
}

impl AgentDef {
    /// Parse one markdown file. `fallback_name` (the file stem) is used
    /// when the frontmatter carries no `name`.
    pub fn parse(text: &str, fallback_name: &str) -> Self {
        let mut name = String::new();
        let mut description = String::new();
        let mut max_steps = 12u32;
        let mut body = text.to_string();

        if let Some(rest) = text.strip_prefix("---") {
            if let Some(end) = rest.find("\n---") {
                for line in rest[..end].lines() {
                    let Some((key, value)) = line.split_once(':') else {
                        continue;
                    };
                    let value = value.trim();
                    match key.trim() {
                        "name" if !value.is_empty() => name = value.to_string(),
                        "description" if !value.is_empty() => description = value.to_string(),
                        "max-steps" | "max_steps" => {
                            max_steps = value.parse().unwrap_or(12).clamp(1, 64);
                        }
                        _ => {}
                    }
                }
                body = rest[end + 4..].trim_start_matches('\n').to_string();
            }
        }
        if name.is_empty() {
            name = fallback_name.to_string();
        }
        Self {
            name: name.to_string(),
            description,
            system: body.trim().to_string(),
            max_steps,
        }
    }

    /// Discovery roots for a working directory.
    fn roots(cwd: &std::path::Path) -> Vec<PathBuf> {
        let mut roots = vec![
            cwd.join(".ka/agents"),
            cwd.join(".agents"),
            cwd.join(".claude/agents"),
        ];
        if let Ok(home) = std::env::var("HOME") {
            roots.push(PathBuf::from(home).join(".config/ka/agents"));
        }
        roots
    }

    /// Discover agents (name-sorted; project dirs win over the user dir
    /// on name collisions — first sighting wins).
    pub fn discover(cwd: &std::path::Path) -> Vec<AgentDef> {
        let mut agents: Vec<AgentDef> = Vec::new();
        for root in Self::roots(cwd) {
            let Ok(entries) = std::fs::read_dir(&root) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_none_or(|e| e != "md") {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let stem = path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                if stem.is_empty() {
                    continue;
                }
                let def = Self::parse(&text, &stem);
                if def.system.is_empty() {
                    continue;
                }
                if !agents.iter().any(|a| a.name == def.name) {
                    agents.push(def);
                }
            }
        }
        agents.sort_by(|a, b| a.name.cmp(&b.name));
        agents
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn parses_frontmatter_and_body() {
        let md = "---\nname: reviewer\ndescription: reviews code for bugs\nmax-steps: 20\n---\nYou are a code reviewer.\nBe harsh.";
        let def = AgentDef::parse(md, "fallback");
        assert_eq!(def.name, "reviewer");
        assert_eq!(def.description, "reviews code for bugs");
        assert_eq!(def.max_steps, 20);
        assert_eq!(def.system, "You are a code reviewer.\nBe harsh.");
    }

    #[test]
    fn frontmatter_is_optional_with_defaults() {
        let def = AgentDef::parse("Just a body prompt.", "stem-name");
        assert_eq!(def.name, "stem-name");
        assert_eq!(def.description, "");
        assert_eq!(def.max_steps, 12);
        assert_eq!(def.system, "Just a body prompt.");
    }

    #[test]
    fn malformed_values_fall_back() {
        let md = "---\nmax-steps: bananas\nname: x\n---\nBody.";
        let def = AgentDef::parse(md, "s");
        assert_eq!(def.max_steps, 12, "unparseable max-steps keeps the default");
    }

    #[test]
    fn unterminated_frontmatter_treated_as_body() {
        let md = "---\nname: broken\nYou are still a prompt.";
        let def = AgentDef::parse(md, "stem");
        assert_eq!(def.name, "stem");
        assert!(def.system.contains("---"), "kept verbatim");
    }

    #[test]
    fn discovers_from_directories_with_precedence() {
        let dir = std::env::temp_dir().join(format!("ka-agents-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let proj = dir.join("proj/.ka/agents");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::create_dir_all(dir.join("proj/.agents")).unwrap();
        std::fs::write(
            proj.join("reviewer.md"),
            "---\nname: reviewer\ndescription: project version\n---\nProject reviewer.",
        )
        .unwrap();
        std::fs::write(dir.join("proj/.agents/util.md"), "Utility agent.").unwrap();

        let agents = AgentDef::discover(dir.join("proj").as_path());
        let names: Vec<&str> = agents.iter().map(|a| a.name.as_str()).collect();
        assert!(names.contains(&"reviewer"), "{names:?}");
        assert!(names.contains(&"util"), "{names:?}");
        let reviewer = agents.iter().find(|a| a.name == "reviewer").unwrap();
        assert_eq!(reviewer.description, "project version");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
