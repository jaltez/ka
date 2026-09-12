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

/// Safe mode (`ka --safe-mode`, or `KA_SAFEMODE=1` in the
/// environment): all convention discovery returns empty. Built-in
/// tools, the config chain, rules, and auth stay untouched — a
/// troubleshooting floor, not a factory reset. The CLI flips the
/// process-global flag before spawning any runtime threads
/// (`std::env::set_var` is unsafe in edition 2024 and the workspace
/// forbids `unsafe`).
static BARE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Enable safe mode for this process (CLI `--safe-mode`).
pub fn set_bare_mode(on: bool) {
    BARE.store(on, std::sync::atomic::Ordering::Relaxed);
}

pub fn bare_mode() -> bool {
    // the env half is read once: bare_mode is consulted on every hook
    // run and discovery call, and KA_SAFEMODE never changes mid-process
    static ENV_BARE: std::sync::LazyLock<bool> =
        std::sync::LazyLock::new(|| std::env::var("KA_SAFEMODE").is_ok_and(|v| v == "1"));
    BARE.load(std::sync::atomic::Ordering::Relaxed) || *ENV_BARE
}

/// One loaded memory file.
pub struct MemoryFile {
    /// Where it came from.
    pub path: PathBuf,
    /// Full content.
    pub content: String,
}

/// Memory tiers: project `MEMORY.md` first (cwd, ungated like
/// AGENTS.md), then the user-level `~/.config/ka/MEMORY.md`. Missing
/// files are skipped.
pub fn discover_memory(cwd: &Path) -> Vec<MemoryFile> {
    if bare_mode() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let project = cwd.join("MEMORY.md");
    if let Ok(content) = std::fs::read_to_string(&project) {
        if !content.trim().is_empty() {
            out.push(MemoryFile {
                path: project,
                content,
            });
        }
    }
    let user = std::env::var("HOME")
        .map(|h| PathBuf::from(h).join(".config/ka/MEMORY.md"))
        .ok();
    if let Some(user) = user {
        if let Ok(content) = std::fs::read_to_string(&user) {
            if !content.trim().is_empty() {
                out.push(MemoryFile {
                    path: user,
                    content,
                });
            }
        }
    }
    out
}

pub fn discover_agents(cwd: &Path) -> Vec<AgentsFile> {
    if bare_mode() {
        return Vec::new();
    }
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
    if bare_mode() {
        return Vec::new();
    }
    let project_trusted = crate::trust::project_trusted(cwd);
    let home = std::env::var("HOME").map(PathBuf::from).ok();
    let project = vec![
        cwd.join(".ka/skills"),
        cwd.join(".agents/skills"),
        cwd.join(".claude/skills"),
    ];
    let user = match &home {
        Some(h) => vec![
            h.join(".config/ka/skills"),
            h.join(".agents/skills"),
            h.join(".claude/skills"),
        ],
        None => Vec::new(),
    };
    discover_skills_scoped(project, user, project_trusted)
}

/// [`discover_skills`] with explicit roots and trust decision: untrusted
/// projects contribute no roots, user scope always loads (tests).
pub fn discover_skills_scoped(
    project: Vec<PathBuf>,
    user: Vec<PathBuf>,
    project_trusted: bool,
) -> Vec<Skill> {
    let project = if project_trusted { project } else { Vec::new() };
    discover_in(project, user)
}

/// Merge pre-gated project roots with always-on user roots; earlier roots
/// shadow later ones, so project skills win over user skills of the same
/// name. Tests: [`discover_skills`] derives `project` from the trust store.
pub fn discover_in(project: Vec<PathBuf>, user: Vec<PathBuf>) -> Vec<Skill> {
    discover_skills_in(project.into_iter().chain(user).collect())
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
    fn bare_mode_defaults_off() {
        // safe mode is strictly opt-in: no test, layer, or startup path
        // may leave the process-global flag set for the normal run
        assert!(!bare_mode());
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

    #[test]
    fn untrusted_project_skills_absent_user_scope_present() {
        let root = temp_tree("gated-untrusted");
        for dir in [
            root.join("proj/.ka/skills/ship"),
            root.join("user/skills/shared"),
        ] {
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("SKILL.md"), "---\ndescription: d\n---\n").unwrap();
        }
        crate::trust::test_support::with_trust_file(|_| {
            // untrusted via the real store: the project skill is hidden
            // (user-scope skills from the real HOME may still appear)
            let skills = discover_skills(&root.join("proj"));
            assert!(
                skills.iter().all(|s| s.name != "ship"),
                "untrusted project skill must be absent: {:?}",
                skills.iter().map(|s| &s.name).collect::<Vec<_>>()
            );
            // user scope is never gated
            let skills = discover_skills_scoped(
                vec![root.join("proj/.ka/skills")],
                vec![root.join("user/skills")],
                false,
            );
            let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
            assert_eq!(
                names,
                vec!["shared"],
                "project hidden, user kept: {names:?}"
            );
        });
    }

    #[test]
    fn trusted_project_skills_load_and_shadow_user() {
        let root = temp_tree("gated-trusted");
        for (dir, desc) in [
            (root.join("proj/.ka/skills/deploy"), "How we ship"),
            (root.join("user/skills/deploy"), "shadowed"),
            (root.join("user/skills/lint"), "user lint"),
        ] {
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("SKILL.md"),
                format!("---\ndescription: {desc}\n---\n"),
            )
            .unwrap();
        }
        let proj = root.join("proj");
        crate::trust::test_support::with_trust_file(|_| {
            // approve via the shared trust path, exactly like the CLI does
            crate::trust::approve(&proj);
            let skills = discover_skills(&proj);
            let by_name: std::collections::HashMap<&str, &str> = skills
                .iter()
                .map(|s| (s.name.as_str(), s.description.as_str()))
                .collect();
            // user roots still load through discover_skills' HOME derivation
            let scoped = discover_skills_scoped(
                vec![proj.join(".ka/skills")],
                vec![root.join("user/skills")],
                crate::trust::project_trusted(&proj),
            );
            let by_name_scoped: std::collections::HashMap<String, String> = scoped
                .into_iter()
                .map(|s| (s.name, s.description))
                .collect();
            assert_eq!(
                by_name_scoped.get("deploy").map(String::as_str),
                Some("How we ship"),
                "project shadows user"
            );
            assert_eq!(
                by_name_scoped.get("lint").map(String::as_str),
                Some("user lint"),
                "user scope still loads"
            );
            // and the real store approved above: project deploy is present
            assert_eq!(
                by_name.get("deploy").map(|s| &**s),
                Some("How we ship"),
                "approved project contributes its skills: {by_name:?}"
            );
        });
    }
}
