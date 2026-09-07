//! The bash hand: guarded shell execution with timeout, process-tree kill,
//! and capped output with spill parking. Analysis/gating happens in the
//! engine (see `bashp`); this hand only executes what was approved.

use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

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
    ///
    /// A command still running after `ctx.bash_background_ms` (> 0) is
    /// promoted to a job in the shared [`crate::hands::jobs::JobTable`]:
    /// it keeps running, its output streams into a spill file, and a
    /// normal tool result points the model at the `jobs` tool.
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

            // Detached-capable spawn: output goes straight into a spill
            // file, the exit code lands in `<spill>.done`, and the child
            // runs in its own process group so it survives session exit.
            let (spill_path, _ptr) = match ctx.spill.slot() {
                Ok(s) => s,
                Err(e) => {
                    return ToolOutput::err(format!(
                        "bash: cannot create output spill: {e}\n{command}"
                    ));
                }
            };
            let done_path = super::jobs::done_path(&spill_path);
            let script = format!(
                "( {command} ) > {} 2>&1; echo $? > {}",
                sh_quote(&spill_path.to_string_lossy()),
                sh_quote(&done_path.to_string_lossy()),
            );
            let mut cmd = Command::new("sh");
            cmd.arg("-c")
                .arg(&script)
                .arg("sh")
                .current_dir(&cwd)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            #[cfg(unix)]
            cmd.process_group(0);

            let mut child = match cmd.spawn() {
                Ok(c) => c,
                Err(e) => return ToolOutput::err(format!("bash spawn: {e}")),
            };
            let pid = child.id();
            let _started = Instant::now();
            let started_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            // Abort/timeout safety: while this future is alive the guard
            // kills the process tree on drop (abort cancels in-flight
            // tool futures; the hard timeout path below also hits it).
            let mut guard = KillGuard { pid, armed: true };

            // Run: spill-tail previews vs child exit vs the
            // auto-background threshold, all under the hard timeout.
            enum Phase {
                Exited,
                Promote,
            }
            let background_ms = ctx.bash_background_ms;
            let run = async {
                let mut ticker = tokio::time::interval(Duration::from_millis(PREVIEW_TICK_MS));
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                let bg = tokio::time::sleep(Duration::from_millis(background_ms));
                tokio::pin!(bg);
                loop {
                    tokio::select! {
                        _ = ticker.tick() => {
                            let tail = super::jobs::tail_of(&spill_path);
                            if !tail.is_empty() {
                                progress(cap_preview(&tail));
                            }
                        }
                        _ = child.wait() => break Phase::Exited,
                        _ = &mut bg, if background_ms > 0 => break Phase::Promote,
                    }
                }
            };
            let outcome = tokio::time::timeout(Duration::from_millis(timeout_ms), run).await;
            match outcome {
                // hard timeout: the still-armed drop guard kills the tree
                Err(_) => {
                    ctx.ledger.lock().invalidate_all();
                    ToolOutput::err(format!(
                        "bash: timed out after {timeout_ms}ms (killed):\n{command}"
                    ))
                }
                Ok(Phase::Promote) => {
                    // the command keeps running detached; ka may even exit
                    guard.disarm();
                    let id =
                        ctx.jobs
                            .register(command.to_string(), started_ms, spill_path.clone(), pid);
                    ctx.ledger.lock().invalidate_all();
                    ToolOutput {
                        content: format!(
                            "backgrounded as job {id} — keeps running detached; poll with jobs"
                        ),
                        is_error: false,
                        spill: None,
                        images: Vec::new(),
                    }
                }
                Ok(Phase::Exited) => {
                    guard.disarm();
                    ctx.ledger.lock().invalidate_all();
                    let text = std::fs::read_to_string(&spill_path).unwrap_or_default();
                    // the inner command's code comes from the .done marker
                    // (the outer shell itself exits with echo's status)
                    let code = std::fs::read_to_string(super::jobs::done_path(&spill_path))
                        .ok()
                        .and_then(|t| t.trim().parse::<i32>().ok());
                    let is_error = code.map(|c| c != 0).unwrap_or(true);
                    let capped = cap_output(ctx, &text);
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
                            images: Vec::new(),
                        }
                    } else {
                        ToolOutput {
                            content: format!("{header}{}", capped.text),
                            is_error,
                            spill: None,
                            images: Vec::new(),
                        }
                    }
                }
            }
        })
    }
}

/// Drop guard for a spawned child: while armed, dropping kills the whole
/// process tree. Covers every early return and — crucially — the abort
/// path, where the awaiting future is dropped mid-flight and nothing
/// else would reap the child.
struct KillGuard {
    pid: Option<u32>,
    armed: bool,
}

impl KillGuard {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for KillGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Some(pid) = self.pid {
            #[cfg(unix)]
            kill_tree(pid);
        }
    }
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

