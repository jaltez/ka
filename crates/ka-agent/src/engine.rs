//! The engine: a turn machine over two queues. Surfaces send [`Command`]s
//! and consume [`Event`]s; the engine owns all sequencing.
//!
//! Turn shape (Phase 0, canned speaker): `Receiving → Speaking → Settling`.
//! Speaking streams paced chunks while listening for interjections and
//! aborts via `select!` — this is the same seam Phase 1's real Speaker
//! streams and Phase 2's tool execution plug into.

use std::collections::VecDeque;
use std::time::Duration;

use ka_protocol::{Command, ContextMeter, DeltaKind, ErrorClass, Event, Mode, Stop, Usage};
use tokio::sync::mpsc;

use crate::canned;
use crate::config::Config;

/// Handle returned by [`spawn`]: the surface's two queue ends.
pub struct EngineHandle {
    /// Send commands to the engine.
    pub commands: mpsc::Sender<Command>,
    /// Consume events from the engine.
    pub events: mpsc::Receiver<Event>,
}

/// Spawn the engine on the current tokio runtime. Must be called inside a
/// runtime (the CLI provides one).
pub fn spawn(config: Config) -> EngineHandle {
    let (cmd_tx, cmd_rx) = mpsc::channel(64);
    let (evt_tx, evt_rx) = mpsc::channel(256);
    tokio::spawn(async move {
        if let Err(e) = run(cmd_rx, evt_tx, config).await {
            // The events channel is gone or the engine hit an unrecoverable
            // state; surface-level diagnostics only.
            eprintln!("ka engine ended: {e}");
        }
    });
    EngineHandle {
        commands: cmd_tx,
        events: evt_rx,
    }
}

/// Live engine settings, derived from the initial config and mutated by
/// commands. Phase 3 persists these as strand `Change` records.
struct EngineState {
    model: Option<String>,
    effort: Option<ka_protocol::Effort>,
    mode: Mode,
    deferrals: VecDeque<String>,
    interjections: Vec<String>,
}

impl From<Config> for EngineState {
    fn from(c: Config) -> Self {
        let mode = c.effective_mode();
        Self {
            model: c.model,
            effort: c.effort,
            mode,
            deferrals: VecDeque::new(),
            interjections: Vec::new(),
        }
    }
}

type DynError = Box<dyn std::error::Error + Send + Sync>;

async fn run(
    mut commands: mpsc::Receiver<Command>,
    events: mpsc::Sender<Event>,
    config: Config,
) -> Result<(), DynError> {
    let mut state = EngineState::from(config);
    while let Some(cmd) = commands.recv().await {
        match cmd {
            Command::Prompt { text, .. } => {
                turn(&mut commands, &events, &mut state, text).await?;
                // Settling: drain deferrals as follow-on turns.
                while let Some(deferred) = state.deferrals.pop_front() {
                    turn(&mut commands, &events, &mut state, deferred).await?;
                }
            }
            other => side_command(&events, &mut state, other).await?,
        }
    }
    Ok(())
}

/// Handle a command that arrives outside a turn.
async fn side_command(
    events: &mpsc::Sender<Event>,
    state: &mut EngineState,
    cmd: Command,
) -> Result<(), DynError> {
    match cmd {
        Command::Interject { text } => state.interjections.push(text),
        Command::Defer { text } => state.deferrals.push_back(text),
        Command::Abort => {}
        Command::SetModel { selector } => {
            state.model = Some(selector.clone());
            events.send(Event::ModelChanged { selector }).await?;
        }
        Command::SetEffort { level } => state.effort = Some(level),
        Command::SetMode { mode } => {
            state.mode = mode;
            events.send(Event::ModeChanged { mode }).await?;
        }
        Command::AlwaysAllow { .. }
        | Command::Answer { .. }
        | Command::Resume { .. }
        | Command::Compact { .. } => {
            events
                .send(Event::Error {
                    class: ErrorClass::Unsupported,
                    retryable: false,
                    message: "command not wired in phase 0".to_string(),
                })
                .await?;
        }
        Command::Prompt { .. } => unreachable!("prompt handled by caller"),
    }
    Ok(())
}

