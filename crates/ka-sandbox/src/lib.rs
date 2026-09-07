//! Filesystem sandbox policy for child processes.
//!
//! `[sandbox] mode = "fs"` restricts bash children to: read everything,
//! write only the working directory, the OS temp dir, and XDG state/cache
//! dirs. Enforcement uses an external sandbox tool that applies
//! `PR_SET_NO_NEW_PRIVS` semantics (bubblewrap, then firejail); when
//! neither tool exists the policy FAILS CLOSED — the command refuses.
//!
//! Deliberate deviation from the roadmap wording: no in-process landlock
//! (the workspace forbids `unsafe`, and adding the landlock crate is not
//! sanctioned). Default mode is `off`, so behavior is unchanged unless
//! the user opts in.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Sandbox mode (`[sandbox] mode`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// No sandboxing (default).
    #[default]
    Off,
    /// Filesystem allowlist sandbox.
    Fs,
}

/// The resolved sandbox policy handed to the bash hand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Policy {
    /// Run the command as-is.
    Off,
    /// Wrap the command in an fs-restricting launcher.
    Fs {
        /// Writable directories (cwd, XDG state/cache, /tmp).
        allow_write: Vec<PathBuf>,
    },
}

/// Sandbox configuration table (`[sandbox]`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SandboxConfig {
    /// `"off"` (default) or `"fs"`.
    pub mode: Option<String>,
}

/// The writable-directory allowlist for `cwd`.
pub fn allowlist_for(cwd: &Path) -> Vec<PathBuf> {
    let mut allow = vec![cwd.to_path_buf(), PathBuf::from("/tmp")];
    let state = std::env::var("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".local/state")));
    let cache = std::env::var("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".cache")));
    if let Ok(p) = state {
        allow.push(p);
    }
    if let Ok(p) = cache {
        allow.push(p);
    }
    allow
}

/// Resolve config → policy.
pub fn policy_from_config(cfg: &SandboxConfig, cwd: &Path) -> Result<Policy, String> {
    let mode = cfg.mode.as_deref().unwrap_or("off");
    match mode {
        "off" => Ok(Policy::Off),
        "fs" => Ok(Policy::Fs {
            allow_write: allowlist_for(cwd),
        }),
        other => Err(format!(
            "[sandbox] mode: unknown value {other:?} (expected \"off\" or \"fs\")"
        )),
    }
}

/// Which external sandbox tool is available, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Bubblewrap,
    Firejail,
}

pub fn detect_tool() -> Option<Tool> {
    for (bin, tool) in [("bwrap", Tool::Bubblewrap), ("firejail", Tool::Firejail)] {
        if std::process::Command::new(bin)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            return Some(tool);
        }
    }
    None
}

/// Build the wrapped argv for `command` under the given policy.
/// `Err` = fail closed (no enforcement tool available).
pub fn wrap_command(policy: &Policy, command: &str, cwd: &Path) -> Result<Vec<String>, String> {
    match policy {
        Policy::Off => Ok(vec!["sh".into(), "-c".into(), command.into()]),
        Policy::Fs { allow_write } => match detect_tool() {
            Some(Tool::Bubblewrap) => {
                let mut argv = vec![
                    "bwrap".into(),
                    "--unshare-all".into(),
                    "--die-with-parent".into(),
                    "--new-session".into(),
                    "--ro-bind".into(),
                    "/".into(),
                    "/".into(),
                    "--dev".into(),
                    "/dev".into(),
                    "--proc".into(),
                    "/proc".into(),
                ];
                for dir in allow_write {
                    argv.push("--bind".into());
                    argv.push(dir.to_string_lossy().into_owned());
                    argv.push(dir.to_string_lossy().into_owned());
                }
                argv.push("--clearenv".into());
                argv.push("sh".into());
                argv.push("-c".into());
                argv.push(command.into());
                Ok(argv)
            }
            Some(Tool::Firejail) => {
                let mut argv = vec![
                    "firejail".into(),
                    "--quiet".into(),
                    "--private=.".into(),
                    "--read-only=/".into(),
                ];
                for dir in allow_write {
                    argv.push(format!("--whitelist={}", dir.display()));
                }
                argv.push("--".into());
                argv.push("sh".into());
                argv.push("-c".into());
                argv.push(command.into());
                let _ = cwd;
                Ok(argv)
            }
            None => Err(
                "sandbox: mode \"fs\" requires bubblewrap (bwrap) or firejail on this host — \
                 refusing to run unsandboxed"
                    .into(),
            ),
        },
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn mode_parses_and_fails_closed_on_unknown() {
        let off: SandboxConfig = serde_json::from_str(r#"{"mode":"off"}"#).unwrap();
        assert_eq!(
            policy_from_config(&off, Path::new("/tmp")).unwrap(),
            Policy::Off
        );
        let bad: SandboxConfig = serde_json::from_str(r#"{"mode":"yolo"}"#).unwrap();
        assert!(policy_from_config(&bad, Path::new("/tmp")).is_err());
    }

    #[test]
    fn allowlist_includes_cwd_tmp_and_xdg() {
        let cwd = Path::new("/work/proj");
        let allow = allowlist_for(cwd);
        assert!(allow.contains(&PathBuf::from("/work/proj")));
        assert!(allow.contains(&PathBuf::from("/tmp")));
    }

    #[test]
    fn off_policy_leaves_command_plain() {
        let p = policy_from_config(
            &serde_json::from_str(r#"{"mode":"off"}"#).unwrap(),
            Path::new("/tmp"),
        )
        .unwrap();
        let argv = wrap_command(&p, "echo hi", Path::new("/tmp")).unwrap();
        assert_eq!(argv, vec!["sh", "-c", "echo hi"]);
    }

    #[test]
    fn fs_policy_wraps_with_allowlist_or_fails_closed() {
        let p = Policy::Fs {
            allow_write: vec![PathBuf::from("/tmp")],
        };
        match wrap_command(&p, "make", Path::new("/tmp")) {
            Ok(argv) => {
                // a tool exists on this host: verify it wraps
                assert!(argv[0] == "bwrap" || argv[0] == "firejail", "{argv:?}");
                assert!(argv.iter().any(|a| a == "make"));
            }
            Err(e) => assert!(e.contains("refusing"), "{e}"),
        }
    }
}