/// Single-quote a path for safe shell embedding.
fn sh_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

pub(crate) fn kill_tree(pid: u32) {
    // Bash children run in their own process group (process_group(0)),
    // so a negative-pid kill sweeps the entire tree (shell + any nested
    // subshells + grandchildren) and can never hit the caller's own
    // group. The one-level pkill/direct kill remains as fallback for
    // anything spawned without its own group.
    let script = format!(
        "kill -9 -{pid} 2>/dev/null; pkill -9 -P {pid} 2>/dev/null; kill -9 {pid} 2>/dev/null; true"
    );
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
    use crate::hands::jobs::JobTable;
    use crate::hands::{Ledger, Spill};

    fn ctx_for(dir: &std::path::Path) -> HandContext {
        HandContext {
            cwd: dir.to_path_buf(),
            ledger: Arc::new(Mutex::new(Ledger::default())),
            spill: Arc::new(Spill::new()),
            snapshots: Arc::new(parking_lot::Mutex::new(
                crate::hands::snapshots::Snapshots::inert(),
            )),
            jobs: Arc::new(JobTable::new()),
            bash_background_ms: 0,
            max_image_mb: 5,
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

    /// Is `pid` alive? (zombie-aware via the jobs helper)
    fn pid_alive(pid: u32) -> bool {
        crate::hands::jobs::pid_alive(Some(pid))
    }

    /// Poll until `pred` holds (or `secs` elapse).
    async fn wait_for(pred: impl Fn() -> bool, secs: u64) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_secs(secs);
        while std::time::Instant::now() < deadline {
            if pred() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        pred()
    }

    #[tokio::test]
    async fn drop_guard_kills_child_when_future_dropped() {
        let dir = std::env::temp_dir().join(format!("ka-bash-drop-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = ctx_for(&dir);
        let pidfile = dir.join("sleeper.pid");
        let cmd = format!("sleep 30 & echo $! > {}; wait", pidfile.display());
        let hand = BashHand;
        let args = json!({"command": cmd});
        let noop = |_: String| {};
        let task = tokio::spawn(async move { hand.execute_streaming(&args, &ctx, &noop).await });
        // wait for the sleeper to publish its pid
        assert!(
            wait_for(|| pidfile.metadata().is_ok(), 5).await,
            "sleeper must start"
        );
        let sleeper: u32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(pid_alive(sleeper));
        // drop the awaiting future: the guard must kill the whole tree
        task.abort();
        let _ = task.await;
        assert!(
            wait_for(|| !pid_alive(sleeper), 5).await,
            "dropped bash future must not orphan the child (pid {sleeper})"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn background_promotes_running_command_into_a_job() {
        let dir = std::env::temp_dir().join(format!("ka-bash-bg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut ctx = ctx_for(&dir);
        ctx.bash_background_ms = 80;
        let jobs = ctx.jobs.clone();

        let out = BashHand
            .execute(&json!({"command": "echo started; sleep 30"}), &ctx)
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(
            out.content.contains("backgrounded as job 1"),
            "{}",
            out.content
        );
        assert!(out.content.contains("poll with jobs"), "{}", out.content);
        // the call returned well before the 30s child finished
        let snap = jobs.snapshot();
        assert_eq!(snap.len(), 1, "exactly one job registered");
        assert_eq!(snap[0].id, 1);
        assert_eq!(snap[0].state, crate::hands::jobs::JobState::Running);
        assert_eq!(snap[0].cmd, "echo started; sleep 30");
        // output streams into the spill file (generous window: the
        // watcher ticks at 300ms but WSL2 spawns can lag far behind)
        assert!(
            wait_for(
                || {
                    std::fs::read_to_string(&snap[0].spill)
                        .map(|s| s.contains("started"))
                        .unwrap_or(false)
                },
                30
            )
            .await,
            "spill file must carry streamed output"
        );

        // jobs kill tears the promoted child down; the watcher records exit
        assert!(jobs.kill(1).is_ok());
        assert!(
            wait_for(
                || jobs.snapshot()[0].state != crate::hands::jobs::JobState::Running,
                5
            )
            .await,
            "killed job must reach a terminal state"
        );
        assert!(matches!(
            jobs.snapshot()[0].state,
            crate::hands::jobs::JobState::Exited(_)
        ));
        assert!(jobs.kill(1).is_err(), "double kill must be an error");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn background_off_by_default_and_fast_commands_never_promote() {
        let dir = std::env::temp_dir().join(format!("ka-bash-bg0-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = ctx_for(&dir);
        let out = BashHand
            .execute(&json!({"command": "echo quick"}), &ctx)
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("quick"));
        assert!(
            ctx.jobs.snapshot().is_empty(),
            "threshold 0 disables backgrounding"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