/// One turn: stream the canned reply, honoring interjections and aborts that
/// arrive mid-stream.
async fn turn(
    commands: &mut mpsc::Receiver<Command>,
    events: &mpsc::Sender<Event>,
    state: &mut EngineState,
    text: String,
) -> Result<(), DynError> {
    let est_in = (text.len() as u64).div_ceil(4);
    events
        .send(Event::TurnStarted {
            context: ContextMeter {
                used: est_in,
                window: 0,
            },
        })
        .await?;

    let chunks = canned::reply(&text);
    let mut idx = 0;
    let mut aborted = false;
    while idx < chunks.len() {
        tokio::select! {
            biased;
            maybe = commands.recv() => {
                match maybe {
                    None => {
                        // Surface went away; finish quietly.
                        return Ok(());
                    }
                    Some(Command::Abort) => {
                        aborted = true;
                        break;
                    }
                    Some(other) => side_command(events, state, other).await?,
                }
            }
            () = tokio::time::sleep(Duration::from_millis(20)) => {
                events
                    .send(Event::Delta { kind: DeltaKind::Text(chunks[idx].clone()) })
                    .await?;
                idx += 1;
            }
        }
    }

    if aborted {
        events
            .send(Event::TurnFinished {
                stop: Stop::Aborted,
                usage: Usage::default(),
            })
            .await?;
        return Ok(());
    }

    // Settling: unhandled interjections become deferrals so they are not lost.
    for interjection in state.interjections.drain(..) {
        state.deferrals.push_back(interjection);
    }
    let est_out: u64 = chunks
        .iter()
        .map(|c| c.len() as u64)
        .sum::<u64>()
        .div_ceil(4);
    events
        .send(Event::TurnFinished {
            stop: Stop::Done,
            usage: Usage {
                input: est_in,
                output: est_out,
                ..Usage::default()
            },
        })
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use ka_protocol::{Command, ErrorClass, Event, Stop};
    use std::time::Duration;
    use tokio::sync::mpsc;

    use super::spawn;
    use crate::config::Config;

    async fn drain_until_finished(events: &mut mpsc::Receiver<Event>) -> Vec<Event> {
        let mut seen = Vec::new();
        while let Some(evt) = events.recv().await {
            let is_finished = matches!(evt, Event::TurnFinished { .. });
            seen.push(evt);
            if is_finished {
                break;
            }
        }
        seen
    }

    #[tokio::test]
    async fn prompt_streams_deltas_then_done() {
        let mut handle = spawn(Config::default());
        handle
            .commands
            .send(Command::Prompt {
                text: "hi".into(),
                attachments: vec![],
            })
            .await
            .unwrap();
        let seen = drain_until_finished(&mut handle.events).await;
        let deltas = seen
            .iter()
            .filter(|e| matches!(e, Event::Delta { .. }))
            .count();
        assert_eq!(deltas, 3);
        match seen.last().unwrap() {
            Event::TurnFinished {
                stop: Stop::Done,
                usage,
            } => {
                assert_eq!(usage.input, 1); // "hi" → ceil(2/4)
            }
            other => panic!("expected TurnFinished, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn abort_mid_turn_finishes_aborted() {
        let mut handle = spawn(Config::default());
        handle
            .commands
            .send(Command::Prompt {
                text: "slow".into(),
                attachments: vec![],
            })
            .await
            .unwrap();
        handle.commands.send(Command::Abort).await.unwrap();
        let seen = drain_until_finished(&mut handle.events).await;
        match seen.last().unwrap() {
            Event::TurnFinished {
                stop: Stop::Aborted,
                ..
            } => {}
            other => panic!("expected aborted finish, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deferrals_trigger_follow_on_turns() {
        let mut handle = spawn(Config::default());
        handle
            .commands
            .send(Command::Prompt {
                text: "one".into(),
                attachments: vec![],
            })
            .await
            .unwrap();
        handle
            .commands
            .send(Command::Defer { text: "two".into() })
            .await
            .unwrap();
        let first = drain_until_finished(&mut handle.events).await;
        assert!(matches!(
            first.last().unwrap(),
            Event::TurnFinished {
                stop: Stop::Done,
                ..
            }
        ));
        let second = drain_until_finished(&mut handle.events).await;
        assert!(matches!(second.first().unwrap(), Event::TurnStarted { .. }));
        assert!(matches!(
            second.last().unwrap(),
            Event::TurnFinished {
                stop: Stop::Done,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn unsupported_commands_report_errors() {
        let mut handle = spawn(Config::default());
        handle
            .commands
            .send(Command::Compact { focus: None })
            .await
            .unwrap();
        let evt = tokio::time::timeout(Duration::from_millis(500), handle.events.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            evt,
            Event::Error {
                class: ErrorClass::Unsupported,
                ..
            }
        ));
    }
}
