//! `ka skill install|list|remove`: user-scope skill lifecycle (roadmap
//! 8.4). Sources: a git URL (shallow clone, git is already an allowed
//! child) or a local directory carrying SKILL.md. No registry, no
//! network beyond the one clone the user asked for. The user skills dir
//! is global, so running the install IS the trust act; discovery still
//! gates user skills on the project's trust decision, and agentskills.io
//! frontmatter fields ka does not use (license, compatibility, metadata,
//! allowed-tools) are tolerated by the parser and shown in listings.

use std::path::{Path, PathBuf};

use crate::SkillCmd;

/// The user skills root (`$HOME/.config/ka/skills`).
fn user_skills_dir() -> Result<PathBuf, String> {
    let home = std::env::var("HOME")
        .map(PathBuf::from)
        .map_err(|_| "HOME is not set — cannot locate the user skills dir")?;
    Ok(home.join(".config/ka/skills"))
}

pub fn run_skill(cmd: SkillCmd) -> Result<std::process::ExitCode, String> {
    match cmd {
        SkillCmd::Install { source, force } => install(&source, force),
        SkillCmd::List => list(),
        SkillCmd::Remove { name } => remove(&name),
    }
}

/// Install from a git URL or a local directory.
fn install(source: &str, force: bool) -> Result<std::process::ExitCode, String> {
    let is_git = source.starts_with("https://")
        || source.starts_with("http://")
        || source.starts_with("git@");
    let mut staging: Option<PathBuf> = None;
    let src: PathBuf = if is_git {
        let tmp = std::env::temp_dir().join(format!(
            "ka-skill-clone-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        ));
        let status = std::process::Command::new("git")
            .args(["clone", "--depth", "1"])
            .arg(source)
            .arg(&tmp)
            .status()
            .map_err(|e| format!("git clone: {e}"))?;
        if !status.success() {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(format!("git clone {source} failed"));
        }
        staging = Some(tmp.clone());
        tmp
    } else {
        PathBuf::from(source)
    };
    let src = locate_skill_dir(&src)?;
    let name = src
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .ok_or_else(|| format!("{} has no directory name", src.display()))?;
    let target = user_skills_dir()?.join(&name);
    if target.exists() {
        if !force {
            return Err(format!(
                "{name} is already installed — pass --force to overwrite"
            ));
        }
        std::fs::remove_dir_all(&target).map_err(|e| format!("remove old: {e}"))?;
    }
    copy_dir(&src, &target)?;
    if is_git {
        let _ = staging.as_ref().map(std::fs::remove_dir_all);
    }
    let description = read_description(&target.join("SKILL.md")).unwrap_or_default();
    println!("installed {name} → {}", target.display());
    if !description.is_empty() {
        println!("  {description}");
    }
    Ok(std::process::ExitCode::SUCCESS)
}

fn list() -> Result<std::process::ExitCode, String> {
    let dir = user_skills_dir()?;
    if !dir.is_dir() {
        println!("no user skills dir yet ({})", dir.display());
        return Ok(std::process::ExitCode::SUCCESS);
    }
    let skills = ka_engine::conventions::discover_skills_in(vec![dir]);
    if skills.is_empty() {
        println!("no user skills installed (ka skill install <git-url|path>)");
        return Ok(std::process::ExitCode::SUCCESS);
    }
    for s in skills {
        println!("{} — {}", s.name, s.description);
    }
    Ok(std::process::ExitCode::SUCCESS)
}

fn remove(name: &str) -> Result<std::process::ExitCode, String> {
    if name.contains('/') || name.contains('\\') || name == ".." {
        return Err("skill name must be a bare directory name".to_string());
    }
    let target = user_skills_dir()?.join(name);
    if !target.is_dir() {
        return Err(format!("{name} is not installed"));
    }
    std::fs::remove_dir_all(&target).map_err(|e| format!("remove {name}: {e}"))?;
    println!("removed {name}");
    Ok(std::process::ExitCode::SUCCESS)
}

/// The skill directory behind a source path: the path itself when it
/// carries SKILL.md, else its single child that does (a repo rooted one
/// level above the skill). Two or more candidates refuse — ambiguous.
fn locate_skill_dir(src: &Path) -> Result<PathBuf, String> {
    if src.join("SKILL.md").is_file() {
        return Ok(src.to_path_buf());
    }
    let mut candidates: Vec<PathBuf> = std::fs::read_dir(src)
        .map_err(|e| format!("read {}: {e}", src.display()))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.join("SKILL.md").is_file())
        .collect();
    match candidates.len() {
        1 => Ok(candidates.remove(0)),
        0 => Err(format!(
            "{} carries no SKILL.md (skills are directories with a SKILL.md)",
            src.display()
        )),
        _ => {
            let names: Vec<String> = candidates
                .iter()
                .map(|p| {
                    p.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default()
                })
                .collect();
            Err(format!(
                "{} carries several skills ({}); point at one directory",
                src.display(),
                names.join(", ")
            ))
        }
    }
}

/// Recursive directory copy (no deps; skills are small).
fn copy_dir(src: &Path, dst: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dst).map_err(|e| format!("mkdir {}: {e}", dst.display()))?;
    for entry in std::fs::read_dir(src).map_err(|e| format!("read {}: {e}", src.display()))? {
        let entry = entry.map_err(|e| format!("read {}: {e}", src.display()))?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            copy_dir(&from, &to)?;
        } else {
            std::fs::copy(&from, &to).map_err(|e| format!("copy {}: {e}", from.display()))?;
        }
    }
    Ok(())
}

