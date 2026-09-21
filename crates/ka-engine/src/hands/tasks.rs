//! Background delegate tasks: `delegate {background: true}` returns
//! immediately and the agent runs detached; this table tracks it and
//! the `tasks` hand gives the model visibility and result retrieval
//! (claude-code background subagents + TaskOutput, in ka terms).

use std::pin::Pin;
use std::sync::Arc;

use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::{Clearance, Hand, HandContext, HandDef, ToolOutput};

/// Lifecycle state of one background agent task.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentTaskState {
    /// Still running.
    Running,
    /// Finished with this summary.
    Done(String),
    /// Failed with this reason.
    Failed(String),
}

impl AgentTaskState {
    fn label(&self) -> &'static str {
        match self {
            AgentTaskState::Running => "running",
            AgentTaskState::Done(_) => "done",
            AgentTaskState::Failed(_) => "failed",
        }
    }
}

/// One tracked background task.
struct AgentTask {
    id: u64,
    agent: String,
    task: String,
    started: std::time::Instant,
    state: AgentTaskState,
    /// The spawned runner (attached right after registration); cancel
    /// aborts it for real.
    handle: Option<tokio::task::JoinHandle<()>>,
    /// Steering channel into the running nested voice: `tasks send`
    /// injects an interjection through it. `None` for finished tasks
    /// and when the runner has not started yet.
    cmds: Option<mpsc::Sender<ka_protocol::Command>>,
    /// Turn cost (USD) accumulated by the runner, once known.
    cost: f64,
    /// The surviving worktree branch of an isolated task (merge source).
    branch: Option<String>,
    /// Messages that arrived for a task that was no longer running —
    /// surfaced as notes, never replayed (no revival in v1).
    inbox: Vec<String>,
}

/// Shared registry of background delegate tasks (engine owns one,
/// DelegateHand registers, TasksHand serves).
pub struct AgentTaskTable {
    inner: parking_lot::Mutex<Vec<AgentTask>>,
    next_id: parking_lot::Mutex<u64>,
}

impl Default for AgentTaskTable {
    fn default() -> Self {
        Self {
            inner: parking_lot::Mutex::new(Vec::new()),
            next_id: parking_lot::Mutex::new(0),
        }
    }
}

impl AgentTaskTable {
    /// An empty table (tests).
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Register a starting task; returns its 1-based id.
    pub fn register(&self, agent: &str, task: &str) -> u64 {
        let mut next = self.next_id.lock();
        *next += 1;
        let id = *next;
        drop(next);
        self.inner.lock().push(AgentTask {
            id,
            agent: agent.to_string(),
            task: task.to_string(),
            started: std::time::Instant::now(),
            state: AgentTaskState::Running,
            handle: None,
            cmds: None,
            cost: 0.0,
            branch: None,
            inbox: Vec::new(),
        });
        id
    }

    /// Attach the spawned runner to a registered task (cancel needs it).
    pub fn attach(&self, id: u64, handle: tokio::task::JoinHandle<()>) {
        if let Some(e) = self.inner.lock().iter_mut().find(|e| e.id == id) {
            e.handle = Some(handle);
        }
    }

    /// Attach the steering channel (the runner's command sender).
    pub fn attach_cmds(&self, id: u64, cmds: mpsc::Sender<ka_protocol::Command>) {
        if let Some(e) = self.inner.lock().iter_mut().find(|e| e.id == id) {
            e.cmds = Some(cmds);
        }
    }

    /// Record the runner's accumulated turn cost (USD).
    pub fn set_cost(&self, id: u64, cost: f64) {
        if let Some(e) = self.inner.lock().iter_mut().find(|e| e.id == id) {
            e.cost = cost;
        }
    }

    /// Record the surviving worktree branch of an isolated task.
    pub fn set_branch(&self, id: u64, branch: String) {
        if let Some(e) = self.inner.lock().iter_mut().find(|e| e.id == id) {
            e.branch = Some(branch);
        }
    }

    /// The surviving worktree branch, if this was an isolated task.
    pub fn branch(&self, id: u64) -> Option<String> {
        self.inner
            .lock()
            .iter()
            .find(|e| e.id == id)
            .and_then(|e| e.branch.clone())
    }

