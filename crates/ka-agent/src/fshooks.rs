//! File-convention hooks: executable shell scripts under `.ka/hooks/`.
//!
//! Three hook points, each addressed by file name (`.sh` optional):
//! `pre-turn`, `post-turn`, and `pre-tool`. Scripts run with the session
//! working directory as cwd and `KA_HOOK=<name>` in the environment
//! (`KA_TOOL=<tool>` additionally for `pre-tool`). A non-zero exit is
//! advisory for turns (surfaced as a note with the stderr tail) and a
//! veto for `pre-tool` (the tool call is refused). Everything is
//! best-effort: missing hooks are a no-op, each hook gets 10 seconds,
//! and failures never panic the engine.
//!
//! This complements the ka.toml `[[hooks]]` entries (config-file hooks,
//! see [`crate::config::Hook`]); both may coexist.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The three conventional hook points.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookPoint {
    /// Before a turn starts.
    PreTurn,
    /// After a turn finished.
    PostTurn,
    /// Before an approved tool call executes (non-zero = veto).
    PreTool,
}

impl HookPoint {
    /// The hook file stem (and `KA_HOOK` value).
    pub fn name(self) -> &'static str {
        match self {
            HookPoint::PreTurn => "pre-turn",
            HookPoint::PostTurn => "post-turn",
            HookPoint::PreTool => "pre-tool",
        }
    }
}

/// Per-hook time budget.
const HOOK_TIMEOUT: Duration = Duration::from_secs(10);
/// Cap on the stderr tail carried in veto/note messages.
const TAIL_CHARS: usize = 400;

/// Run the convention hook for `event` if one exists. `Ok(())` means
/// proceed (hook missing, or exited zero); `Err(reason)` means the hook
/// failed or timed out, with the stderr tail as the reason.
pub async fn run(event: HookPoint, cwd: &Path, tool: Option<&str>) -> Result<(), String> {
    let Some(script) = locate(event, cwd) else {
        return Ok(());
    };
    let name = event.name();
    let tool = tool.map(str::to_string);
    let attempt = {
        let script = script.clone();
        let cwd = cwd.to_path_buf();
        tokio::task::spawn_blocking(move || execute(&script, &cwd, name, tool.as_deref()))
    };
    // belt-and-braces around the blocking pool join; execute() enforces
    // the real 10s budget itself and kills the child on overrun
    match tokio::time::timeout(HOOK_TIMEOUT + Duration::from_secs(5), attempt).await {
        Ok(Ok(result)) => result,
        Ok(Err(join)) => Err(format!("hook task failed: {join}")),
        Err(_) => Err(format!("{name} hook did not finish in 10s")),
    }
}

/// Find the hook script: `.ka/hooks/<name>.sh` first, then `.ka/hooks/<name>`.
fn locate(event: HookPoint, cwd: &Path) -> Option<PathBuf> {
    let base = cwd.join(".ka").join("hooks");
    [format!("{}.sh", event.name()), event.name().to_string()]
        .into_iter()
        .map(|name| base.join(name))
        .find(|path| path.is_file())
}

/// Execute one hook script synchronously (runs on the blocking pool).
fn execute(script: &Path, cwd: &Path, name: &str, tool: Option<&str>) -> Result<(), String> {
    let mut cmd = std::process::Command::new("sh");
    cmd.arg(script)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .env("KA_HOOK", name);
    if let Some(tool) = tool {
        cmd.env("KA_TOOL", tool);
    }
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => return Err(format!("{name} hook failed to spawn: {e}")),
    };
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if started.elapsed() >= HOOK_TIMEOUT {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("{name} hook timed out after 10s"));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(format!("{name} hook wait failed: {e}")),
        }
    };
    if status.success() {
        return Ok(());
    }
    let stderr = child
        .stderr
        .take()
        .and_then(|mut pipe| {
            use std::io::Read;
            let mut buf = Vec::new();
            pipe.read_to_end(&mut buf).ok().map(|_| buf)
        })
        .unwrap_or_default();
    let code = status
        .code()
        .map(|c| c.to_string())
        .unwrap_or_else(|| "signal".to_string());
    Err(format!("{name} hook exited {code}: {}", tail(&stderr)))
}

/// The last `TAIL_CHARS` characters of a stream, trimmed.
fn tail(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let trimmed = text.trim_end();
    let tail: String = trimmed.chars().rev().take(TAIL_CHARS).collect();
    tail.chars().rev().collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn hook_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ka-fshooks-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".ka/hooks")).unwrap();
        dir
    }

    #[tokio::test]
    async fn missing_hooks_are_a_noop() {
        let dir = hook_dir("missing");
        assert!(run(HookPoint::PreTurn, &dir, None).await.is_ok());
        assert!(run(HookPoint::PreTool, &dir, Some("bash")).await.is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn failing_pre_turn_reports_stderr_tail() {
        let dir = hook_dir("pre-turn-fail");
        std::fs::write(dir.join(".ka/hooks/pre-turn.sh"), "echo boom >&2; exit 3\n").unwrap();
        let err = run(HookPoint::PreTurn, &dir, None).await.unwrap_err();
        assert!(err.contains("exited 3"), "{err}");
        assert!(err.contains("boom"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn successful_hook_and_bare_name_resolution() {
        let dir = hook_dir("ok");
        std::fs::write(dir.join(".ka/hooks/post-turn"), "exit 0\n").unwrap();
        assert!(run(HookPoint::PostTurn, &dir, None).await.is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn pre_tool_veto_carries_tool_env() {
        let dir = hook_dir("pre-tool");
        std::fs::write(
            dir.join(".ka/hooks/pre-tool.sh"),
            "test \"$KA_TOOL\" = bash -a \"$KA_HOOK\" = pre-tool || exit 1\nexit 5\n",
        )
        .unwrap();
        let err = run(HookPoint::PreTool, &dir, Some("bash"))
            .await
            .unwrap_err();
        assert!(err.contains("exited 5"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn hanging_hook_times_out() {
        let dir = hook_dir("hang");
        std::fs::write(dir.join(".ka/hooks/pre-turn.sh"), "sleep 30\n").unwrap();
        let err = run(HookPoint::PreTurn, &dir, None).await.unwrap_err();
        assert!(err.contains("timed out"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tail_caps_long_output() {
        let long = "x".repeat(2_000);
        assert_eq!(tail(long.as_bytes()).chars().count(), TAIL_CHARS);
    }
}
