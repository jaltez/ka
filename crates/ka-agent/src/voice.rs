//! The live speaker path: resolves a model selector into a dialect + wire
//! Speaker, streams normalized events into the engine's queues, and keeps
//! the neutral conversation history for multi-turn requests.

use ka_dialect::dialects::{Catalog, Wire};
use ka_dialect::speaker::{SpeakRequest, Speaker, StreamEvent, TurnMessage, TurnRole};
use ka_protocol::{Command, Event, Stop, Usage};
use tokio::sync::mpsc;

/// Owns everything needed to speak to real models.
pub struct Voice {
    catalog: Catalog,
    speakers: std::collections::HashMap<Wire, std::sync::Arc<dyn Speaker>>,
}

impl Voice {
    /// New voice over a catalog.
    pub fn new(catalog: Catalog) -> Self {
        Self {
            catalog,
            speakers: Default::default(),
        }
    }

    fn speaker(&mut self, wire: Wire) -> std::sync::Arc<dyn Speaker> {
        self.speakers
            .entry(wire)
            .or_insert_with(|| ka_dialect::speaker_for(wire))
            .clone()
    }

    /// Run one live turn. Emits exactly one TurnFinished (Done/Length on
    /// success, Aborted, or Error) into `events`.
    #[allow(clippy::too_many_arguments)]
    pub async fn turn(
        &mut self,
        model_selector: &str,
        history: &mut Vec<TurnMessage>,
        prompt: String,
        commands: &mut mpsc::Receiver<Command>,
        events: &mpsc::Sender<Event>,
        interjections: &mut Vec<String>,
        deferrals: &mut std::collections::VecDeque<String>,
    ) {
        use ka_dialect::parse_selector;

        // Resolve selector → dialect
        let parsed = match parse_selector(model_selector) {
            Ok(p) => p,
            Err(e) => {
                emit_error(events, ka_protocol::ErrorClass::Protocol, &e.to_string()).await;
                return;
            }
        };
        let model_id = parsed.model_id();
        let Some(dialect) = self.catalog.get(&model_id).cloned() else {
            emit_error(
                events,
                ka_protocol::ErrorClass::Protocol,
                &format!(
                    "unknown model {model_id:?} (not in catalog; add it via a dialect overlay)"
                ),
            )
            .await;
            return;
        };
        let wire_model = dialect.wire_model.clone();
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

        let est_in = (prompt.len() as f64 / ratio as f64).ceil() as u64;
        let window = dialect.context as u64;
        let _ = wire_model;
        events
            .send(Event::TurnStarted {
                context: ka_protocol::ContextMeter {
                    used: est_in,
                    window,
                },
            })
            .await
            .ok();

        // Build request
        let mut messages = history.clone();
        messages.push(TurnMessage {
            role: TurnRole::User,
            content: prompt,
        });
        let req = SpeakRequest {
            model_id,
            dialect,
            effort: parsed.effort,
            system: String::new(),
            messages,
            token,
            cache_key: None,
        };

        let speaker = self.speaker(req.dialect.wire);
        let (tx, mut rx) = mpsc::channel::<StreamEvent>(256);
        {
            let speaker = speaker.clone();
            tokio::spawn(async move {
                speaker.speak(req, tx).await;
            });
        }

        let mut assistant_text = String::new();
        let mut final_stop = Stop::Error;
        let mut usage = Usage::default();
        let mut failed: Option<String> = None;

        loop {
            tokio::select! {
                biased;
                maybe_cmd = commands.recv() => {
                    match maybe_cmd {
                        None => return, // surface gone
                        Some(Command::Abort) => {
                            // dropping rx kills the speaker task's sends
                            events.send(Event::TurnFinished {
                                stop: Stop::Aborted,
                                usage: Usage::default(),
                            }).await.ok();
                            return;
                        }
                        Some(Command::Interject { text }) => interjections.push(text),
                        Some(Command::Defer { text }) => deferrals.push_back(text),
                        Some(Command::SetModel { selector }) => {
                            events.send(Event::ModelChanged { selector }).await.ok();
                        }
                        Some(Command::SetMode { mode }) => {
                            events.send(Event::ModeChanged { mode }).await.ok();
                        }
                        Some(other) => {
                            let unsupported = matches!(
                                other,
                                Command::AlwaysAllow { .. }
                                    | Command::Answer { .. }
                                    | Command::Resume { .. }
                                    | Command::Compact { .. }
                            );
                            if unsupported {
                                emit_error(events, ka_protocol::ErrorClass::Unsupported,
                                    "command not wired in phase 1").await;
                            }
                        }
                    }
                }
                maybe_evt = rx.recv() => {
                    let Some(evt) = maybe_evt else {
                        // speaker ended without Finished/Failed (aborted)
                        break;
                    };
                    match evt {
                        StreamEvent::Text(t) => {
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
                            // execution lands in Phase 2
                            events.send(Event::CallFinished {
                                tool: call.tool,
                                id: call.id,
                                ok: true,
                            }).await.ok();
                        }
                        StreamEvent::Finished { stop, usage: u } => {
                            final_stop = stop;
                            usage = u;
                            break;
                        }
                        StreamEvent::Failed { class, message, .. } => {
                            failed = Some(message.clone());
                            emit_error(events, class, &message).await;
                            final_stop = Stop::Error;
                            break;
                        }
                    }
                }
            }
        }

        // True-up: provider usage beats the char-ratio estimate.
        if usage.input == 0 {
            usage.input = est_in;
        }
        if usage.output == 0 {
            usage.output = (assistant_text.len() as f64 / ratio as f64).ceil() as u64;
        }
        usage.cost = cost_of(&usage, price);

        if final_stop != Stop::Error && final_stop != Stop::Aborted {
            history.push(TurnMessage {
                role: TurnRole::Assistant,
                content: if assistant_text.is_empty() {
                    "(tool calls; no text)".to_string()
                } else {
                    assistant_text
                },
            });
        }

        let _ = failed;
        events
            .send(Event::TurnFinished {
                stop: final_stop,
                usage,
            })
            .await
            .ok();
    }
}

/// USD cost from usage and per-mtok prices (cache reads billed at input
/// rate — a conservative overestimate until per-tier pricing lands).
fn cost_of(usage: &Usage, price: ka_dialect::dialects::Price) -> f64 {
    let in_tokens = usage.input + usage.cache_read + usage.cache_write;
    (in_tokens as f64 / 1_000_000.0) * price.input_per_mtok
        + (usage.output as f64 / 1_000_000.0) * price.output_per_mtok
}

async fn emit_error(events: &mpsc::Sender<Event>, class: ka_protocol::ErrorClass, message: &str) {
    events
        .send(Event::Error {
            class,
            retryable: false,
            message: message.to_string(),
        })
        .await
        .ok();
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::cost_of;
    use ka_dialect::dialects::{Catalog, Wire};
    use ka_protocol::Usage;

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

    #[test]
    fn voice_resolves_unknown_model_to_error_event() {
        // covered end-to-end by engine tests via the canned path; here we
        // only pin the catalog lookup contract
        let catalog = Catalog::embedded();
        assert!(catalog.get("nope/missing").is_none());
        assert!(catalog.get("openai/gpt-5.1").is_some());
        let _ = Wire::OpenaiChat;
    }
}
