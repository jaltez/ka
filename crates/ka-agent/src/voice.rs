//! The live voice: multi-step tool loop. Each step speaks with the offered
//! tools, executes returned calls through the Hands registry under
//! clearance gating (hardstops always prompt; headless surfaces deny),
//! feeds results back, and repeats until the model rests or the step cap
//! hits.

use std::collections::{HashMap, HashSet, VecDeque};

use ka_dialect::dialects::{Catalog, Wire};
use ka_dialect::speaker::{
    SpeakRequest, Speaker, StreamEvent, ToolCall, ToolResult, ToolSpec, TurnMessage,
};
use ka_protocol::{AskId, AskQuestion, Command, ErrorClass, Event, Stop, Usage};
use tokio::sync::mpsc;

use crate::hands::bashp::{all_readonly, analyze, hardstop};
use crate::hands::{Clearance, Hand, HandContext, Ledger, Spill, ToolOutput, registry};

/// Session-scoped mutable state the voice needs (owned by the engine).
#[derive(Default)]
pub struct VoiceState {
    /// Session always-allow rules (`tool:<name>`, `bash:<program>`).
    pub rules: HashSet<String>,
    /// Monotonic ask counter.
    pub ask_counter: u32,
    /// Loop-guard counts of (tool, args) signatures this prompt.
    pub loop_counts: HashMap<String, usize>,
}

/// Everything needed to speak to real models and act on the world.
pub struct Voice {
    catalog: Catalog,
    speakers: HashMap<Wire, std::sync::Arc<dyn Speaker>>,
    hands: Vec<Box<dyn Hand>>,
    hand_ctx: HandContext,
    pub(crate) state: VoiceState,
    max_steps: u32,
    mode: ka_protocol::Mode,
}

impl Voice {
    /// New voice over a catalog, working in `cwd`.
    pub fn new(
        catalog: Catalog,
        cwd: std::path::PathBuf,
        mode: ka_protocol::Mode,
        max_steps: u32,
    ) -> Self {
        Self {
            catalog,
            speakers: Default::default(),
            hands: registry(),
            hand_ctx: HandContext {
                cwd,
                ledger: std::sync::Arc::new(parking_lot::Mutex::new(Ledger::default())),
                spill: std::sync::Arc::new(Spill::new()),
            },
            state: VoiceState::default(),
            max_steps,
            mode,
        }
    }

    /// Update the permission mode (engine forwards `SetMode`).
    pub fn set_mode(&mut self, mode: ka_protocol::Mode) {
        self.mode = mode;
    }

    fn speaker(&mut self, wire: Wire) -> std::sync::Arc<dyn Speaker> {
        self.speakers
            .entry(wire)
            .or_insert_with(|| ka_dialect::speaker_for(wire))
            .clone()
    }

    /// Inject a speaker for a wire (tests).
    #[cfg(test)]
    pub fn with_speaker(mut self, wire: Wire, speaker: std::sync::Arc<dyn Speaker>) -> Self {
        self.speakers.insert(wire, speaker);
        self
    }

    fn specs(&self) -> Vec<ToolSpec> {
        self.hands
            .iter()
            .map(|h| {
                let def = h.def();
                ToolSpec {
                    name: def.name.to_string(),
                    description: def.description.clone(),
                    parameters: def.parameters.clone(),
                }
            })
            .collect()
    }