/// The `description:` line of a SKILL.md, if any.
fn read_description(skill_md: &Path) -> Option<String> {
    let text = std::fs::read_to_string(skill_md).ok()?;
    let rest = text.strip_prefix("---\n")?;
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

/// Render a strand export as a self-contained HTML page: the markdown
/// rides in an inert island and a vendored renderer turns it into DOM —
/// no dependencies, works offline.
pub fn render_html(title: &str, markdown: &str) -> String {
    let esc = |s: &str| {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    };
    crate::EXPORT_TEMPLATE
        .replace("__KA_TITLE__", &esc(title))
        .replace("__KA_MARKDOWN__", &esc(markdown))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ka-skills-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn install_list_remove_round_trip() {
        let src = temp_dir("src");
        let skill = src.join("my-skill");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: my-skill\ndescription: does the thing\nlicense: MIT\n---\nbody",
        )
        .unwrap();
        std::fs::write(skill.join("helper.txt"), "extra file").unwrap();

        let target = user_skills_dir().unwrap().join("my-skill");
        let _ = std::fs::remove_dir_all(&target);

        // locate: direct dir with SKILL.md
        assert_eq!(locate_skill_dir(&skill).unwrap(), skill);
        // repo-rooted one level up: the child carrying SKILL.md wins
        assert_eq!(locate_skill_dir(&src).unwrap(), skill);

        install(skill.to_str().unwrap(), false).unwrap();
        assert!(target.join("SKILL.md").is_file());
        assert!(target.join("helper.txt").is_file());

        // re-install without --force refuses
        assert!(install(skill.to_str().unwrap(), false).is_err());
        // with --force it overwrites
        install(skill.to_str().unwrap(), true).unwrap();

        // list sees it (discovery over the user dir)
        let skills = ka_engine::conventions::discover_skills_in(vec![user_skills_dir().unwrap()]);
        assert!(skills.iter().any(|s| s.name == "my-skill"));

        remove("my-skill").unwrap();
        assert!(!target.exists());
        // traversal-safe remove
        assert!(remove("../evil").is_err());
        let _ = std::fs::remove_dir_all(&src);
    }

    #[test]
    fn html_export_escapes_and_marks_the_island() {
        let page = render_html(
            "t<title>",
            "# Head\n\n<script>alert(1)</script>\n\n- item `code`",
        );
        assert!(!page.contains("__KA_TITLE__"), "title is filled");
        assert!(page.contains("t&lt;title&gt;"), "title is html-escaped");
        assert!(page.contains("&lt;script&gt;"), "markdown is inert");
        assert!(
            !page.contains("<script>alert"),
            "raw script must not pass through"
        );
        assert!(page.contains("text/markdown"), "island present");
    }
}
