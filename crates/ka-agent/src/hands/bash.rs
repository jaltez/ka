//! The bash hand: guarded shell execution with timeout, process-tree kill,
//! and capped output with spill parking. Analysis/gating happens in the
//! engine (see `bashp`); this hand only executes what was approved.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
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

/// Live-preview cadence: partial output is emitted at most every 300ms.
pub const PREVIEW_TICK_MS: u64 = 300;
/// Live-preview cap: at most the last 8 lines per excerpt.
pub const PREVIEW_LINES: usize = 8;
/// Live-preview cap: at most ~600 bytes per excerpt.
pub const PREVIEW_BYTES: usize = 600;

/// The bash tool.
pub struct BashHand;

impl Hand for BashHand {
    fn def(&self) -> HandDef {
        HandDef {
            name: "bash".to_string(),
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
            let noop = |_: String| {};
            Self.execute_streaming(args, ctx, &noop).await
        })
    }
}

impl BashHand {
    /// Execute with live progress: while the child runs, output produced
    /// since the last emission is handed to `progress` at most every
    /// [`PREVIEW_TICK_MS`] (ANSI-stripped, capped, one final drain at exit).
    /// The returned [`ToolOutput`] and all caps/spill/kill behavior are
    /// identical to the plain [`Hand::execute`] path.
    pub fn execute_streaming<'a, P>(
        &'a self,
        args: &'a Value,
        ctx: &'a HandContext,
        progress: &'a P,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>>
    where
        P: Fn(String) + Send + Sync,
    {
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

            // shared pending buffer: both stream pumps append freshly read
            // output here; the ticker drains it as preview excerpts
            let pending: Arc<parking_lot::Mutex<String>> = Arc::default();
            let collect = async {
                let stdout = child.stdout.take();
                let stderr = child.stderr.take();
                let p_out = pending.clone();
                let out_task = tokio::spawn(async move {
                    use tokio::io::AsyncReadExt;
                    let mut buf = Vec::new();
                    if let Some(mut s) = stdout {
                        let mut chunk = [0u8; 4096];
                        loop {
                            match s.read(&mut chunk).await {
                                Ok(0) | Err(_) => break,
                                Ok(n) => {
                                    buf.extend_from_slice(&chunk[..n]);
                                    p_out.lock().push_str(&String::from_utf8_lossy(&chunk[..n]));
                                }
                            }
                        }
                    }
                    buf
                });
                let p_err = pending.clone();
                let err_task = tokio::spawn(async move {
                    use tokio::io::AsyncReadExt;
                    let mut buf = Vec::new();
                    if let Some(mut s) = stderr {
                        let mut chunk = [0u8; 4096];
                        loop {
                            match s.read(&mut chunk).await {
                                Ok(0) | Err(_) => break,
                                Ok(n) => {
                                    buf.extend_from_slice(&chunk[..n]);
                                    p_err.lock().push_str(&String::from_utf8_lossy(&chunk[..n]));
                                }
                            }
                        }
                    }
                    buf
                });
                let out = out_task.await.unwrap_or_default();
                let err = err_task.await.unwrap_or_default();
                let status = child.wait().await.ok();
                (out, err, status)
            };

            let result = tokio::time::timeout(Duration::from_millis(timeout_ms), async {
                tokio::pin!(collect);
                let mut ticker = tokio::time::interval(Duration::from_millis(PREVIEW_TICK_MS));
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    tokio::select! {
                        _ = ticker.tick() => {
                            emit_progress(&pending, progress);
                        }
                        r = &mut collect => break r,
                    }
                }
            })
            .await;
            let (stdout, stderr, status) = match result {
                Ok(r) => {
                    // final drain: output produced since the last tick
                    emit_progress(&pending, progress);
                    r
                }
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

/// Drain the shared pending buffer as one capped preview excerpt.
fn emit_progress(pending: &parking_lot::Mutex<String>, progress: &impl Fn(String)) {
    let fresh = std::mem::take(&mut *pending.lock());
    if fresh.is_empty() {
        return;
    }
    progress(cap_preview(&fresh));
}

/// Strip ANSI escape sequences (CSI runs and OSC strings), keeping
/// printable text only.
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some('[') => {
                chars.next();
                // CSI: parameters + intermediates, then one final byte
                while let Some(&n) = chars.peek() {
                    chars.next();
                    if ('\u{40}'..='\u{7e}').contains(&n) {
                        break;
                    }
                }
            }
            Some(']') => {
                chars.next();
                // OSC: terminated by BEL or ESC \
                while let Some(&n) = chars.peek() {
                    chars.next();
                    if n == '\u{7}' {
                        break;
                    }
                    if n == '\x1b' {
                        chars.next();
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// Cap a live preview excerpt: ANSI-stripped, at most the last
/// [`PREVIEW_LINES`] lines and [`PREVIEW_BYTES`] bytes.
pub fn cap_preview(raw: &str) -> String {
    let plain = strip_ansi(raw);
    let mut lines: Vec<&str> = plain.lines().collect();
    if lines.len() > PREVIEW_LINES {
        lines.drain(..lines.len() - PREVIEW_LINES);
    }
    let mut text = lines.join("\n");
    if text.len() > PREVIEW_BYTES {
        let start = text.len() - PREVIEW_BYTES;
        let start = (start..=text.len())
            .find(|&i| text.is_char_boundary(i))
            .unwrap_or(text.len());
        text = text[start..].to_string();
    }
    text
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

    #[tokio::test]
    async fn streaming_emits_capped_partials() {
        let dir = std::env::temp_dir().join(format!("ka-bash-stream-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = ctx_for(&dir);
        let got: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = got.clone();
        let progress = move |excerpt: String| sink.lock().push(excerpt);
        let out = BashHand
            .execute_streaming(
                &json!({
                    "command": "for i in 1 2 3 4 5 6; do echo line-$i; sleep 0.12; done"
                }),
                &ctx,
                &progress,
            )
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("line-6"), "final output complete");
        let partials = got.lock().clone();
        assert!(
            !partials.is_empty(),
            "a slow multi-line command must produce partial previews"
        );
        for p in &partials {
            assert!(p.lines().count() <= PREVIEW_LINES, "line cap: {p:?}");
            assert!(p.len() <= PREVIEW_BYTES, "byte cap: {p:?}");
            assert!(!p.contains('\x1b'), "ansi stripped: {p:?}");
            assert!(p.contains("line-"), "output content: {p:?}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cap_preview_strips_ansi_and_keeps_tail() {
        let text = strip_ansi("\x1b[31mred\x1b[0m plain \x1b]0;title\x07end");
        assert_eq!(text, "red plain end");
        let ten: String = (1..=10).map(|i| format!("line-{i}\n")).collect();
        let capped = cap_preview(&ten);
        let lines: Vec<&str> = capped.lines().collect();
        assert_eq!(lines.len(), PREVIEW_LINES, "{capped:?}");
        assert_eq!(lines.first(), Some(&"line-3"), "keeps the tail");
        assert_eq!(lines.last(), Some(&"line-10"));
        let long = format!("x{}y", "é".repeat(1_000));
        let capped = cap_preview(&long);
        assert!(capped.len() <= PREVIEW_BYTES, "byte cap on one long line");
        assert!(capped.ends_with('y'), "tail preserved");
    }

    use parking_lot::Mutex;

    use super::*;
    use crate::hands::{Ledger, Spill};

    fn ctx_for(dir: &std::path::Path) -> HandContext {
        HandContext {
            cwd: dir.to_path_buf(),
            ledger: Arc::new(Mutex::new(Ledger::default())),
            spill: Arc::new(Spill::new()),
            snapshots: Arc::new(parking_lot::Mutex::new(
                crate::hands::snapshots::Snapshots::inert(),
            )),
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