    /// Run one live prompt to completion. Always emits exactly one
    /// `TurnFinished`.
    #[allow(clippy::too_many_arguments)]
    pub async fn turn(
        &mut self,
        model_selector: &str,
        history: &mut Vec<TurnMessage>,
        prompt: String,
        commands: &mut mpsc::Receiver<Command>,
        events: &mpsc::Sender<Event>,
        interjections: &mut Vec<String>,
        deferrals: &mut VecDeque<String>,
    ) {
        use ka_dialect::parse_selector;

        let parsed = match parse_selector(model_selector) {
            Ok(p) => p,
            Err(e) => {
                return finish_after_error(events, ErrorClass::Protocol, &e.to_string()).await;
            }
        };
        let model_id = parsed.model_id();
        let Some(dialect) = self.catalog.get(&model_id).cloned() else {
            return finish_after_error(
                events,
                ErrorClass::Protocol,
                &format!("unknown model {model_id:?} (not in catalog; add a dialect overlay)"),
            )
            .await;
        };
        let price = dialect.price;
        let ratio = if dialect.ratio > 0.0 {
            dialect.ratio
        } else {
            4.0
        };
        let token = dialect
            .api_key_env
            .as_deref()
            .and_then(ka_dialect::auth::resolve_token);
        let window = dialect.context as u64;

        let est_in = (prompt.len() as f64 / ratio as f64).ceil() as u64;
        events
            .send(Event::TurnStarted {
                context: ka_protocol::ContextMeter {
                    used: est_in,
                    window,
                },
            })
            .await
            .ok();

        // Minimal system context: identity + read-only git awareness.
        let snap = crate::hands::git::RepoSnapshot::capture(&self.hand_ctx.cwd);
        let system = format!(
            "You are ka, a precise coding agent. {}. Use the provided tools to inspect and modify the repository; prefer read before edit.",
            snap.summary()
        );

        history.push(TurnMessage::user(prompt));
        self.state.loop_counts.clear();
        let mut usage_total = Usage::default();
        let mut assistant_text = String::new();
        let mut final_stop = Stop::Done;
        let mut steps = 0u32;

        'outer: loop {
            let req = SpeakRequest {
                model_id: model_id.clone(),
                dialect: dialect.clone(),
                effort: parsed.effort.clone(),
                system: system.clone(),
                messages: history.clone(),
                tools: self.specs(),
                token: token.clone(),
                cache_key: None,
            };
            let speaker = self.speaker(dialect.wire);
            let (tx, mut rx) = mpsc::channel::<StreamEvent>(256);
            {
                let speaker = speaker.clone();
                tokio::spawn(async move {
                    speaker.speak(req, tx).await;
                });
            }

            let mut step_calls: Vec<ToolCall> = Vec::new();
            let mut step_text = String::new();
            let mut step_failed: Option<(ErrorClass, String)> = None;
            let mut step_finished = false;

            while !step_finished {
                tokio::select! {
                    biased;
                    maybe_cmd = commands.recv() => {
                        match maybe_cmd {
                            None => return,
                            Some(Command::Abort) => {
                                events.send(Event::TurnFinished {
                                    stop: Stop::Aborted,
                                    usage: Usage::default(),
                                }).await.ok();
                                return;
                            }
                            Some(Command::Interject { text }) => interjections.push(text),
                            Some(Command::Defer { text }) => deferrals.push_back(text),
                            Some(Command::SetMode { mode }) => {
                                self.mode = mode;
                                events.send(Event::ModeChanged { mode }).await.ok();
                            }
                            Some(Command::AlwaysAllow { rule }) => {
                                self.state.rules.insert(rule);
                            }
                            Some(_) => {}
                        }
                    }
                    maybe_evt = rx.recv() => {
                        let Some(evt) = maybe_evt else {
                            // speaker ended without Finished/Failed (aborted)
                            break;
                        };
                        match evt {
                            StreamEvent::Text(t) => {
                                step_text.push_str(&t);
                                assistant_text.push_str(&t);
                                events.send(Event::Delta { kind: ka_protocol::DeltaKind::Text(t) }).await.ok();
                            }
                            StreamEvent::Thought(t) => {
                                events.send(Event::Delta { kind: ka_protocol::DeltaKind::Thought(t) }).await.ok();
                            }
                            StreamEvent::Call(call) => {
                                events.send(Event::CallStarted {
                                    tool: call.tool.clone(),
                                    id: call.id.clone(),
                                }).await.ok();
                                step_calls.push(call);
                            }
                            StreamEvent::Finished { stop, usage } => {
                                usage_total.input += usage.input;
                                usage_total.output += usage.output;
                                usage_total.cache_read += usage.cache_read;
                                usage_total.cache_write += usage.cache_write;
                                final_stop = stop;
                                step_finished = true;
                            }
                            StreamEvent::Failed { class, message, .. } => {
                                step_failed = Some((class, message));
                                final_stop = Stop::Error;
                                step_finished = true;
                            }
                        }
                    }
                }
            }

            if let Some((class, message)) = step_failed {
                events
                    .send(Event::Error {
                        class,
                        retryable: false,
                        message,
                    })
                    .await
                    .ok();
                break 'outer;
            }

            if step_calls.is_empty() || steps >= self.max_steps {
                break 'outer;
            }

            // Execute this step's calls; ordered results.
            history.push(TurnMessage::assistant_with_calls(
                step_text.clone(),
                step_calls.clone(),
            ));
            let mut results: Vec<ToolResult> = Vec::new();
            for call in &step_calls {
                let sig = format!("{}|{}", call.tool, call.arguments);
                *self.state.loop_counts.entry(sig).or_insert(0) += 1;
                if self.state.loop_counts.values().any(|c| *c >= 4) {
                    results.push(ToolResult {
                        call_id: call.id.clone(),
                        content: "loop guard: this tool was called with identical arguments 4+ \
                                  times; stop repeating and reconsider"
                            .to_string(),
                        is_error: true,
                    });
                    events
                        .send(Event::CallFinished {
                            tool: call.tool.clone(),
                            id: call.id.clone(),
                            ok: false,
                        })
                        .await
                        .ok();
                    continue;
                }
                let output = self.gate_and_execute(call, commands, events).await;
                events
                    .send(Event::CallOutput {
                        tool: call.tool.clone(),
                        id: call.id.clone(),
                        excerpt: truncate_excerpt(&output.content),
                        is_error: output.is_error,
                        spill: output.spill.clone(),
                    })
                    .await
                    .ok();
                events
                    .send(Event::CallFinished {
                        tool: call.tool.clone(),
                        id: call.id.clone(),
                        ok: !output.is_error,
                    })
                    .await
                    .ok();
                results.push(ToolResult {
                    call_id: call.id.clone(),
                    content: output.content,
                    is_error: output.is_error,
                });
            }
            history.push(TurnMessage::tool(results));
            steps += 1;
            if final_stop == Stop::Length {
                break 'outer;
            }
        }

