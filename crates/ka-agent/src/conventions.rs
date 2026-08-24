//! Conventions: AGENTS.md hierarchy discovery and SKILL.md progressive
//! disclosure. Pure filesystem discovery; the voice folds results into
//! the system prompt.

use std::path::{Path, PathBuf};

/// One discovered AGENTS.md layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentsFile {
    /// Absolute path (for on-demand reads by the model).
    pub path: PathBuf,
    /// File content (root→cwd layers are concatenated by the caller).
    pub content: String,
}

/// Discover AGENTS.md files from the filesystem root side of cwd down to
/// cwd itself (nearest = last). Stops walking at the home directory or `/`.
pub fn discover_agents(cwd: &Path) -> Vec<AgentsFile> {
    let mut chain: Vec<PathBuf> = vec![cwd.to_path_buf()];
    let mut cur = cwd.to_path_buf();
    let home = std::env::var("HOME").map(PathBuf::from).ok();
    while let Some(parent) = cur.parent() {
        if parent == cur {
            break;
        }
        cur = parent.to_path_buf();
        chain.push(cur.clone());
        if home.as_ref() == Some(&cur) || cur == Path::new("/") {
            break;
        }
    }
    let mut found = Vec::new();
    for dir in chain.iter().rev() {
        for name in ["AGENTS.md", "CLAUDE.md"] {
            let candidate = dir.join(name);
            if let Ok(content) = std::fs::read_to_string(&candidate) {
                if !content.trim().is_empty() {
                    found.push(AgentsFile {
                        path: candidate,
                        content,
                    });
                    break; // one file per directory, AGENTS.md preferred
                }
            }
        }
    }
    // cap: keep the nearest 4 layers (deepest = most specific)
    if found.len() > 4 {
        let skip = found.len() - 4;
        found.drain(..skip);
    }
    found
}

/// One discovered skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    /// Skill name (directory name).
    pub name: String,
    /// Absolute SKILL.md path (the model reads this on demand).
    pub path: PathBuf,
    /// One-line description from frontmatter.
    pub description: String,
}

/// Discover SKILL.md skills across ka-native and ecosystem directories.
/// Progressive disclosure: only name+description+path reach the prompt.
pub fn discover_skills(cwd: &Path) -> Vec<Skill> {
    let home = std::env::var("HOME").map(PathBuf::from).ok();
    let mut roots: Vec<PathBuf> = vec![
        cwd.join(".ka/skills"),
        cwd.join(".agents/skills"),
        cwd.join(".claude/skills"),
    ];
    if let Some(h) = &home {
        roots.push(h.join(".config/ka/skills"));
        roots.push(h.join(".agents/skills"));
        roots.push(h.join(".claude/skills"));
    }
    discover_skills_in(roots)
}

/// Skill discovery against explicit roots (tests).
pub fn discover_skills_in(roots: Vec<PathBuf>) -> Vec<Skill> {
    let mut skills: Vec<Skill> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let dir = entry.path();
            let skill_md = dir.join("SKILL.md");
            if !skill_md.is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if !seen.insert(name.clone()) {
                continue; // first root wins (project > user)
            }
            let Ok(content) = std::fs::read_to_string(&skill_md) else {
                continue;
            };
            let description = parse_frontmatter_description(&content)
                .unwrap_or_else(|| "(no description)".to_string());
            skills.push(Skill {
                name,
                path: skill_md,
                description,
            });
        }
    }
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills.truncate(20);
    skills
}

/// Extract `description:` from YAML-ish frontmatter (no YAML dep).
fn parse_frontmatter_description(content: &str) -> Option<String> {
    let rest = content.strip_prefix("---\n")?;
    let end = rest.find("\n---")?;
    for line in rest[..end].lines() {
        if let Some(v) = line.strip_prefix("description:") {
            let v = v.trim().trim_matches('"').trim_matches('\'');
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn temp_tree(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ka-conv-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn agents_walk_root_to_cwd_prefers_agents_md() {
        let root = temp_tree("agents");
        let mid = root.join("pkg");
        let deep = mid.join("src");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(root.join("AGENTS.md"), "root rules").unwrap();
        std::fs::write(mid.join("CLAUDE.md"), "mid rules (compat)").unwrap();
        std::fs::write(deep.join("AGENTS.md"), "deep rules").unwrap();

        let found = discover_agents(&deep);
        assert_eq!(found.len(), 3);
        assert_eq!(found[0].content, "root rules");
        assert_eq!(found[1].content, "mid rules (compat)");
        assert_eq!(found[2].content, "deep rules");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn skills_discover_with_description() {
        let root = temp_tree("skills");
        let proj = root.join(".ka/skills/deploy");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(
            proj.join("SKILL.md"),
            "---\nname: deploy\ndescription: How we ship\n---\nbody",
        )
        .unwrap();
        let user = root.join(".agents/skills/deploy");
        std::fs::create_dir_all(&user).unwrap();
        std::fs::write(user.join("SKILL.md"), "---\ndescription: shadowed\n---\n").unwrap();

        let skills = discover_skills_in(vec![root.join(".ka/skills"), root.join(".agents/skills")]);
        assert_eq!(skills.len(), 1, "project root wins over user: {skills:?}");
        assert_eq!(skills[0].description, "How we ship");
        assert!(skills[0].path.starts_with(&root));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn frontmatter_description_parsed() {
        assert_eq!(
            parse_frontmatter_description("---\nname: x\ndescription: \"quoted desc\"\n---\n"),
            Some("quoted desc".to_string())
        );
        assert_eq!(parse_frontmatter_description("no frontmatter"), None);
    }
}
