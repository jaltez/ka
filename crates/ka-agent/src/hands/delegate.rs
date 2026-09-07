//! The delegate tool: run a user-defined markdown agent as a subagent.
//! Same machinery as pathfinder — a nested read-only voice with the
//! agent's markdown body as its system prompt — generalized over
//! `.ka/agents/*.md` definitions.

use std::pin::Pin;

use Future;
use std::sync::Arc;

use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::{Clearance, Hand, HandContext, HandDef, ToolOutput};
use crate::agents::AgentDef;
use crate::voice::{GuardRuntime, Voice};

/// The delegate tool: one hand over every discovered agent.
pub struct DelegateHand {
    agents: Vec<AgentDef>,
    /// Shared catalog/model bootstrap (the engine-owned pathfinder slot —
    /// the single source of truth for the nested voices' speaker).
    source: Arc<parking_lot::RwLock<super::pathfinder::PathfinderSource>>,
    /// The parent session's permission mode (gates isolated agents).
    parent_mode: ka_protocol::Mode,
}

impl DelegateHand {
    /// New hand over discovered agents and the shared subagent source.
    pub fn new(
        agents: Vec<AgentDef>,
        source: Arc<parking_lot::RwLock<super::pathfinder::PathfinderSource>>,
        parent_mode: ka_protocol::Mode,
    ) -> Self {
        Self {
            agents,
            source,
            parent_mode,
        }
    }

    fn find(&self, name: &str) -> Option<&AgentDef> {
        self.agents.iter().find(|a| a.name == name)
    }
}

impl Hand for DelegateHand {
    fn def(&self) -> HandDef {
        let mut listing = String::from(
            "Delegate a self-contained subtask to a named subagent. Available agents:\n",
        );
        for a in &self.agents {
            let desc = if a.description.is_empty() {
                "(no description)"
            } else {
                &a.description
            };
            listing.push_str(&format!("- {}: {desc}\n", a.name));
        }
        listing.push_str("The agent runs with read-only tools and returns a dense summary.");
        HandDef {
            name: "delegate".to_string(),
            description: listing,
            parameters: json!({
                "type": "object",
                "properties": {
                    "agent": {
                        "type": "string",
                        "enum": self.agents.iter().map(|a| a.name.clone()).collect::<Vec<_>>(),
                        "description": "Which agent to run"
                    },
                    "task": {
                        "type": "string",
                        "description": "The complete, self-contained task for the agent"
                    }
                },
                "required": ["agent", "task"]
            }),
            clearance: Clearance::Read,
            read_only: true,
        }
    }