        // True-up + cost
        if usage_total.input == 0 {
            usage_total.input = est_in;
        }
        if usage_total.output == 0 {
            usage_total.output = (assistant_text.len() as f64 / ratio as f64).ceil() as u64;
        }
        usage_total.cost = cost_of(&usage_total, price);

        if final_stop != Stop::Aborted {
            history.push(TurnMessage::assistant(if assistant_text.is_empty() {
                "(no text)".to_string()
            } else {
                assistant_text.clone()
            }));
        }
        events
            .send(Event::TurnFinished {
                stop: final_stop,
                usage: usage_total,
            })
            .await
            .ok();
    }

    /// Clearance gate, then execution. Ask-and-wait for anything not
    /// auto-allowed; surface answers arrive as `Command::Answer`.
    async fn gate_and_execute(
        &mut self,
        call: &ToolCall,
        commands: &mut mpsc::Receiver<Command>,
        events: &mpsc::Sender<Event>,
    ) -> ToolOutput {
        let Some(hand) = self.hands.iter().find(|h| h.def().name == call.tool) else {
            return ToolOutput::err(format!("unknown tool {}", call.tool));
        };
        let def = hand.def();
        let verdict = self.gate(def.clearance, call);
        match verdict {
            Gate::Allow => {}
            Gate::Ask { question } => {
                self.state.ask_counter += 1;
                let ask_id = AskId(format!("ask-{}", self.state.ask_counter));
                let options = vec![
                    "allow".to_string(),
                    "always".to_string(),
                    "deny".to_string(),
                ];
                let ask = Event::Ask {
                    id: ask_id.clone(),
                    questions: vec![AskQuestion {
                        text: question,
                        options,
                    }],
                };
                if events.send(ask).await.is_err() {
                    return ToolOutput::err("permission ask failed: surface closed");
                }
                // wait for the answer (or abort)
                loop {
                    tokio::select! {
                        maybe = commands.recv() => {
                            match maybe {
                                Some(Command::Answer { question: q, choice }) if q == ask_id => {
                                    match choice {
                                        1 => {
                                            self.state.rules.insert(format!("tool:{}", call.tool));
                                            break;
                                        }
                                        2 => {
                                            return ToolOutput::err(format!(
                                                "permission denied by user for {}",
                                                call.tool
                                            ));
                                        }
                                        _ => break,
                                    }
                                }
                                Some(Command::AlwaysAllow { rule }) => {
                                    self.state.rules.insert(rule);
                                }
                                Some(Command::Abort) => {
                                    return ToolOutput::err("aborted");
                                }
                                Some(_) => {}
                                None => return ToolOutput::err("surface closed during ask"),
                            }
                        }
                    }
                }
            }
        }
        hand.execute(&call.arguments, &self.hand_ctx).await
    }

    fn gate(&self, clearance: Clearance, call: &ToolCall) -> Gate {
        if self.state.rules.contains(&format!("tool:{}", call.tool)) {
            return Gate::Allow;
        }
        match clearance {
            Clearance::Read => Gate::Allow,
            Clearance::Write => match self.mode {
                ka_protocol::Mode::Free => Gate::Allow,
                ka_protocol::Mode::Guarded => Gate::Ask {
                    question: format!("allow {} to modify files?", call.tool),
                },
            },
            Clearance::Exec => {
                let command = call
                    .arguments
                    .get("command")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                let analysis = analyze(command);
                if let Some(stop) = hardstop(command, &analysis) {
                    return Gate::Ask {
                        question: format!(
                            "HARDSTOP — {}: `{}`. Proceed anyway?",
                            stop.reason, command
                        ),
                    };
                }
                if self.state.rules.contains(&format!(
                    "bash:{}",
                    analysis
                        .segments
                        .first()
                        .and_then(|s| s.first())
                        .cloned()
                        .unwrap_or_default()
                )) {
                    return Gate::Allow;
                }
                if all_readonly(&analysis) {
                    return Gate::Allow;
                }
                match self.mode {
                    ka_protocol::Mode::Free => Gate::Allow,
                    ka_protocol::Mode::Guarded => Gate::Ask {
                        question: format!("run `{command}`?"),
                    },
                }
            }
        }
    }
}

