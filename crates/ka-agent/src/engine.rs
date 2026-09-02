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

/// Which strand the engine should attach to.
#[derive(Debug, Clone)]
pub enum StrandChoice {
    /// Create a fresh strand.
    New,
    /// Continue the newest strand for the cwd (create if none).
    Latest,
    /// Open a specific strand file.
    Path(std::path::PathBuf),
}

/// Spawn the engine with the embedded catalog. Must be called inside a
/// tokio runtime (the CLI provides one).
pub fn spawn(config: Config) -> EngineHandle {
    spawn_full(config, ka_dialect::Catalog::embedded(), StrandChoice::New)
}

/// Spawn the engine over an explicit catalog (embedded + overlays +
/// discovery, assembled by the caller), attaching to a fresh strand.
pub fn spawn_with(config: Config, catalog: ka_dialect::Catalog) -> EngineHandle {
    spawn_full(config, catalog, StrandChoice::New)
}

/// Spawn with full control: catalog + strand choice.
pub fn spawn_full(
    config: Config,
    catalog: ka_dialect::Catalog,
    strand: StrandChoice,
) -> EngineHandle {
    let (cmd_tx, cmd_rx) = mpsc::channel(64);
    let (evt_tx, evt_rx) = mpsc::channel(256);
    tokio::spawn(async move {
        if let Err(e) = run(cmd_rx, evt_tx, config, catalog, strand).await {
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

/// Resolve a strand choice into an open strand (creating when allowed).
fn resolve_strand(
    choice: &StrandChoice,
    cwd: &std::path::Path,
) -> std::io::Result<ka_strand::StrandFile> {
    match choice {
        StrandChoice::New => {
            let repo = repo_snapshot(cwd);
            ka_strand::StrandFile::create(cwd, repo)
        }
        StrandChoice::Latest => match ka_strand::latest(cwd)? {
            Some(summary) => ka_strand::StrandFile::open(&summary.path),
            None => {
                let repo = repo_snapshot(cwd);
                ka_strand::StrandFile::create(cwd, repo)
            }
        },
        StrandChoice::Path(path) => ka_strand::StrandFile::open(path),
    }
}

/// Capture the repo snapshot in ka-strand's shape.
fn repo_snapshot(cwd: &std::path::Path) -> Option<ka_strand::RepoSnapshot> {
    let snap = crate::hands::git::RepoSnapshot::capture(cwd);
    (!snap.branch.is_empty()).then_some(ka_strand::RepoSnapshot {
        branch: snap.branch,
        dirty: snap.dirty,
    })
}

/// Convert persisted records into conversation history + active digest.
/// Digest records reset the accumulated history (the summary carries it).
fn history_from_records(
    records: &[ka_strand::Record],
) -> (
    Vec<ka_dialect::speaker::TurnMessage>,
    Vec<ka_protocol::RecordId>,
    Option<String>,
) {
    use ka_dialect::speaker::{ToolCall, ToolResult, TurnMessage, TurnRole};
    let mut out: Vec<TurnMessage> = Vec::new();
    let mut ids: Vec<ka_protocol::RecordId> = Vec::new();
    let mut digest: Option<String> = None;
    for r in records {
        match r {
            ka_strand::Record::Message {
                role,
                content,
                calls,
                results,
                ..
            } => {
                let turn_role = match role {
                    ka_strand::Role::User => TurnRole::User,
                    ka_strand::Role::Assistant | ka_strand::Role::System => TurnRole::Assistant,
                    ka_strand::Role::Tool => TurnRole::Tool,
                };
                if let Some(id) = r.id() {
                    ids.push(id.clone());
                }
                out.push(TurnMessage {
                    role: turn_role,
                    content: content.clone(),
                    calls: calls
                        .iter()
                        .map(|c| ToolCall {
                            id: c.id.clone(),
                            tool: c.tool.clone(),
                            arguments: c.arguments.clone(),
                        })
                        .collect(),
                    results: results
                        .iter()
                        .map(|r| ToolResult {
                            call_id: r.call_id.clone(),
                            content: r.content.clone(),
                            is_error: r.is_error,
                        })
                        .collect(),
                });
            }
            ka_strand::Record::Digest { summary, .. } => {
                // everything before the digest is carried by its summary
                out.clear();
                ids.clear();
                digest = Some(summary.clone());
            }
            ka_strand::Record::Boundary { .. } => {
                out.clear();
                ids.clear();
                digest = None;
            }
            ka_strand::Record::Rewind { kept_from, .. } => {
                // drop messages after the kept point (the tail of the log
                // is the abandoned branch)
                if let Some(pos) = ids.iter().position(|i| i == kept_from) {
                    let keep_msgs = pos + 1;
                    out.truncate(keep_msgs);
                    ids.truncate(keep_msgs);
                }
            }
            _ => {}
        }
    }
    (out, ids, digest)
}

/// Convert one neutral history message into a persistable record.
fn record_from_message(msg: &ka_dialect::speaker::TurnMessage) -> ka_strand::Record {
    use ka_dialect::speaker::TurnRole;
    let role = match msg.role {
        TurnRole::User => ka_strand::Role::User,
        TurnRole::Assistant => ka_strand::Role::Assistant,
        TurnRole::Tool => ka_strand::Role::Tool,
    };
    ka_strand::Record::Message {
        id: ka_strand::new_record_id(),
        role,
        content: msg.content.clone(),
        calls: msg
            .calls
            .iter()
            .map(|c| ka_strand::StoredCall {
                id: c.id.clone(),
                tool: c.tool.clone(),
                arguments: c.arguments.clone(),
            })
            .collect(),
        results: msg
            .results
            .iter()
            .map(|r| ka_strand::StoredResult {
                call_id: r.call_id.clone(),
                content: r.content.clone(),
                is_error: r.is_error,
            })
            .collect(),
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
    /// Record ids of persisted history messages, aligned with
    /// voice.history[..record_ids.len()] (mod digest truncation).
    record_ids: Vec<ka_protocol::RecordId>,
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
            record_ids: Vec::new(),
        }
    }
}

type DynError = Box<dyn std::error::Error + Send + Sync>;

/// The engine's owned state bundle: one place, passed to every command
/// handler. Replaces the run()/side_command() split where arms were
/// partitioned by which pieces they happened to touch.
struct Ctx {
    cwd: std::path::PathBuf,
    events: mpsc::Sender<Event>,
    state: EngineState,
    voice: Voice,
    strand: ka_strand::StrandFile,
}

async fn run(
    mut commands: mpsc::Receiver<Command>,
    events: mpsc::Sender<Event>,
    config: Config,
    catalog: ka_dialect::Catalog,
    strand_choice: StrandChoice,
) -> Result<(), DynError> {
    let cwd = config
        .cwd
        .clone()
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let mcp_servers = config.mcp.clone();
    let mode = config.effective_mode();
    let max_steps = config.effective_max_steps();
    let rules = config.rules.clone();
    let hooks = config.hooks.clone();
    let pathfinder_catalog = catalog.clone();
    let mut voice = Voice::new(catalog, cwd.clone(), mode, max_steps);
    voice.set_rules(rules);
    voice.set_hooks(hooks);
    {
        let slot = voice.pathfinder_slot();
        slot.write().catalog = pathfinder_catalog;
    }
    let mut state = EngineState::from(config);
    let strand = attach_strand(&events, &mut state, &mut voice, &cwd, &strand_choice).await?;
    let mut ctx = Ctx {
        cwd,
        events,
        state,
        voice,
        strand,
    };
    // markdown agents: .ka/agents/*.md etc. become one `delegate` hand
    let agents = crate::agents::AgentDef::discover(&ctx.cwd);
    if !agents.is_empty() {
        let names: Vec<String> = agents.iter().map(|a| a.name.clone()).collect();
        let slot = ctx.voice.pathfinder_slot();
        ctx.voice
            .push_hand(Box::new(crate::hands::delegate::DelegateHand::new(
                agents, slot,
            )));
        ctx.events
            .send(Event::Note {
                message: format!("agents available: {}", names.join(", ")),
            })
            .await
            .ok();
    }

    // MCP servers: spawn, handshake, list; each tool becomes a hand at
    // exec-tier clearance. Failures are per-server notes, never fatal.
    for cfg in &mcp_servers {
        let connected = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            crate::mcp::McpClient::spawn_connect(cfg),
        )
        .await;
        match connected {
            Ok(Ok((client, tools))) if !tools.is_empty() => {
                let shared = std::sync::Arc::new(tokio::sync::Mutex::new(client));
                let names: Vec<String> = tools.iter().map(|t| t.name.clone()).collect();
                for tool in tools {
                    ctx.voice
                        .push_hand(Box::new(crate::mcp::McpHand::new(tool, shared.clone())));
                }
                ctx.events
                    .send(Event::Note {
                        message: format!(
                            "mcp {}: {} tool(s): {}",
                            cfg.name,
                            names.len(),
                            names.join(", ")
                        ),
                    })
                    .await
                    .ok();
            }
            Ok(Ok((_client, _empty))) => {
                ctx.events
                    .send(Event::Note {
                        message: format!("mcp {}: connected but advertises no tools", cfg.name),
                    })
                    .await
                    .ok();
            }
            Ok(Err(e)) => {
                ctx.events
                    .send(Event::Error {
                        class: ErrorClass::Protocol,
                        retryable: false,
                        message: format!("mcp {}: {e}", cfg.name),
                    })
                    .await
                    .ok();
            }
            Err(_) => {
                ctx.events
                    .send(Event::Error {
                        class: ErrorClass::Protocol,
                        retryable: false,
                        message: format!("mcp {}: connect timed out", cfg.name),
                    })
                    .await
                    .ok();
            }
        }
    }

    while let Some(cmd) = commands.recv().await {
        handle_command(cmd, &mut commands, &mut ctx).await?;
    }
    Ok(())
}

/// The single, exhaustive command dispatcher. Every `Command` variant is
/// handled (or explicitly rejected) here — no unreachable arms, no
/// routing split. Arms that need to poll the surface mid-turn (Prompt)
/// also receive the receiver.
async fn handle_command(
    cmd: Command,
    commands: &mut mpsc::Receiver<Command>,
    ctx: &mut Ctx,
) -> Result<(), DynError> {
    match cmd {
        Command::Prompt { text } => {
            dispatch_turn(
                commands,
                &ctx.events,
                &mut ctx.state,
                &mut ctx.voice,
                text,
                &mut ctx.strand,
            )
            .await;
            settle_context(&mut ctx.voice, &mut ctx.state, &mut ctx.strand, &ctx.events).await;
            // Settling: drain deferrals as follow-on turns.
            while let Some(deferred) = ctx.state.deferrals.pop_front() {
                dispatch_turn(
                    commands,
                    &ctx.events,
                    &mut ctx.state,
                    &mut ctx.voice,
                    deferred,
                    &mut ctx.strand,
                )
                .await;
                settle_context(&mut ctx.voice, &mut ctx.state, &mut ctx.strand, &ctx.events).await;
            }
            ctx.events.send(Event::Idle).await.ok();
        }
        Command::SetModel { selector } => {
            ctx.state.model = Some(selector.clone());
            ctx.voice.pathfinder_slot().write().model = Some(selector.clone());
            let _ = ctx.strand.append(ka_strand::Record::Change {
                id: ka_strand::new_record_id(),
                model: Some(selector.clone()),
                effort: None,
                mode: None,
            });
            ctx.events.send(Event::ModelChanged { selector }).await?;
        }
        Command::SetMode { mode } => {
            ctx.state.mode = mode;
            ctx.voice.set_mode(mode);
            let _ = ctx.strand.append(ka_strand::Record::Change {
                id: ka_strand::new_record_id(),
                model: None,
                effort: None,
                mode: Some(mode),
            });
            ctx.events.send(Event::ModeChanged { mode }).await?;
        }
        Command::SwitchStrand { id } => {
            let choice = if id.trim() == "new" {
                StrandChoice::New
            } else {
                match ka_strand::resolve_id(&ctx.cwd, &id) {
                    Ok(ka_strand::IdMatch::Unique(summary)) => StrandChoice::Path(summary.path),
                    Ok(ka_strand::IdMatch::Ambiguous(candidates)) => {
                        let ids: Vec<String> = candidates.iter().map(|c| c.id.clone()).collect();
                        ctx.events
                            .send(Event::Error {
                                class: ErrorClass::Unsupported,
                                retryable: false,
                                message: format!(
                                    "session id '{}' is ambiguous: {}",
                                    id,
                                    ids.join(", ")
                                ),
                            })
                            .await
                            .ok();
                        ctx.events.send(Event::Idle).await.ok();
                        return Ok(());
                    }
                    other => {
                        let message = match other {
                            Ok(ka_strand::IdMatch::None) => {
                                format!("no session matches '{id}'")
                            }
                            Err(e) => format!("session lookup failed: {e}"),
                            _ => unreachable!(),
                        };
                        ctx.events
                            .send(Event::Error {
                                class: ErrorClass::Unsupported,
                                retryable: false,
                                message,
                            })
                            .await
                            .ok();
                        ctx.events.send(Event::Idle).await.ok();
                        return Ok(());
                    }
                }
            };
            match attach_strand(
                &ctx.events,
                &mut ctx.state,
                &mut ctx.voice,
                &ctx.cwd,
                &choice,
            )
            .await
            {
                Ok(fresh) => {
                    ctx.strand = fresh;
                    ctx.events
                        .send(Event::Note {
                            message: format!("switched to session {}", strand_id(&ctx.strand)),
                        })
                        .await
                        .ok();
                }
                Err(e) => {
                    ctx.events
                        .send(Event::Error {
                            class: ErrorClass::Protocol,
                            retryable: false,
                            message: format!("session switch failed: {e}"),
                        })
                        .await
                        .ok();
                }
            }
            ctx.events.send(Event::Idle).await.ok();
        }
        Command::Interject { text } => ctx.state.interjections.push(text),
        Command::Defer { text } => ctx.state.deferrals.push_back(text),
        Command::Abort => {}
        Command::SetEffort { level } => ctx.state.effort = Some(level),
        Command::Compact { focus } => {
            run_digest(
                &mut ctx.voice,
                &mut ctx.state,
                &mut ctx.strand,
                &ctx.events,
                focus,
            )
            .await;
            ctx.events.send(Event::Idle).await.ok();
        }
        Command::Rewind { turns } => {
            match ctx.voice.rewind(turns) {
                Some(kept) => {
                    // kept = index of the dropped user message; keep
                    // everything strictly before it
                    let keep_idx = kept.saturating_sub(1);
                    let kept_from = ctx
                        .state
                        .record_ids
                        .get(keep_idx)
                        .cloned()
                        .unwrap_or_else(ka_strand::new_record_id);
                    let _ = ctx.strand.append(ka_strand::Record::Rewind {
                        id: ka_strand::new_record_id(),
                        kept_from,
                    });
                    ctx.state.record_ids.truncate(keep_idx + 1);
                    ctx.events
                        .send(Event::Note {
                            message: format!("rewound {turns} turn(s)"),
                        })
                        .await
                        .ok();
                }
                None => {
                    ctx.events
                        .send(Event::Error {
                            class: ErrorClass::Protocol,
                            retryable: false,
                            message: format!("cannot rewind {turns} turn(s): not enough history"),
                        })
                        .await
                        .ok();
                }
            }
            ctx.events.send(Event::Idle).await.ok();
        }
        Command::UndoFile => {
            let sink = ctx.voice.snapshot_sink();
            let outcome = sink.lock().undo();
            match outcome {
                Ok(Some(entry)) => {
                    let what = if entry.existed {
                        format!("restored {}", entry.path.display())
                    } else {
                        format!(
                            "removed {} (was created this session)",
                            entry.path.display()
                        )
                    };
                    ctx.events
                        .send(Event::Note {
                            message: format!("↩ {what}"),
                        })
                        .await
                        .ok();
                }
                Ok(None) => {
                    ctx.events
                        .send(Event::Note {
                            message: "↩ nothing to undo in this session".to_string(),
                        })
                        .await
                        .ok();
                }
                Err(e) => {
                    ctx.events
                        .send(Event::Error {
                            class: ErrorClass::Protocol,
                            retryable: false,
                            message: format!("undo failed: {e}"),
                        })
                        .await
                        .ok();
                }
            }
            ctx.events.send(Event::Idle).await.ok();
        }
        Command::SaveSettings {
            model,
            effort,
            mode,
        } => {
            match crate::config::save_user_settings(model.as_deref(), effort, mode) {
                Ok(path) => {
                    ctx.events
                        .send(Event::Note {
                            message: format!("saved settings to {}", path.display()),
                        })
                        .await
                        .ok();
                }
                Err(e) => {
                    ctx.events
                        .send(Event::Error {
                            class: ErrorClass::Protocol,
                            retryable: false,
                            message: format!("saving settings failed: {e}"),
                        })
                        .await
                        .ok();
                }
            }
            ctx.events.send(Event::Idle).await.ok();
        }
        Command::SaveApiKey { env_var, value } => {
            match crate::config::save_api_key(&env_var, &value) {
                Ok(path) => {
                    ctx.events
                        .send(Event::Note {
                            message: format!("{env_var} saved to {}", path.display()),
                        })
                        .await
                        .ok();
                }
                Err(e) => {
                    ctx.events
                        .send(Event::Error {
                            class: ErrorClass::Protocol,
                            retryable: false,
                            message: format!("saving {env_var} failed: {e}"),
                        })
                        .await
                        .ok();
                }
            }
            ctx.events.send(Event::Idle).await.ok();
        }
        Command::Answer { .. } => {
            // answers are consumed inside the pending ask; one arriving
            // here means the surface answered an ask that no longer exists
            ctx.events
                .send(Event::Error {
                    class: ErrorClass::Unsupported,
                    retryable: false,
                    message: "no pending question to answer".to_string(),
                })
                .await?;
        }
    }
    Ok(())
}

/// Attach to a strand (startup or mid-session switch): open/create it,
/// replay its settings over the live state, load history into the voice,
/// mark the waypoint, and announce the session to surfaces.
async fn attach_strand(
    events: &mpsc::Sender<Event>,
    state: &mut EngineState,
    voice: &mut Voice,
    cwd: &std::path::Path,
    choice: &StrandChoice,
) -> Result<ka_strand::StrandFile, DynError> {
    let mut strand =
        resolve_strand(choice, cwd).map_err(|e| -> DynError { format!("strand: {e}").into() })?;
    // interrupted-turn synthesis
    let _ = strand.synthesize_aborted();
    // session settings win over whatever the engine was running with
    let settings = strand.settings().clone();
    if settings.model.is_some() {
        state.model = settings.model.clone();
    }
    if settings.effort.is_some() {
        state.effort = settings.effort;
    }
    if settings.mode.is_some() {
        state.mode = settings.mode.unwrap_or_default();
        voice.set_mode(state.mode);
    }
    voice.pathfinder_slot().write().model = state.model.clone();
    let (history, ids, digest) = history_from_records(strand.records());
    voice.load_history(history, digest);
    state.record_ids = ids;
    if let Some(path) = strand.path() {
        write_waypoint(cwd, path);
    }
    let id = strand
        .records()
        .first()
        .and_then(|r| match r {
            ka_strand::Record::Header { id, .. } => Some(id.0.clone()),
            _ => None,
        })
        .unwrap_or_default();
    events
        .send(Event::SessionInfo { id: id.clone() })
        .await
        .ok();
    // the snapshot journal follows the active strand (undo is session-scoped)
    voice.snapshot_sink().lock().set_strand(id);
    if let Some(selector) = &state.model {
        events
            .send(Event::ModelChanged {
                selector: selector.clone(),
            })
            .await
            .ok();
    }
    if settings.mode.is_some() {
        events
            .send(Event::ModeChanged { mode: state.mode })
            .await
            .ok();
    }
    // replay resumed history so surfaces can rebuild the transcript
    if !voice.history.is_empty() {
        let messages = voice
            .history
            .iter()
            .filter(|m| !m.content.trim().is_empty())
            .map(|m| ka_protocol::ReplayedMessage {
                role: match m.role {
                    ka_dialect::speaker::TurnRole::User => "user".to_string(),
                    _ => "assistant".to_string(),
                },
                content: m.content.clone(),
            })
            .collect();
        events.send(Event::Replay { messages }).await.ok();
    }
    Ok(strand)
}

/// Post-turn context maintenance: prune old tool outputs, digest while
/// the window is under pressure.
async fn settle_context(
    voice: &mut Voice,
    state: &mut EngineState,
    strand: &mut ka_strand::StrandFile,
    events: &mpsc::Sender<Event>,
) {
    let window = voice.window_tokens();
    let ratio = voice_ratio(voice);
    let saved = voice.prune_tool_outputs(ratio);
    if std::env::var("KA_DEBUG_SETTLE").is_ok() {
        eprintln!(
            "[settle] window={window} ratio={ratio} last_context={} pressure={}",
            voice.debug_last_context(),
            voice.context_pressure(window)
        );
    }
    if saved > 0 {
        events
            .send(Event::Note {
                message: format!("pruned ~{saved} tokens of old tool output"),
            })
            .await
            .ok();
    }
    let mut digests = 0;
    while voice.context_pressure(window) && digests < 3 {
        digests += 1;
        match run_digest(voice, state, strand, events, None).await {
            DigestResult::Digested => continue,
            DigestResult::NoModel => break,
        }
    }
}

async fn run_digest(
    voice: &mut Voice,
    state: &mut EngineState,
    strand: &mut ka_strand::StrandFile,
    events: &mpsc::Sender<Event>,
    focus: Option<String>,
) -> DigestResult {
    let Some(model) = voice.model_selector_cloned() else {
        events
            .send(Event::Error {
                class: ErrorClass::Unsupported,
                retryable: false,
                message: "compact needs an active model".to_string(),
            })
            .await
            .ok();
        return DigestResult::NoModel;
    };
    let _ = model;
    events.send(Event::DigestStarted).await.ok();
    let ratio = voice_ratio(voice);
    match voice
        .summarize(focus.as_deref(), std::time::Duration::from_secs(120))
        .await
    {
        Some(summary) => {
            let kept = voice.apply_digest(summary, ratio);
            let _ = kept;
            persist_delta(voice, state, strand);
            events
                .send(Event::Note {
                    message: "context digested".to_string(),
                })
                .await
                .ok();
            DigestResult::Digested
        }
        None => {
            // Mechanical fallback: keep the recent tail with a truncation
            // note. Guarantees progress when the summarizer cannot fit.
            events
                .send(Event::Note {
                    message: "digest summarizer unavailable; truncating to recent tail".to_string(),
                })
                .await
                .ok();
            let summary = "Earlier conversation was truncated automatically (summarizer \
unavailable). The most recent exchange follows; re-read files you need."
                .to_string();
            voice.apply_digest(summary, ratio);
            persist_delta(voice, state, strand);
            DigestResult::Digested
        }
    }
}

enum DigestResult {
    Digested,
    NoModel,
}

fn voice_ratio(voice: &Voice) -> f64 {
    voice.model_ratio()
}

/// Route one prompt through the live voice when a model is configured,
/// else the canned speaker.
async fn dispatch_turn(
    commands: &mut mpsc::Receiver<Command>,
    events: &mpsc::Sender<Event>,
    state: &mut EngineState,
    voice: &mut Voice,
    text: String,
    strand: &mut ka_strand::StrandFile,
) {
    if let Some(model) = state.model.clone() {
        voice
            .turn(
                &model,
                text,
                commands,
                events,
                &mut state.interjections,
                &mut state.deferrals,
            )
            .await;
    } else {
        let mut history = std::mem::take(&mut voice.history);
        turn_canned(commands, events, state, &mut history, text).await;
        voice.history = history;
    }
    persist_delta(voice, state, strand);
}

/// Persist any pending digest (as a Digest record) and the history delta.
fn persist_delta(voice: &mut Voice, state: &mut EngineState, strand: &mut ka_strand::StrandFile) {
    if let Some((summary, kept, _rev)) = voice.take_pending_digest() {
        let kept_from = state
            .record_ids
            .get(kept)
            .cloned()
            .unwrap_or_else(ka_strand::new_record_id);
        let _ = strand.append(ka_strand::Record::Digest {
            id: ka_strand::new_record_id(),
            summary,
            kept_from,
        });
        state.record_ids = state.record_ids.get(kept..).unwrap_or(&[]).to_vec();
    }
    while state.record_ids.len() < voice.history.len() {
        let record = record_from_message(&voice.history[state.record_ids.len()]);
        let _ = strand.append(record.clone());
        if let Some(id) = record.id() {
            state.record_ids.push(id.clone());
        } else {
            break;
        }
    }
}

/// The strand's own id from its header record.
fn strand_id(strand: &ka_strand::StrandFile) -> String {
    strand
        .records()
        .first()
        .and_then(|r| match r {
            ka_strand::Record::Header { id, .. } => Some(id.0.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

/// Waypoint: tiny per-terminal pointer so `ka -c` continues the right
/// strand per pane. Best-effort.
fn write_waypoint(cwd: &std::path::Path, strand_path: &std::path::Path) {
    let Some(key) = tty_key() else { return };
    let dir = ka_strand::data_dir().join("waypoints");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let _ = std::fs::write(
        dir.join(key),
        format!("{}\n{}", cwd.display(), strand_path.display()),
    );
}

/// Read this terminal's waypoint (cwd + strand path), if any.
pub fn read_waypoint() -> Option<(std::path::PathBuf, std::path::PathBuf)> {
    let key = tty_key()?;
    let text = std::fs::read_to_string(ka_strand::data_dir().join("waypoints").join(key)).ok()?;
    let mut lines = text.lines();
    let cwd = lines.next()?.to_string();
    let path = lines.next()?.to_string();
    Some((
        std::path::PathBuf::from(cwd),
        std::path::PathBuf::from(path),
    ))
}

fn tty_key() -> Option<String> {
    if let Ok(explicit) = std::env::var("KA_TTY") {
        return Some(format!("{}-{}", explicit.len(), explicit.replace('/', "_")));
    }
    let link = std::fs::read_link("/proc/self/fd/0").ok()?;
    let name = link.to_string_lossy();
    if name.starts_with("/dev/") && name.contains("tty") {
        Some(format!("fd-{}", name.replace('/', "_")))
    } else {
        None
    }
}

/// One canned turn: stream paced chunks, honoring aborts that arrive
/// mid-stream. Used when no model is configured.
async fn turn_canned(
    commands: &mut mpsc::Receiver<Command>,
    events: &mpsc::Sender<Event>,
    state: &mut EngineState,
    history: &mut Vec<ka_dialect::speaker::TurnMessage>,
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

    history.push(ka_dialect::speaker::TurnMessage::user(text.clone()));
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
                        // canned path: only queue-side effects are safe here
                        match other {
                            Command::Interject { text } => state.interjections.push(text),
                            Command::Defer { text } => state.deferrals.push_back(text),
                            Command::Abort => {}
                            _ => {}
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
    history.push(ka_dialect::speaker::TurnMessage::assistant(chunks.concat()));
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
            .send(Command::Prompt { text: "hi".into() })
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
            .send(Command::Prompt { text: "one".into() })
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
    async fn strand_persists_and_resumes() {
        use crate::engine::{StrandChoice, spawn_full};
        use ka_protocol::Command;

        let data = std::env::temp_dir().join(format!("ka-eng-strand-{}", std::process::id()));
        let work = data.join("work");
        let _ = std::fs::remove_dir_all(&data);
        std::fs::create_dir_all(&work).unwrap();
        // thread-local data dir applies to engine tasks on this runtime
        ka_strand::set_data_dir_for_tests(data.clone());

        let cfg = Config {
            cwd: Some(work.display().to_string()),
            ..Default::default()
        };
        let mut h1 = spawn_full(
            cfg.clone(),
            ka_dialect::Catalog::embedded(),
            StrandChoice::New,
        );
        h1.commands
            .send(Command::Prompt {
                text: "hello there".into(),
            })
            .await
            .unwrap();
        while let Some(evt) = h1.events.recv().await {
            if matches!(evt, Event::TurnFinished { .. }) {
                break;
            }
        }
        drop(h1);
        tokio::time::sleep(Duration::from_millis(50)).await;

        // strand exists with the conversation
        let summaries = ka_strand::list(&work).unwrap();
        assert_eq!(summaries.len(), 1, "{summaries:?}");
        let records = ka_strand::read(&summaries[0].path).unwrap();
        let user = records.iter().find_map(|r| match r {
            ka_strand::Record::Message {
                role: ka_strand::Role::User,
                content,
                ..
            } => Some(content.clone()),
            _ => None,
        });
        assert_eq!(user.as_deref(), Some("hello there"));
        assert_eq!(
            summaries[0].title, "hello there",
            "title = first user message"
        );

        // resume: continue the same strand with a second prompt
        let mut h2 = spawn_full(cfg, ka_dialect::Catalog::embedded(), StrandChoice::Latest);
        h2.commands
            .send(Command::Prompt {
                text: "second question".into(),
            })
            .await
            .unwrap();
        while let Some(evt) = h2.events.recv().await {
            if matches!(evt, Event::TurnFinished { .. }) {
                break;
            }
        }
        drop(h2);
        tokio::time::sleep(Duration::from_millis(50)).await;
        let after = ka_strand::list(&work).unwrap();
        assert_eq!(after.len(), 1, "Latest must reuse the strand: {after:?}");
        assert_eq!(
            after[0].messages, 4,
            "two full exchanges expected: {after:?}"
        );
        let resumed = ka_strand::read(&after[0].path).unwrap();
        let users: Vec<&str> = resumed
            .iter()
            .filter_map(|r| match r {
                ka_strand::Record::Message {
                    role: ka_strand::Role::User,
                    content,
                    ..
                } => Some(content.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(users, vec!["hello there", "second question"]);
        let _ = std::fs::remove_dir_all(&data);
    }

    #[tokio::test]
    async fn rewind_persists_and_resume_truncates() {
        use crate::engine::{StrandChoice, spawn_full};
        use ka_protocol::Command;

        let data = std::env::temp_dir().join(format!("ka-eng-rewind-{}", std::process::id()));
        let work = data.join("work");
        let _ = std::fs::remove_dir_all(&data);
        std::fs::create_dir_all(&work).unwrap();
        ka_strand::set_data_dir_for_tests(data.clone());

        let cfg = Config {
            cwd: Some(work.display().to_string()),
            ..Default::default()
        };
        let mut h = spawn_full(
            cfg.clone(),
            ka_dialect::Catalog::embedded(),
            StrandChoice::New,
        );
        for prompt in ["first question", "second question"] {
            h.commands
                .send(Command::Prompt {
                    text: prompt.into(),
                })
                .await
                .unwrap();
            while let Some(evt) = h.events.recv().await {
                if matches!(evt, Event::Idle) {
                    break;
                }
            }
        }
        // rewind one turn
        h.commands.send(Command::Rewind { turns: 1 }).await.unwrap();
        while let Some(evt) = h.events.recv().await {
            if matches!(evt, Event::Idle) {
                break;
            }
        }
        drop(h);
        tokio::time::sleep(Duration::from_millis(50)).await;

        // resume: history must end after the FIRST exchange only
        let mut h2 = spawn_full(cfg, ka_dialect::Catalog::embedded(), StrandChoice::Latest);
        let mut replayed = Vec::new();
        while let Some(evt) = h2.events.recv().await {
            if let Event::Replay { messages } = evt {
                replayed = messages.into_iter().map(|m| m.content).collect();
                break;
            }
        }
        drop(h2);
        assert_eq!(
            replayed,
            vec!["first question".to_string(), "(ka, no model configured) heard: first question — set a model with --model or KA_MODEL to speak for real.".to_string()],
            "rewound session replays only the kept exchange: {replayed:?}"
        );
        let _ = std::fs::remove_dir_all(&data);
    }

    #[tokio::test]
    async fn switch_strand_replays_target_history() {
        let data = std::env::temp_dir().join(format!("ka-test-switch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        std::fs::create_dir_all(&data).unwrap();
        ka_strand::set_data_dir_for_tests(data.clone());
        let cwd = data.join("repo");
        std::fs::create_dir_all(&cwd).unwrap();

        let mut handle = spawn(Config {
            cwd: Some(cwd.clone().to_string_lossy().into()),
            ..Default::default()
        });
        handle
            .commands
            .send(Command::Prompt {
                text: "first session question".into(),
            })
            .await
            .unwrap();
        // drain to idle
        loop {
            let evt = tokio::time::timeout(Duration::from_millis(2_000), handle.events.recv())
                .await
                .unwrap()
                .unwrap();
            if matches!(evt, Event::Idle) {
                break;
            }
        }
        let sessions = ka_strand::list(&cwd).unwrap();
        let first_id = sessions[0].id.clone();

        // switch to a fresh session
        handle
            .commands
            .send(Command::SwitchStrand { id: "new".into() })
            .await
            .unwrap();
        let mut saw_new_session = false;
        let mut saw_replay_empty = true;
        loop {
            let evt = tokio::time::timeout(Duration::from_millis(2_000), handle.events.recv())
                .await
                .unwrap()
                .unwrap();
            match evt {
                Event::SessionInfo { id } if id != first_id => saw_new_session = true,
                Event::Replay { messages } => saw_replay_empty = messages.is_empty(),
                Event::Note { .. } => {}
                Event::Idle => break,
                _ => {}
            }
        }
        assert!(saw_new_session, "switch announced a new session id");
        assert!(saw_replay_empty, "fresh session replays no history");

        // switch back to the first session by id prefix
        let tail: String = first_id.split('-').nth(1).unwrap_or(&first_id).to_string();
        handle
            .commands
            .send(Command::SwitchStrand { id: tail })
            .await
            .unwrap();
        let mut back_id = String::new();
        let mut replayed_first = false;
        loop {
            let evt = tokio::time::timeout(Duration::from_millis(2_000), handle.events.recv())
                .await
                .unwrap()
                .unwrap();
            match evt {
                Event::SessionInfo { id } => back_id = id,
                Event::Replay { messages } => {
                    replayed_first = messages
                        .iter()
                        .any(|m| m.content.contains("first session question"));
                }
                Event::Note { .. } => {}
                Event::Idle => break,
                _ => {}
            }
        }
        assert_eq!(
            back_id, first_id,
            "returned to the first session by tail prefix"
        );
        assert!(replayed_first, "history of the target session was replayed");
        drop(handle);
        let _ = std::fs::remove_dir_all(&data);
    }

    #[tokio::test]
    async fn unsupported_commands_report_errors() {
        let mut handle = spawn(Config::default());
        handle
            .commands
            .send(Command::Compact { focus: None })
            .await
            .unwrap();
        let evt = loop {
            let evt = tokio::time::timeout(Duration::from_millis(500), handle.events.recv())
                .await
                .unwrap()
                .unwrap();
            match evt {
                Event::SessionInfo { .. }
                | Event::ModelChanged { .. }
                | Event::ModeChanged { .. }
                | Event::Replay { .. } => continue,
                other => break other,
            }
        };
        assert!(matches!(
            evt,
            Event::Error {
                class: ErrorClass::Unsupported,
                ..
            }
        ));
    }
}