    /// Steer a running task (an interjection its next step honors) or,
    /// for a finished task, record the message as a note — no revival.
    pub fn send(&self, id: u64, text: &str) -> Result<String, String> {
        let mut t = self.inner.lock();
        let e = t
            .iter_mut()
            .find(|e| e.id == id)
            .ok_or_else(|| format!("no such task: t-{id}"))?;
        match &e.state {
            AgentTaskState::Running => match &e.cmds {
                Some(cmds) => {
                    let cmds = cmds.clone();
                    drop(t);
                    let sent = cmds.try_send(ka_protocol::Command::Interject {
                        text: text.to_string(),
                    });
                    match sent {
                        Ok(()) => Ok(format!("t-{id} steered")),
                        Err(_) => Err(format!(
                            "t-{id}'s queue is full or closed — try again or cancel it"
                        )),
                    }
                }
                None => Err(format!("t-{id} cannot be steered (no live channel)")),
            },
            other => {
                let note = format!("[message for t-{id} while {}: {}]", other.label(), text);
                e.inbox.push(note.clone());
                Err(format!(
                    "t-{id} already {} — the message is recorded and surfaces via tasks read",
                    other.label()
                ))
            }
        }
    }

    /// A task's recorded messages (notes for a finished task).
    pub fn inbox(&self, id: u64) -> Option<Vec<String>> {
        self.inner
            .lock()
            .iter()
            .find(|e| e.id == id)
            .map(|e| e.inbox.clone())
    }

    /// Record a task's outcome. First write wins: a cancelled task's
    /// late runner result never overwrites the cancellation.
    pub fn finish(&self, id: u64, outcome: Result<String, String>) {
        let mut t = self.inner.lock();
        if let Some(entry) = t
            .iter_mut()
            .find(|e| e.id == id && e.state == AgentTaskState::Running)
        {
            entry.state = match outcome {
                Ok(summary) => AgentTaskState::Done(summary),
                Err(reason) => AgentTaskState::Failed(reason),
            };
        }
    }

    /// Rendering rows for listings (`id | state | elapsed | agent | task`).
    pub fn rows(&self) -> Vec<String> {
        self.rows_except(None)
    }

    /// Rows for every task except `exclude` — the sibling roster a
    /// spawned delegate sees.
    pub fn rows_except(&self, exclude: Option<u64>) -> Vec<String> {
        let t = self.inner.lock();
        t.iter()
            .filter(|e| Some(e.id) != exclude)
            .map(|e| {
                let head: String = e.task.chars().take(60).collect();
                let cost = if e.cost > 0.0 {
                    format!("  ${:.4}", e.cost)
                } else {
                    String::new()
                };
                format!(
                    "t-{}  {:7}  {:5}  {} — {}{cost}",
                    e.id,
                    e.state.label(),
                    elapsed(e.started),
                    e.agent,
                    head
                )
            })
            .collect()
    }

    /// A task's full result (Done) or its status (Running/Failed).
    pub fn result(&self, id: u64) -> Option<String> {
        let t = self.inner.lock();
        let e = t.iter().find(|e| e.id == id)?;
        Some(match &e.state {
            AgentTaskState::Running => format!(
                "t-{id} still running ({} elapsed, agent {})",
                elapsed(e.started),
                e.agent
            ),
            AgentTaskState::Done(summary) => summary.clone(),
            AgentTaskState::Failed(reason) => format!("t-{id} failed: {reason}"),
        })
    }

    /// Abort a still-running task's runner and mark it cancelled. An
    /// isolated agent aborted mid-run leaves its worktree behind (the
    /// branch keeps whatever was committed before the abort).
    pub fn cancel(&self, id: u64) -> Result<String, String> {
        let mut t = self.inner.lock();
        let e = t
            .iter_mut()
            .find(|e| e.id == id)
            .ok_or_else(|| format!("no such task: t-{id}"))?;
        match e.state.clone() {
            AgentTaskState::Running => {
                if let Some(h) = e.handle.take() {
                    h.abort();
                }
                e.state = AgentTaskState::Failed("cancelled by request".to_string());
                Ok(format!("t-{id} cancelled"))
            }
            other => Err(format!(
                "t-{id} already {} — nothing to cancel",
                other.label()
            )),
        }
    }
}

/// Compact elapsed ("42s", "3m07s").
fn elapsed(started: std::time::Instant) -> String {
    let secs = started.elapsed().as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m{:02}s", secs / 60, secs % 60)
    }
}

