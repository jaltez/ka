//! The bash hand: guarded shell execution with timeout, process-tree kill,
//! and capped output with spill parking. Analysis/gating happens in the
//! engine (see `bashp`); this hand only executes what was approved.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::process::Command;

use super::{Hand, HandContext, HandDef, ToolOutput};

/// Default timeout (ms).
pub const DEFAULT_TIMEOUT_MS: u64 = 120_000;
/// Maximum timeout (ms).
pub const MAX_TIMEOUT_MS: u64 = 3_600_000;
/// Output cap: bytes kept in the tail shown to the model.
pub const TAIL_CAP: usize = 32_768;
/// Output cap: bytes kept from the head.
pub const HEAD_CAP: usize = 8_192;
/// Per-line character cap.
pub const LINE_CAP: usize = 768;

/// The bash tool.
pub struct BashHand;

impl Hand for BashHand {
    fn def(&self) -> HandDef {
        HandDef {
            name: "bash",
            description: "Run a shell command and return combined output. Output is capped \
                (tail kept, full output parked in a spill file). timeout_ms default 120000, \
                max 3600000."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Shell command line" },
                    "timeout_ms": { "type": "integer", "description": "Timeout in milliseconds" },
                    "cwd": { "type": "string", "description": "Working directory (default: session cwd)" }
                },
                "required": ["command"]
            }),
            clearance: super::Clearance::Exec,
            read_only: false,
        }
    }

    fn execute<'a>(
        &'a self,
        args: &'a Value,
        ctx: &'a HandContext,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>> {
        Box::pin(async move {
            let Some(command) = args.get("command").and_then(Value::as_str) else {
                return ToolOutput::err("bash: missing required 'command'");
            };
            let timeout_ms = args
                .get("timeout_ms")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_TIMEOUT_MS)
                .min(MAX_TIMEOUT_MS);
            let cwd = args
                .get("cwd")
                .and_then(Value::as_str)
                .map(|p| super::read::resolve(ctx, p))
                .unwrap_or_else(|| ctx.cwd.clone());

            let mut cmd = Command::new("sh");
            cmd.arg("-c").arg(command).arg("sh").current_dir(&cwd);

            let mut child = match cmd
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
            {
                Ok(c) => c,
                Err(e) => return ToolOutput::err(format!("bash spawn: {e}")),
            };
            let pid = child.id();

            let collect = async {
                let stdout = child.stdout.take();
                let stderr = child.stderr.take();
                let out_task = tokio::spawn(async move {
                    use tokio::io::AsyncReadExt;
                    let mut buf = Vec::new();
                    if let Some(mut s) = stdout {
                        let _ = s.read_to_end(&mut buf).await;
                    }
                    buf
                });
                let err_task = tokio::spawn(async move {
                    use tokio::io::AsyncReadExt;
                    let mut buf = Vec::new();
                    if let Some(mut s) = stderr {
                        let _ = s.read_to_end(&mut buf).await;
                    }
                    buf
                });
                let out = out_task.await.unwrap_or_default();
                let err = err_task.await.unwrap_or_default();
                let status = child.wait().await.ok();
                (out, err, status)
            };

            let (stdout, stderr, status) =
                match tokio::time::timeout(Duration::from_millis(timeout_ms), collect).await {
                    Ok(r) => r,
                    Err(_) => {
                        // kill the whole process group, then reap
                        if let Some(pid) = pid {
                            #[cfg(unix)]
                            {
                                kill_tree(pid);
                            }
                        }
                        let _ = child.start_kill();
                        ctx.ledger.lock().invalidate_all();
                        return ToolOutput::err(format!(
                            "bash: timed out after {timeout_ms}ms (killed):\n{command}"
                        ));
                    }
                };

            ctx.ledger.lock().invalidate_all();
            let mut combined = String::from_utf8_lossy(&stdout).into_owned();
            if !stderr.is_empty() {
                let err_text = String::from_utf8_lossy(&stderr).into_owned();
                if !combined.is_empty() {
                    combined.push('\n');
                }
                combined.push_str("(stderr)\n");
                combined.push_str(&err_text);
            }

            let code = status.and_then(|s| s.code());
            let is_error = code.map(|c| c != 0).unwrap_or(true);
            let capped = cap_output(ctx, &combined);
            let exit_note = match code {
                Some(0) => String::new(),
                Some(c) => format!("(exit {c})\n"),
                None => "(no exit status)\n".to_string(),
            };
            let header = format!("{exit_note}$ {command}\n");
            if capped.spilled {
                let pointer = capped.pointer.clone().unwrap_or_default();
                ToolOutput {
                    content: format!(
                        "{header}{}[output capped; full output at {pointer}]",
                        capped.text
                    ),
                    is_error,
                    spill: capped.pointer,
                }
            } else {
                ToolOutput {
                    content: format!("{header}{}", capped.text),
                    is_error,
                    spill: None,
                }
            }
        })
    }
}

