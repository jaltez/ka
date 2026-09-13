//! Background delegate tasks: `delegate {background: true}` returns
//! immediately and the agent runs detached; this table tracks it and
//! the `tasks` hand gives the model visibility and result retrieval
//! (claude-code background subagents + TaskOutput, in ka terms).

use std::pin::Pin;
use std::sync::Arc;

use serde_json::{Value, json};

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
        });
        id
    }

    /// Attach the spawned runner to a registered task (cancel needs it).
    pub fn attach(&self, id: u64, handle: tokio::task::JoinHandle<()>) {
        if let Some(e) = self.inner.lock().iter_mut().find(|e| e.id == id) {
            e.handle = Some(handle);
        }
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
        let t = self.inner.lock();
        t.iter()
            .map(|e| {
                let head: String = e.task.chars().take(60).collect();
                format!(
                    "t-{}  {:7}  {:5}  {} — {}",
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
                (or its live status); {\"action\": \"cancel\", \"id\": N} stops one. \
                Poll list after starting background work and read results when done."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["list", "read", "cancel"],
                        "description": "list (default), read, or cancel"
                    },
                    "id": { "type": "integer", "description": "Task id (read/cancel)" }
                }
            }),
            clearance: Clearance::Read,
            read_only: true,
        }
    }

    fn execute<'a>(
        &'a self,
        args: &'a Value,
        _ctx: &'a HandContext,
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
                    "tasks: unknown action {other:?} (expected \"list\", \"read\", or \"cancel\")"
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
}