/// Merge an isolated task's surviving branch into the current tree:
/// clean-only patch apply (`git apply --check` before `git apply`). On
/// conflict nothing is applied — the patch is written into `.ka/` and
/// the error names it. The result stays uncommitted for review.
fn merge_branch(cwd: &std::path::Path, id: u64, branch: &str) -> Result<String, String> {
    let git = |args: &[&str]| -> Result<std::process::Output, String> {
        std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .map_err(|e| format!("git: {e}"))
    };
    let base = String::from_utf8_lossy(&git(&["merge-base", "HEAD", branch])?.stdout)
        .trim()
        .to_string();
    if base.is_empty() {
        return Err(format!("no merge base between HEAD and {branch}"));
    }
    let diff = git(&["diff", &format!("{base}..{branch}")])?;
    if !diff.status.success() {
        return Err(format!(
            "git diff failed: {}",
            String::from_utf8_lossy(&diff.stderr)
        ));
    }
    let patch = diff.stdout;
    if patch.is_empty() {
        return Ok(format!("branch {branch} is empty — nothing to merge"));
    }
    let feed = |check: bool| -> Result<std::process::Output, String> {
        let mut args = vec!["apply", "--whitespace=nowarn"];
        if check {
            args.push("--check");
        }
        let mut child = std::process::Command::new("git")
            .args(&args)
            .current_dir(cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("git apply: {e}"))?;
        {
            use std::io::Write;
            child
                .stdin
                .as_mut()
                .ok_or_else(|| "git apply: no stdin".to_string())?
                .write_all(&patch)
                .map_err(|e| format!("git apply: {e}"))?;
        }
        child
            .wait_with_output()
            .map_err(|e| format!("git apply: {e}"))
    };
    let check = feed(true)?;
    if !check.status.success() {
        // the root project's .ka/, so a session launched from a
        // subdirectory still drops the patch in one findable place
        let patch_path = crate::project_root(cwd)
            .join(".ka")
            .join(format!("t-{id}.patch"));
        if let Some(parent) = patch_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(&patch_path, &patch).map_err(|e| format!("saving the patch: {e}"))?;
        return Err(format!(
            "branch {branch} does not apply cleanly onto the current tree — the patch is \
             saved at {}; resolve it there and apply manually",
            patch_path.display()
        ));
    }
    let apply = feed(false)?;
    if !apply.status.success() {
        return Err(format!(
            "git apply failed: {}",
            String::from_utf8_lossy(&apply.stderr)
        ));
    }
    Ok(format!(
        "merged {branch} into the working tree (uncommitted — review the diff, then commit)"
    ))
}

/// The tasks tool: visibility over background delegate work.
pub struct TasksHand {
    table: Arc<AgentTaskTable>,
}

impl TasksHand {
    /// Hand over the shared table.
    pub fn new(table: Arc<AgentTaskTable>) -> Self {
        Self { table }
    }
}

