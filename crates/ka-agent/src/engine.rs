//! The engine: a turn machine over two queues. Surfaces send [`Command`]s
//! and consume [`Event`]s; the engine owns all sequencing.
//!
//! Two turn paths: the **canned** speaker (no model configured — keeps
//! `ka run` working keyless) and the **live voice** (real wires via
//! ka-dialect). Both honor interjections, deferrals, and aborts through the
//! same `select!` seam.

use std::collections::VecDeque;
use std::time::Duration;

use ka_protocol::{Command, ContextMeter, DeltaKind, ErrorClass, Event, Mode, Stop, Usage};
use tokio::sync::mpsc;

use crate::canned;
use crate::config::Config;
use crate::voice::Voice;

/// Handle returned by [`spawn`]: the surface's two queue ends.
pub struct EngineHandle {
    /// Send commands to the engine.
    pub commands: mpsc::Sender<Command>,
    /// Consume events from the engine.
    pub events: mpsc::Receiver<Event>,
}

/// Spawn the engine with the embedded catalog. Must be called inside a
/// tokio runtime (the CLI provides one).
pub fn spawn(config: Config) -> EngineHandle {
    spawn_with(config, ka_dialect::Catalog::embedded())
}

/// Spawn the engine over an explicit catalog (embedded + overlays +
/// discovery, assembled by the caller).
pub fn spawn_with(config: Config, catalog: ka_dialect::Catalog) -> EngineHandle {
    let (cmd_tx, cmd_rx) = mpsc::channel(64);
    let (evt_tx, evt_rx) = mpsc::channel(256);
    tokio::spawn(async move {
        if let Err(e) = run(cmd_rx, evt_tx, config, catalog).await {
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
    history: Vec<ka_dialect::speaker::TurnMessage>,
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
            history: Vec::new(),
        }
    }
}

type DynError = Box<dyn std::error::Error + Send + Sync>;

async fn run(
    mut commands: mpsc::Receiver<Command>,
    events: mpsc::Sender<Event>,
    config: Config,
    catalog: ka_dialect::Catalog,
) -> Result<(), DynError> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let mode = config.effective_mode();
    let max_steps = config.effective_max_steps();
    let mut state = EngineState::from(config);
    let mut voice = Voice::new(catalog, cwd, mode, max_steps);
    while let Some(cmd) = commands.recv().await {
        match cmd {
            Command::Prompt { text, .. } => {
                dispatch_turn(&mut commands, &events, &mut state, &mut voice, text).await;
                // Settling: drain deferrals as follow-on turns.
                while let Some(deferred) = state.deferrals.pop_front() {
                    dispatch_turn(&mut commands, &events, &mut state, &mut voice, deferred).await;
                }
            }
            Command::SetModel { selector } => {
                state.model = Some(selector.clone());
                events.send(Event::ModelChanged { selector }).await?;
            }
            Command::SetMode { mode } => {
                state.mode = mode;
                voice.set_mode(mode);
                events.send(Event::ModeChanged { mode }).await?;
            }
            other => side_command(&events, &mut state, other).await?,
        }
    }
    Ok(())
}

/// Route one prompt through the live voice when a model is configured,
/// else the canned speaker.
async fn dispatch_turn(
    commands: &mut mpsc::Receiver<Command>,
    events: &mpsc::Sender<Event>,
    state: &mut EngineState,
    voice: &mut Voice,
    text: String,
) {
    if let Some(model) = state.model.clone() {
        voice
            .turn(
                &model,
                &mut state.history,
                text,
                commands,
                events,
                &mut state.interjections,
                &mut state.deferrals,
            )
            .await;
    } else {
        turn_canned(commands, events, state, text).await;
    }
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
        Command::SetEffort { level } => state.effort = Some(level),
        Command::SetModel { .. } | Command::SetMode { .. } => {
            unreachable!("handled by caller with voice access")
        }
        Command::AlwaysAllow { .. }
        | Command::Answer { .. }
        | Command::Resume { .. }
        | Command::Compact { .. } => {
            events
                .send(Event::Error {
                    class: ErrorClass::Unsupported,
                    retryable: false,
                    message: "command not wired in phase 1".to_string(),
                })
                .await?;
        }
        Command::Prompt { .. } => unreachable!("prompt handled by caller"),
    }
    Ok(())
}

/// One canned turn: stream paced chunks, honoring aborts that arrive
/// mid-stream. Used when no model is configured.
async fn turn_canned(
    commands: &mut mpsc::Receiver<Command>,
    events: &mpsc::Sender<Event>,
    state: &mut EngineState,
    text: String,
) {
    let est_in = (text.len() as u64).div_ceil(4);
    events
        .send(Event::TurnStarted {
            context: ContextMeter {
                used: est_in,
                window: 0,
            },
        })
        .await
        .ok();

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
                        return;
                    }
                    Some(Command::Abort) => {
                        aborted = true;
                        break;
                    }
                    Some(other) => {
                        if let Err(e) = side_command(events, state, other).await {
                            eprintln!("ka engine: {e}");
                        }
                    }
                }
            }
            () = tokio::time::sleep(Duration::from_millis(20)) => {
                events
                    .send(Event::Delta { kind: DeltaKind::Text(chunks[idx].clone()) })
                    .await
                    .ok();
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
            .await
            .ok();
        return;
    }

    // Settling: unhandled interjections become deferrals so they are not lost.
    for interjection in state.interjections.drain(..) {
        state.deferrals.push_back(interjection);
    }
    let est_out: u64 = chunks
        .iter()
        .map(|c: &String| c.len() as u64)
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
        .await
        .ok();
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use ka_protocol::{Command, ErrorClass, Event, Stop};
    use std::time::Duration;
    use tokio::sync::mpsc;

    use crate::config::Config;
    use crate::engine::spawn;

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