    fn execute<'a>(
        &'a self,
        args: &'a Value,
        ctx: &'a HandContext,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>> {
        Box::pin(async move {
            let Some(agent_name) = args.get("agent").and_then(Value::as_str) else {
                return ToolOutput::err("delegate: missing required 'agent'");
            };
            let Some(task) = args.get("task").and_then(Value::as_str) else {
                return ToolOutput::err("delegate: missing required 'task'");
            };
            let Some(def) = self.find(agent_name) else {
                let known: Vec<String> = self.agents.iter().map(|a| a.name.clone()).collect();
                return ToolOutput::err(format!(
                    "delegate: unknown agent '{agent_name}' (known: {})",
                    known.join(", ")
                ));
            };
            let source = self.source.read().clone();
            let Some(model) = source.model else {
                return ToolOutput::err("delegate: no model configured for the parent session");
            };

            // isolated agents write in a throwaway git worktree on their
            // own branch: they need a repo and write-mode permission
            let mut worktree: Option<std::path::PathBuf> = None;
            if def.isolate {
                if !matches!(
                    self.parent_mode,
                    ka_protocol::Mode::AcceptEdits | ka_protocol::Mode::Free
                ) {
                    return ToolOutput::err(
                        "delegate: isolated agents need write access — switch to accept_edits or free mode first (/mode)",
                    );
                }
                match create_worktree(&ctx.cwd, &format!("ka-{agent_name}")) {
                    Ok(path) => worktree = Some(path),
                    Err(e) => return ToolOutput::err(e),
                }
            }
            let agent_cwd = worktree.clone().unwrap_or_else(|| ctx.cwd.clone());

            let prompt = format!("{}\n\nTask: {}", def.system, task);
            let (cmd_tx, mut cmd_rx) = mpsc::channel(16);
            let (evt_tx, mut evt_rx) = mpsc::channel(256);
            let isolated = worktree.is_some();
            let max_steps = def.max_steps;
            let handle = tokio::spawn(async move {
                let mut interjections = Vec::new();
                let mut deferrals = std::collections::VecDeque::new();
                let mut voice = if isolated {
                    // isolated agents may write — inside the worktree only
                    Voice::new(
                        source.catalog,
                        agent_cwd,
                        ka_protocol::Mode::Free,
                        max_steps,
                    )
                } else {
                    Voice::new_readonly(
                        source.catalog,
                        agent_cwd,
                        ka_protocol::Mode::Free,
                        max_steps,
                    )
                };
                voice.set_model_selector(&model, 4.0);
                voice
                    .turn(
                        &model,
                        prompt,
                        &mut cmd_rx,
                        &evt_tx,
                        &mut interjections,
                        &mut deferrals,
                        &mut GuardRuntime::default(),
                        None,
                        Vec::new(),
                    )
                    .await;
            });

            let mut summary = String::new();
            let mut thought = String::new();
            let mut failed: Option<String> = None;
            // 10-minute cap, same budget as pathfinder
            let deadline = tokio::time::timeout(std::time::Duration::from_secs(600), async {
                while let Some(evt) = evt_rx.recv().await {
                    match evt {
                        ka_protocol::Event::Delta {
                            kind: ka_protocol::DeltaKind::Text(t),
                        } => summary.push_str(&t),
                        ka_protocol::Event::Delta {
                            kind: ka_protocol::DeltaKind::Thought(t),
                        } => thought.push_str(&t),
                        ka_protocol::Event::Error { message, .. } => failed = Some(message),
                        ka_protocol::Event::TurnFinished { .. } => break,
                        _ => {}
                    }
                }
            })
            .await;
            drop(cmd_tx);
            let _ = handle.await;

            if !matches!(deadline, Ok(())) {
                return ToolOutput::err("delegate: agent timed out (10m)");
            }
            if summary.trim().is_empty() && !thought.trim().is_empty() {
                summary = thought; // thinking models: reason-only replies
            }
            if summary.trim().is_empty() {
                return ToolOutput::err(format!(
                    "agent {agent_name} failed: {}",
                    failed.unwrap_or_else(|| "no summary produced".to_string())
                ));
            }
            if let Some(wt) = &worktree {
                match finish_worktree(&ctx.cwd, wt, agent_name) {
                    Ok(branch) => summary.push_str(&format!(
                        "\n\n(isolated worktree: changes live on branch `{branch}`)"
                    )),
                    Err(e) => return ToolOutput::err(e),
                }
            }
            ToolOutput::ok(summary)
        })
    }
}
/// Create an isolated git worktree on its own branch under the state
/// dir. `Err` when the cwd is not a git repository.
fn create_worktree(cwd: &std::path::Path, name: &str) -> Result<std::path::PathBuf, String> {
    let git_ok = std::process::Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(cwd)
        .output();
    match git_ok {
        Ok(o) if o.status.success() && o.stdout.starts_with(b"true") => {}
        Ok(o) if o.stdout.starts_with(b"false") => {
            return Err("delegate: isolate requires a git repository (cwd is inside one?)".into());
        }
        _ => {
            return Err("delegate: isolate requires a git repository".into());
        }
    }
    let state = std::env::var("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|_| {
            std::env::var("HOME").map(|h| std::path::PathBuf::from(h).join(".local/state"))
        })
        .unwrap_or_else(|_| std::env::temp_dir());
    let uuid = format!(
        "{}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0),
        std::process::id()
    );
    let path = state.join("ka/worktrees").join(format!("{name}-{uuid}"));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("worktree: {e}"))?;
    }
    let branch = format!("{name}-{uuid}");
    let status = std::process::Command::new("git")
        .args([
            "worktree",
            "add",
            "-b",
            &branch,
            path.to_string_lossy().as_ref(),
            "HEAD",
        ])
        .current_dir(cwd)
        .status()
        .map_err(|e| format!("git worktree: {e}"))?;
    if !status.success() {
        return Err("git worktree add failed".to_string());
    }
    Ok(path)
}