impl Hand for TasksHand {
    fn def(&self) -> HandDef {
        HandDef {
            name: "tasks".to_string(),
            description: "Track background delegate tasks (started via delegate with \
                background: true). {\"action\": \"list\"} shows every task; \
                {\"action\": \"read\", \"id\": N} returns a finished task's full result \
                (or its live status); {\"action\": \"send\", \"id\": N, \"text\": \"...\"} \
                steers a running task (an interjection it honors next step); \
                {\"action\": \"inbox\", \"id\": N} shows recorded messages; \
                {\"action\": \"merge\", \"id\": N} applies an isolated task's surviving \
                branch as a clean-only patch (conflict: the patch path is returned, \
                nothing applied); {\"action\": \"cancel\", \"id\": N} stops one. \
                Poll list after starting background work and read results when done."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["list", "read", "send", "inbox", "merge", "cancel"],
                        "description": "list (default), read, send, inbox, merge, or cancel"
                    },
                    "id": { "type": "integer", "description": "Task id" },
                    "text": { "type": "string", "description": "Steering message (send)" }
                }
            }),
            clearance: Clearance::Read,
            read_only: false,
        }
    }

    /// Listing and reading are Read; steering and merging mutate another
    /// agent's context or the working tree (Write: asks in guarded
    /// mode); cancel stays at the hand's Read tier (abort, not mutate).
    fn clearance_for(&self, args: &Value) -> Clearance {
        match args.get("action").and_then(Value::as_str) {
            Some("send") | Some("merge") => Clearance::Write,
            _ => Clearance::Read,
        }
    }

    fn execute<'a>(
        &'a self,
        args: &'a Value,
        ctx: &'a HandContext,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>> {
        Box::pin(async move {
            match args.get("action").and_then(Value::as_str).unwrap_or("list") {
                "list" => {
                    let rows = self.table.rows();
                    if rows.is_empty() {
                        return ToolOutput::ok("no background tasks".to_string());
                    }
                    ToolOutput::ok(rows.join("\n"))
                }
                "read" => {
                    let Some(id) = args.get("id").and_then(Value::as_u64) else {
                        return ToolOutput::err("tasks read: missing required 'id'");
                    };
                    match self.table.result(id) {
                        Some(text) => {
                            let capped: String = text.chars().take(8_000).collect();
                            ToolOutput::ok(capped)
                        }
                        None => ToolOutput::err(format!("no such task: t-{id}")),
                    }
                }
                "send" => {
                    let (Some(id), Some(text)) = (
                        args.get("id").and_then(Value::as_u64),
                        args.get("text").and_then(Value::as_str),
                    ) else {
                        return ToolOutput::err("tasks send: missing required 'id' and 'text'");
                    };
                    match self.table.send(id, text) {
                        Ok(msg) => ToolOutput::ok(msg),
                        Err(e) => ToolOutput::err(e),
                    }
                }
                "inbox" => {
                    let Some(id) = args.get("id").and_then(Value::as_u64) else {
                        return ToolOutput::err("tasks inbox: missing required 'id'");
                    };
                    match self.table.inbox(id) {
                        Some(messages) if messages.is_empty() => {
                            ToolOutput::ok(format!("t-{id}: inbox empty"))
                        }
                        Some(messages) => ToolOutput::ok(messages.join("\n")),
                        None => ToolOutput::err(format!("no such task: t-{id}")),
                    }
                }
                "merge" => {
                    let Some(id) = args.get("id").and_then(Value::as_u64) else {
                        return ToolOutput::err("tasks merge: missing required 'id'");
                    };
                    match self.table.branch(id) {
                        Some(branch) => match merge_branch(&ctx.cwd, id, &branch) {
                            Ok(msg) => ToolOutput::ok(format!("tasks: {msg}")),
                            Err(e) => ToolOutput::err(format!("tasks merge: {e}")),
                        },
                        None => ToolOutput::err(format!(
                            "t-{id} has no isolated worktree branch — merge applies to \
                             isolate: true tasks only"
                        )),
                    }
                }
                "cancel" => {
                    let Some(id) = args.get("id").and_then(Value::as_u64) else {
                        return ToolOutput::err("tasks cancel: missing required 'id'");
                    };
                    match self.table.cancel(id) {
                        Ok(msg) => ToolOutput::ok(msg),
                        Err(e) => ToolOutput::err(e),
                    }
                }
                other => ToolOutput::err(format!(
                    "tasks: unknown action {other:?} (expected \"list\", \"read\", \"send\", \
                     \"inbox\", \"merge\", or \"cancel\")"
                )),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn ctx_for() -> HandContext {
        HandContext {
            cwd: std::env::temp_dir(),
            ledger: Arc::new(parking_lot::Mutex::new(super::super::Ledger::default())),
            spill: Arc::new(super::super::Spill::new()),
            snapshots: Arc::new(parking_lot::Mutex::new(
                crate::hands::snapshots::Snapshots::inert(),
            )),
            jobs: Arc::new(crate::hands::jobs::JobTable::new()),
            bash_background_ms: 0,
            max_image_mb: 5,
            web_allow_private: false,
            sandbox: ka_sandbox::Policy::Off,
        }
    }

    #[tokio::test]
    async fn register_finish_list_read_cancel() {
        let table = AgentTaskTable::new();
        let ctx = ctx_for();
        async fn run(table: &Arc<AgentTaskTable>, args: Value, ctx: &HandContext) -> ToolOutput {
            TasksHand::new(table.clone()).execute(&args, ctx).await
        }

        // empty listing
        let out = run(&table, json!({}), &ctx).await;
        assert_eq!(out.content, "no background tasks");

        let id = table.register("reviewer", "audit error handling across crates");
        assert_eq!(id, 1);
        let out = run(&table, json!({}), &ctx).await;
        assert!(out.content.contains("t-1"), "{}", out.content);
        assert!(out.content.contains("running"), "{}", out.content);
        assert!(out.content.contains("reviewer"), "{}", out.content);

        // live status
        let out = run(&table, json!({"action": "read", "id": 1}), &ctx).await;
        assert!(out.content.contains("still running"), "{}", out.content);

        // finish + full result
        table.finish(1, Ok("3 findings: ...".to_string()));
        let out = run(&table, json!({"action": "read", "id": 1}), &ctx).await;
        assert!(out.content.contains("3 findings"), "{}", out.content);
        let out = run(&table, json!({}), &ctx).await;
        assert!(out.content.contains("done"), "{}", out.content);

        // cancel a running one; cancelling a finished one errors
        let id2 = table.register("scout", "find callers");
        let out = run(&table, json!({"action": "cancel", "id": id2}), &ctx).await;
        assert!(out.content.contains("cancelled"), "{}", out.content);
        let out = run(&table, json!({"action": "cancel", "id": id2}), &ctx).await;
        assert!(out.is_error, "{}", out.content);

        // unknown ids and actions
        let out = run(&table, json!({"action": "read", "id": 99}), &ctx).await;
        assert!(out.is_error);
        let out = run(&table, json!({"action": "read"}), &ctx).await;
        assert!(out.is_error);
        let out = run(&table, json!({"action": "zap"}), &ctx).await;
        assert!(out.is_error);
    }

    #[tokio::test]
    async fn send_steers_running_and_records_for_finished() {
        let table = AgentTaskTable::new();
        let ctx = ctx_for();
        let hand = TasksHand::new(table.clone());
        let id = table.register("worker", "long job");

        // no channel attached yet: steering says so
        let out = hand
            .execute(&json!({"action": "send", "id": id, "text": "hurry"}), &ctx)
            .await;
        assert!(out.is_error, "{}", out.content);

        // with a channel: the steering lands as an Interject command
        let (tx, mut rx) = mpsc::channel(4);
        table.attach_cmds(id, tx);
        let out = hand
            .execute(
                &json!({"action": "send", "id": id, "text": "focus on the parser"}),
                &ctx,
            )
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("steered"), "{}", out.content);
        match rx.recv().await {
            Some(ka_protocol::Command::Interject { text }) => {
                assert_eq!(text, "focus on the parser");
            }
            other => panic!("expected an interjection, got {other:?}"),
        }

        // finished: the message is recorded, never replayed
        table.finish(id, Ok("done".to_string()));
        let out = hand
            .execute(
                &json!({"action": "send", "id": id, "text": "too late"}),
                &ctx,
            )
            .await;
        assert!(out.is_error, "{}", out.content);
        let out = hand
            .execute(&json!({"action": "inbox", "id": id}), &ctx)
            .await;
        assert!(out.content.contains("too late"), "{}", out.content);
    }

    fn git_repo(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ka-tasks-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&dir)
                .output()
                .unwrap()
        };
        assert!(git(&["init"]).status.success());
        std::fs::write(dir.join("f.txt"), "base\n").unwrap();
        assert!(git(&["add", "."]).status.success());
        assert!(
            git(&[
                "-c",
                "user.name=ka",
                "-c",
                "user.email=ka@local",
                "commit",
                "-m",
                "base"
            ])
            .status
            .success()
        );
        dir
    }

    /// tasks merge applies an isolated task's branch as a clean patch;
    /// a conflicting tree is refused with the patch path and nothing
    /// applied.
    #[tokio::test]
    async fn merge_applies_clean_branch_and_refers_conflicts_to_a_patch() {
        let repo = git_repo("merge");
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .output()
                .unwrap()
        };
        // a worker branch with one commit
        assert!(git(&["checkout", "-b", "ka-worker"]).status.success());
        std::fs::write(repo.join("f.txt"), "worker edit\n").unwrap();
        assert!(
            git(&[
                "-c",
                "user.name=ka",
                "-c",
                "user.email=ka@local",
                "commit",
                "-am",
                "work"
            ])
            .status
            .success()
        );
        assert!(git(&["checkout", "-"]).status.success());

        let table = AgentTaskTable::new();
        let id = table.register("iso", "work");
        table.set_branch(id, "ka-worker".to_string());
        let mut ctx = ctx_for();
        ctx.cwd = repo.clone();
        let hand = TasksHand::new(table.clone());

        // no-branch tasks refuse
        let ghost = table.register("plain", "no worktree");
        let out = hand
            .execute(&json!({"action": "merge", "id": ghost}), &ctx)
            .await;
        assert!(out.is_error, "{}", out.content);

        // clean apply: the working tree changes, uncommitted
        let out = hand
            .execute(&json!({"action": "merge", "id": id}), &ctx)
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(
            std::fs::read_to_string(repo.join("f.txt")).unwrap(),
            "worker edit\n"
        );

        // now the patch no longer applies: conflict → patch path
        let out = hand
            .execute(&json!({"action": "merge", "id": id}), &ctx)
            .await;
        assert!(out.is_error, "{}", out.content);
        assert!(out.content.contains(".patch"), "{}", out.content);
        assert!(
            repo.join(".ka/t-{id}.patch").exists()
                || repo.join(format!(".ka/t-{id}.patch")).exists()
        );
        assert_eq!(
            std::fs::read_to_string(repo.join("f.txt")).unwrap(),
            "worker edit\n",
            "conflict applies nothing"
        );
        let _ = std::fs::remove_dir_all(&repo);
    }
}
