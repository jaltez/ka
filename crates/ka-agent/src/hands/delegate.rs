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
use crate::voice::Voice;

/// The delegate tool: one hand over every discovered agent.
pub struct DelegateHand {
    agents: Vec<AgentDef>,
    /// Shared catalog/model bootstrap (the engine-owned pathfinder slot —
    /// the single source of truth for the nested voices' speaker).
    source: Arc<parking_lot::RwLock<super::pathfinder::PathfinderSource>>,
}

impl DelegateHand {
    /// New hand over discovered agents and the shared subagent source.
    pub fn new(
        agents: Vec<AgentDef>,
        source: Arc<parking_lot::RwLock<super::pathfinder::PathfinderSource>>,
    ) -> Self {
        Self { agents, source }
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

            let mut voice = Voice::new_readonly(
                source.catalog,
                ctx.cwd.clone(),
                ka_protocol::Mode::Free,
                def.max_steps,
            );
            voice.set_model_selector(&model, 4.0);
            let prompt = format!("{}\n\nTask: {}", def.system, task);

            let (cmd_tx, mut cmd_rx) = mpsc::channel(16);
            let (evt_tx, mut evt_rx) = mpsc::channel(256);
            let handle = tokio::spawn(async move {
                let mut interjections = Vec::new();
                let mut deferrals = std::collections::VecDeque::new();
                voice
                    .turn(
                        &model,
                        prompt,
                        &mut cmd_rx,
                        &evt_tx,
                        &mut interjections,
                        &mut deferrals,
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
            ToolOutput::ok(summary)
        })
    }
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
            },
            AgentDef {
                name: "scout".to_string(),
                description: String::new(),
                system: "You scout.".to_string(),
                max_steps: 12,
            },
        ];
        DelegateHand::new(
            agents,
            std::sync::Arc::new(parking_lot::RwLock::new(PathfinderSource::default())),
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
}