enum Gate {
    Allow,
    Ask { question: String },
}

fn truncate_excerpt(text: &str) -> String {
    let capped: String = text.chars().take(2_000).collect();
    if capped.len() < text.len() {
        format!("{capped}…")
    } else {
        capped
    }
}

async fn finish_after_error(events: &mpsc::Sender<Event>, class: ErrorClass, message: &str) {
    events
        .send(Event::Error {
            class,
            retryable: false,
            message: message.to_string(),
        })
        .await
        .ok();
    events
        .send(Event::TurnFinished {
            stop: Stop::Error,
            usage: Usage::default(),
        })
        .await
        .ok();
}

/// USD cost from usage and per-mtok prices (cache reads billed at input
/// rate — a conservative overestimate until per-tier pricing lands).
fn cost_of(usage: &Usage, price: ka_dialect::dialects::Price) -> f64 {
    let in_tokens = usage.input + usage.cache_read + usage.cache_write;
    (in_tokens as f64 / 1_000_000.0) * price.input_per_mtok
        + (usage.output as f64 / 1_000_000.0) * price.output_per_mtok
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use ka_dialect::dialects::{Catalog, Wire};
    use ka_dialect::speaker::{
        SpeakFuture, SpeakRequest, Speaker, StreamEvent, ToolCall, TurnMessage, TurnRole,
    };
    use ka_protocol::Usage;

    use super::{Voice, cost_of};

    #[test]
    fn cost_of_computes_from_price() {
        let usage = Usage {
            input: 1_000_000,
            output: 1_000_000,
            ..Default::default()
        };
        let price = ka_dialect::dialects::Price {
            input_per_mtok: 3.0,
            output_per_mtok: 15.0,
        };
        assert!((cost_of(&usage, price) - 18.0).abs() < 1e-9);
    }

    /// Fake speaker: first request → one `read` tool call; once a tool
    /// result is present → final text. Records every request it saw.
    struct FakeSpeaker {
        seen: std::sync::Arc<parking_lot::Mutex<Vec<Vec<TurnMessage>>>>,
    }

    impl Speaker for FakeSpeaker {
        fn speak<'a>(
            &'a self,
            req: SpeakRequest,
            out: tokio::sync::mpsc::Sender<StreamEvent>,
        ) -> SpeakFuture<'a> {
            let seen = self.seen.clone();
            Box::pin(async move {
                seen.lock().push(req.messages.clone());
                let has_result = req
                    .messages
                    .iter()
                    .any(|m| m.role == TurnRole::Tool && !m.results.is_empty());
                if has_result {
                    out.send(StreamEvent::Text("all done".into())).await.ok();
                    out.send(StreamEvent::Finished {
                        stop: ka_protocol::Stop::Done,
                        usage: ka_protocol::Usage {
                            input: 10,
                            output: 5,
                            ..Default::default()
                        },
                    })
                    .await
                    .ok();
                } else {
                    out.send(StreamEvent::Call(ToolCall {
                        id: "c1".into(),
                        tool: "read".into(),
                        arguments: serde_json::json!({"path": "roundtrip.txt"}),
                    }))
                    .await
                    .ok();
                    out.send(StreamEvent::Finished {
                        stop: ka_protocol::Stop::Done,
                        usage: ka_protocol::Usage {
                            input: 10,
                            output: 5,
                            ..Default::default()
                        },
                    })
                    .await
                    .ok();
                }
            })
        }
    }

    #[tokio::test]
    async fn tool_roundtrip_executes_and_feeds_results_back() {
        use ka_protocol::Event;
        use tokio::sync::mpsc;

        let dir = std::env::temp_dir().join(format!("ka-voice-rt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("roundtrip.txt"), "ROUNDTRIP-CONTENT\n").unwrap();

        let catalog = Catalog::parse(
            "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000\n",
        )
        .unwrap();
        let seen = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let mut voice = Voice::new(catalog, dir.clone(), ka_protocol::Mode::Guarded, 10)
            .with_speaker(
                Wire::OpenaiChat,
                std::sync::Arc::new(FakeSpeaker { seen: seen.clone() }),
            );

        let (cmd_tx, mut cmd_rx) = mpsc::channel(16);
        let (evt_tx, mut evt_rx) = mpsc::channel(256);
        let mut history = Vec::new();
        let mut interjections = Vec::new();
        let mut deferrals = std::collections::VecDeque::new();

        let handle = tokio::spawn(async move {
            voice
                .turn(
                    "test/m",
                    &mut history,
                    "read the file".into(),
                    &mut cmd_rx,
                    &evt_tx,
                    &mut interjections,
                    &mut deferrals,
                )
                .await;
        });

        let mut saw_output = false;
        let mut saw_done = false;
        while let Some(evt) = evt_rx.recv().await {
            match evt {
                Event::CallOutput { excerpt, .. } => {
                    assert!(excerpt.contains("ROUNDTRIP-CONTENT"), "{excerpt}");
                    saw_output = true;
                }
                Event::TurnFinished {
                    stop: ka_protocol::Stop::Done,
                    usage,
                } => {
                    assert_eq!(usage.input, 20); // two steps × 10
                    saw_done = true;
                    break;
                }
                Event::TurnFinished { .. } => break,
                _ => {}
            }
        }
        drop(cmd_tx);
        handle.await.unwrap();
        assert!(saw_output, "tool output must reach the surface");
        assert!(saw_done, "turn must finish done");

        let seen = seen.lock();
        assert_eq!(seen.len(), 2, "exactly two speaks expected");
        let second = &seen[1];
        let result = second
            .iter()
            .flat_map(|m| &m.results)
            .next()
            .expect("second speak must carry the tool result");
        assert!(result.content.contains("ROUNDTRIP-CONTENT"));
        assert!(!result.is_error);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn catalog_lookup_contract() {
        let catalog = Catalog::embedded();
        assert!(catalog.get("nope/missing").is_none());
        assert!(catalog.get("openai/gpt-5.1").is_some());
        let _ = Wire::OpenaiChat;
    }
}