#[cfg(unix)]
fn kill_tree(pid: u32) {
    // Positive-pid kills only: negative-pid (process-group) kills proved
    // unsafe on some hosts (can signal the caller's own group). We kill
    // direct children first, then the shell itself. `sh -c` execs single
    // commands, so the common case is one process anyway.
    let script = format!("pkill -9 -P {pid} 2>/dev/null; kill -9 {pid} 2>/dev/null; true");
    let _ = std::process::Command::new("sh")
        .arg("-c")
        .arg(&script)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

struct Capped {
    text: String,
    spilled: bool,
    pointer: Option<String>,
}

fn cap_output(ctx: &HandContext, raw: &str) -> Capped {
    let line_capped: Vec<String> = raw
        .lines()
        .map(|l| {
            if l.chars().count() > LINE_CAP {
                let t: String = l.chars().take(LINE_CAP).collect();
                format!("{t}…")
            } else {
                l.to_string()
            }
        })
        .collect();
    let joined = line_capped.join("\n");
    if joined.len() <= TAIL_CAP + HEAD_CAP {
        return Capped {
            text: joined,
            spilled: false,
            pointer: None,
        };
    }
    let head: String = joined.chars().take(HEAD_CAP).collect();
    let tail: String = joined
        .chars()
        .rev()
        .take(TAIL_CAP)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let pointer = ctx.spill.park(raw).ok();
    Capped {
        text: format!(
            "{head}\n[…{} bytes elided…]\n{tail}",
            joined.len() - HEAD_CAP - TAIL_CAP
        ),
        spilled: true,
        pointer,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::Arc;

    use parking_lot::Mutex;

    use super::*;
    use crate::hands::{Ledger, Spill};

    fn ctx_for(dir: &std::path::Path) -> HandContext {
        HandContext {
            cwd: dir.to_path_buf(),
            ledger: Arc::new(Mutex::new(Ledger::default())),
            spill: Arc::new(Spill::new()),
        }
    }

    #[tokio::test]
    async fn runs_and_reports_exit_codes() {
        let dir = std::env::temp_dir().join(format!("ka-bash-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = ctx_for(&dir);
        let out = BashHand
            .execute(&json!({"command": "echo hello"}), &ctx)
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("hello"));
        let out = BashHand.execute(&json!({"command": "exit 3"}), &ctx).await;
        assert!(out.is_error);
        assert!(out.content.contains("(exit 3)"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn timeout_kills_and_errors() {
        let dir = std::env::temp_dir().join(format!("ka-bash-to-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = ctx_for(&dir);
        let started = std::time::Instant::now();
        let out = BashHand
            .execute(&json!({"command": "sleep 5", "timeout_ms": 300}), &ctx)
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("timed out"));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "kill must be prompt"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn huge_output_spills() {
        let dir = std::env::temp_dir().join(format!("ka-bash-cap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = ctx_for(&dir);
        let out = BashHand
            .execute(&json!({"command": "seq 1 60000"}), &ctx)
            .await;
        assert!(!out.is_error);
        assert!(out.spill.is_some(), "expected spill pointer");
        assert!(
            out.content.contains("elided"),
            "{}",
            out.content.chars().take(200).collect::<String>()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn bash_invalidates_ledger() {
        let dir = std::env::temp_dir().join(format!("ka-bash-led-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("f.txt");
        std::fs::write(&f, "v1").unwrap();
        let ctx = ctx_for(&dir);
        let meta = std::fs::metadata(&f).unwrap();
        ctx.ledger.lock().mint(&f, &meta);
        assert_eq!(ctx.ledger.lock().len(), 1);
        BashHand.execute(&json!({"command": "true"}), &ctx).await;
        assert!(
            ctx.ledger.lock().is_empty(),
            "any bash run must invalidate the ledger"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