/// Remove the worktree (force) and its directory; the branch keeps any
/// commits the agent made. Returns the branch name for the report.
fn finish_worktree(
    cwd: &std::path::Path,
    path: &std::path::Path,
    name: &str,
) -> Result<String, String> {
    let branch_out = std::process::Command::new("git")
        .args(["branch", "--show-current"])
        .current_dir(path)
        .output()
        .map_err(|e| format!("git branch: {e}"))?;
    let branch = String::from_utf8_lossy(&branch_out.stdout)
        .trim()
        .to_string();
    let _branch = branch;
    let status = std::process::Command::new("git")
        .args(["worktree", "remove", "--force"])
        .arg(path)
        .current_dir(cwd)
        .status()
        .map_err(|e| format!("git worktree remove: {e}"))?;
    if !status.success() {
        let _ = std::fs::remove_dir_all(path);
    }
    Ok(format!("{name}-worktree"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::hands::pathfinder::PathfinderSource;

    fn hand() -> DelegateHand {
        let agents = vec![
            AgentDef {
                name: "reviewer".to_string(),
                description: "reviews diffs".to_string(),
                system: "You review.".to_string(),
                max_steps: 8,
                isolate: false,
            },
            AgentDef {
                name: "scout".to_string(),
                description: String::new(),
                system: "You scout.".to_string(),
                max_steps: 12,
                isolate: false,
            },
        ];
        DelegateHand::new(
            agents,
            std::sync::Arc::new(parking_lot::RwLock::new(PathfinderSource::default())),
            ka_protocol::Mode::Free,
        )
    }

    fn ctx_for() -> HandContext {
        HandContext {
            cwd: std::env::temp_dir(),
            ledger: std::sync::Arc::new(parking_lot::Mutex::new(super::super::Ledger::default())),
            spill: std::sync::Arc::new(super::super::Spill::new()),
            snapshots: std::sync::Arc::new(parking_lot::Mutex::new(
                crate::hands::snapshots::Snapshots::inert(),
            )),
            jobs: std::sync::Arc::new(crate::hands::jobs::JobTable::new()),
            bash_background_ms: 0,
            max_image_mb: 5,
        }
    }

    #[test]
    fn def_lists_every_agent_with_descriptions() {
        let d = hand().def();
        assert_eq!(d.name, "delegate");
        assert_eq!(d.clearance, Clearance::Read);
        assert!(d.read_only);
        assert!(
            d.description.contains("reviewer: reviews diffs"),
            "{}",
            d.description
        );
        assert!(d.description.contains("scout: (no description)"));
        let enum_names = d.parameters["properties"]["agent"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(enum_names, vec!["reviewer", "scout"]);
    }

    #[tokio::test]
    async fn unknown_agent_and_missing_args_error_cleanly() {
        let h = hand();
        let ctx = ctx_for();
        let out = h
            .execute(&serde_json::json!({"agent": "nope", "task": "x"}), &ctx)
            .await;
        assert!(out.is_error);
        assert!(
            out.content.contains("unknown agent 'nope'"),
            "{}",
            out.content
        );
        assert!(out.content.contains("reviewer, scout"));

        let out = h.execute(&serde_json::json!({"task": "x"}), &ctx).await;
        assert!(out.is_error);
        assert!(out.content.contains("missing required 'agent'"));

        let out = h
            .execute(&serde_json::json!({"agent": "reviewer"}), &ctx)
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("missing required 'task'"));
    }

    #[tokio::test]
    async fn delegate_without_model_reports_missing_configuration() {
        let h = hand();
        let ctx = ctx_for();
        let out = h
            .execute(
                &serde_json::json!({"agent": "reviewer", "task": "review x"}),
                &ctx,
            )
            .await;
        assert!(out.is_error);
        assert!(
            out.content.contains("no model configured"),
            "{}",
            out.content
        );
    }

    #[test]
    fn isolate_parse_flag() {
        let a = AgentDef::parse("---\nname: w\nisolate: true\n---\nbody", "w");
        assert!(a.isolate);
        let b = AgentDef::parse("plain body", "b");
        assert!(!b.isolate);
    }

    #[test]
    fn worktree_lifecycle_creates_own_branch_and_cleans_up() {
        let dir = std::env::temp_dir().join(format!("ka-wt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&dir)
                .output()
                .unwrap()
        };
        assert!(run(&["init"]).status.success());
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(dir.join("f.txt"), "one\n").unwrap();
        run(&["add", "."]);
        assert!(run(&["commit", "-m", "init"]).status.success());

        let wt = create_worktree(&dir, "reviewer").expect("worktree created");
        assert!(wt.exists());
        assert!(wt.join(".git").exists());
        // branch check: HEAD of the worktree is on its own branch
        let branch = std::process::Command::new("git")
            .args(["branch", "--show-current"])
            .current_dir(&wt)
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&branch.stdout).contains("reviewer"));

        let name = finish_worktree(&dir, &wt, "reviewer").unwrap();
        assert!(name.contains("reviewer"));
        assert!(!wt.exists(), "worktree dir removed");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
