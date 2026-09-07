//! The engine: a turn machine over two queues. Surfaces send [`Command`]s
//! and consume [`Event`]s; the engine owns all sequencing.
//!
//! Two turn paths: the **canned** speaker (no model configured — keeps
//! `ka run` working keyless) and the **live voice** (real wires via
//! ka-dialect). Both honor interjections, deferrals, and aborts through the
//! same `select!` seam.

use std::collections::VecDeque;
use std::time::Duration;

use ka_protocol::{
    AskId, AskQuestion, Command, ContextMeter, DeltaKind, ErrorClass, Event, McpSummary, Mode,
    Stop, Usage,
};
use tokio::sync::mpsc;

use crate::canned;
use crate::config::Config;
use crate::fshooks::HookPoint;
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

/// Role selectors resolved against the engine's catalog at startup.
/// Roles are separate from the interactive default model (`/model`
/// keeps touching only `state.model`).
#[derive(Debug, Default, Clone)]
struct EngineRoles {
    /// Main-line selector (`[roles] default`, falling back to the
    /// configured model). Resolved (validated against the catalog) at
    /// startup alongside `fast`; the first non-title role consumers
    /// will read it.
    #[allow(dead_code)]
    default: Option<String>,
    /// Cheap/fast selector (`[roles] fast`); `None` degrades every
    /// fast-role consumer (auto-titles today) to its fallback.
    fast: Option<String>,
}

/// Resolve one role selector through the same catalog path as the main
/// model: it must parse, and its `vendor/model` must exist in the
/// catalog (locals arrive via the discovery overlay before the engine
/// starts). Unresolvable → `None`.
fn resolve_role(catalog: &ka_dialect::Catalog, selector: &str) -> Option<String> {
    let id = ka_dialect::parse_selector(selector).ok()?.model_id();
    catalog.get(&id).is_some().then_some(id)
}

/// Resolve the `[roles]` table against the catalog. `default` falls
/// back to the configured model; a missing/invalid `fast` simply stays
/// `None` (fast-role-gated features degrade gracefully).
fn resolve_roles(
    catalog: &ka_dialect::Catalog,
    roles: &crate::config::Roles,
    fallback_default: Option<&str>,
) -> EngineRoles {
    EngineRoles {
        default: roles
            .default
            .as_deref()
            .or(fallback_default)
            .and_then(|s| resolve_role(catalog, s)),
        fast: roles.fast.as_deref().and_then(|s| resolve_role(catalog, s)),
    }
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
                            images: Vec::new(),
                        })
                        .collect(),
                    images: Vec::new(),
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
    /// Checkpoints taken this session: (commit id, timestamp).
    checkpoints: Vec<(String, String)>,
    /// Spend/context guard thresholds + latches for this session.
    guards: crate::voice::GuardRuntime,
    /// Role selectors resolved against the catalog at engine start.
    roles: EngineRoles,
    /// Fresh strand awaiting its first completed turn: the auto-title
    /// fires once after that turn (fast role configured); resumed
    /// strands never re-title.
    needs_title: bool,
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
            checkpoints: Vec::new(),
            guards: crate::voice::GuardRuntime::new(c.guards.spend_usd, c.guards.context_pct),
            roles: EngineRoles::default(),
            needs_title: false,
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
    /// Shared handles to the connected MCP servers (watchdog + refresh).
    mcp_shared: Vec<crate::mcp::McpShared>,
    /// Watchdog/refresh bookkeeping from MCP supervisors.
    maintenance: mpsc::Receiver<crate::mcp::Maintenance>,
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
    let allowed_tools = config.permissions.allow.clone();
    let hooks = config.hooks.clone();
    // roles resolve once at engine start, through the same catalog the
    // main model uses (discovery overlays already applied by the caller)
    let roles = resolve_roles(&catalog, &config.roles, config.model.as_deref());
    let pathfinder_catalog = catalog.clone();
    let mut voice = Voice::new(catalog, cwd.clone(), mode, max_steps);
    voice.set_rules(rules);
    voice.set_allowed_tools(allowed_tools);
    voice.set_hooks(hooks);
    voice.set_bash_background_ms(config.effective_bash_background_after_ms());
    voice.set_fallbacks(config.fallback.models.clone());
    voice.set_max_image_mb(config.effective_max_image_mb());
    voice.set_context_promote(config.effective_context_promote());
    {
        let slot = voice.pathfinder_slot();
        slot.write().catalog = pathfinder_catalog;
    }
    let mut state = EngineState::from(config);
    state.roles = roles;
    let strand = attach_strand(&events, &mut state, &mut voice, &cwd, &strand_choice).await?;
    let (maintenance_tx, maintenance_rx) = mpsc::channel(16);
    let mut ctx = Ctx {
        cwd,
        events,
        state,
        voice,
        strand,
        mcp_shared: Vec::new(),
        maintenance: maintenance_rx,
    };
    // markdown agents: .ka/agents/*.md etc. become one `delegate` hand
    let agents = crate::agents::AgentDef::discover(&ctx.cwd);
    let agent_names: Vec<String> = agents.iter().map(|a| a.name.clone()).collect();
    if !agents.is_empty() {
        let slot = ctx.voice.pathfinder_slot();
        ctx.voice.push_hand(std::sync::Arc::new(
            crate::hands::delegate::DelegateHand::new(agents, slot, mode),
        ));
    }

    // MCP servers: spawn, handshake, list; each tool becomes a hand at
    // exec-tier clearance. Failures are per-server errors, never fatal.
    let mut mcp_summary = Vec::with_capacity(mcp_servers.len());
    for cfg in &mcp_servers {
        let connected = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            crate::mcp::McpClient::spawn_connect(cfg),
        )
        .await;
        match connected {
            Ok(Ok((client, tools))) => {
                let tool_count = tools.len();
                let shared = crate::mcp::McpShared::new(cfg.clone(), client, tools.clone());
                for tool in tools {
                    ctx.voice
                        .push_hand(std::sync::Arc::new(crate::mcp::McpHand::new(
                            tool,
                            shared.clone(),
                        )));
                }
                ctx.mcp_shared.push(shared.clone());
                tokio::spawn(crate::mcp::supervise(
                    shared,
                    maintenance_tx.clone(),
                    crate::mcp::WatchdogTiming::default(),
                ));
                mcp_summary.push(McpSummary {
                    name: cfg.name.clone(),
                    ok: true,
                    tools: tool_count,
                });
            }
            Ok(Err(e)) => {
                mcp_summary.push(McpSummary {
                    name: cfg.name.clone(),
                    ok: false,
                    tools: 0,
                });
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
                mcp_summary.push(McpSummary {
                    name: cfg.name.clone(),
                    ok: false,
                    tools: 0,
                });
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

    // One bootstrap inventory card replaces the old ad-hoc notes: the
    // surface renders tools/MCP/agents/skills compactly at startup.
    let skill_names: Vec<String> = crate::conventions::discover_skills(&ctx.cwd)
        .into_iter()
        .map(|s| s.name)
        .collect();
    // MCP prompts for the surface's /prompt popup (20s gate like tools)
    let mut prompts: Vec<String> = Vec::new();
    for shared in &ctx.mcp_shared {
        let listed =
            tokio::time::timeout(std::time::Duration::from_secs(20), shared.list_prompts()).await;
        if let Ok(Ok(items)) = listed {
            for p in items {
                prompts.push(format!(
                    "{}/{}{}",
                    p.server,
                    p.name,
                    if p.arguments.is_empty() {
                        String::new()
                    } else {
                        format!(" ({})", p.arguments.join(", "))
                    }
                ));
            }
        }
    }
    // browse hand over every server: resources + prompts discovery
    if !ctx.mcp_shared.is_empty() {
        ctx.voice
            .push_hand(std::sync::Arc::new(crate::mcp::McpMetaHand::new(
                ctx.mcp_shared.clone(),
            )));
    }
    ctx.events
        .send(Event::Inventory {
            tools: ctx.voice.hand_names(),
            mcp: mcp_summary,
            agents: agent_names,
            skills: skill_names,
            prompts,
        })
        .await
        .ok();
    // surfaces initialize the todo section immediately, before any turn
    ctx.events
        .send(Event::Todos { items: Vec::new() })
        .await
        .ok();
    loop {
        tokio::select! {
            maybe = commands.recv() => {
                let Some(cmd) = maybe else { break };
                handle_command(cmd, &mut commands, &mut ctx).await?;
            }
            maybe = ctx.maintenance.recv() => {
                if let Some(update) = maybe {
                    handle_maintenance(update, &mut ctx).await;
                }
            }
        }
    }
    // session shutdown: no backgrounded bash job may outlive the engine
    // detached jobs intentionally SURVIVE session exit; adopt any
    // survivors from earlier sessions into this table
    if let Some(path) = crate::hands::jobs::default_jobs_file() {
        ctx.voice.jobs().set_path(path);
    }
    Ok(())
}

/// The single, exhaustive command dispatcher. Every `Command` variant is
/// handled (or explicitly rejected) here — no unreachable arms, no
/// routing split. Arms that need to poll the surface mid-turn (Prompt)
/// also receive the receiver.
/// Apply one MCP supervisor update: swap the server's hands for the
/// fresh tool set and tell the surface.
async fn handle_maintenance(update: crate::mcp::Maintenance, ctx: &mut Ctx) {
    let (server, tools, note) = match update {
        crate::mcp::Maintenance::McpReconnected {
            server,
            tools,
            gained,
        } => (
            server,
            tools,
            format!("mcp {{server}} reconnected (+{gained} tools)"),
        ),
        crate::mcp::Maintenance::McpRefreshed {
            server,
            tools,
            delta,
        } => (
            server,
            tools,
            format!("mcp {{server}} refreshed ({delta:+} tools)"),
        ),
        crate::mcp::Maintenance::McpGaveUp { server } => {
            ctx.events
                .send(Event::Error {
                    class: ErrorClass::Protocol,
                    retryable: false,
                    message: format!("mcp {server}: reconnect failed; tools error until restart"),
                })
                .await
                .ok();
            return;
        }
    };
    if let Some(shared) = ctx.mcp_shared.iter().find(|s| s.name() == server).cloned() {
        let hands: Vec<std::sync::Arc<dyn crate::hands::Hand>> = tools
            .iter()
            .map(|t| std::sync::Arc::new(crate::mcp::McpHand::new(t.clone(), shared.clone())) as _)
            .collect();
        ctx.voice.replace_server_hands(&server, hands);
    }
    ctx.events
        .send(Event::Note {
            message: note.replace("{server}", &server),
        })
        .await
        .ok();
}

async fn handle_command(
    cmd: Command,
    commands: &mut mpsc::Receiver<Command>,
    ctx: &mut Ctx,
) -> Result<(), DynError> {
    match cmd {
        Command::Prompt {
            text,
            schema,
            images,
        } => {
            dispatch_turn(
                commands,
                &ctx.events,
                &mut ctx.state,
                &mut ctx.voice,
                text,
                schema,
                images,
                &mut ctx.strand,
                &ctx.cwd,
            )
            .await;
            settle_context(&mut ctx.voice, &mut ctx.state, &mut ctx.strand, &ctx.events).await;
            // auto-title once, right after the first completed turn of a
            // fresh strand (deferral follow-on turns are later turns)
            maybe_auto_title(&mut ctx.voice, &mut ctx.state, &mut ctx.strand, &ctx.events).await;
            // Settling: drain deferrals as follow-on turns.
            while let Some(deferred) = ctx.state.deferrals.pop_front() {
                dispatch_turn(
                    commands,
                    &ctx.events,
                    &mut ctx.state,
                    &mut ctx.voice,
                    deferred,
                    None,
                    Vec::new(),
                    &mut ctx.strand,
                    &ctx.cwd,
                )
                .await;
                settle_context(&mut ctx.voice, &mut ctx.state, &mut ctx.strand, &ctx.events).await;
            }
            ctx.events.send(Event::Idle).await.ok();
        }
        Command::RefreshMcp => {
            for shared in &ctx.mcp_shared {
                match shared.refresh_and_install().await {
                    Ok((tools, delta)) => {
                        if delta != 0 {
                            let hands: Vec<std::sync::Arc<dyn crate::hands::Hand>> = tools
                                .iter()
                                .map(|t| {
                                    std::sync::Arc::new(crate::mcp::McpHand::new(
                                        t.clone(),
                                        shared.clone(),
                                    )) as _
                                })
                                .collect();
                            ctx.voice.replace_server_hands(shared.name(), hands);
                        }
                        ctx.events
                            .send(Event::Note {
                                message: format!(
                                    "mcp {} refreshed ({delta:+} tools)",
                                    shared.name()
                                ),
                            })
                            .await
                            .ok();
                    }
                    Err(e) => {
                        ctx.events
                            .send(Event::Error {
                                class: ErrorClass::Protocol,
                                retryable: false,
                                message: e,
                            })
                            .await
                            .ok();
                    }
                }
            }
            ctx.events.send(Event::Idle).await.ok();
        }
        Command::CallPrompt { server, name, args } => {
            let Some(shared) = ctx.mcp_shared.iter().find(|s| s.name() == server).cloned() else {
                ctx.events
                    .send(Event::Error {
                        class: ErrorClass::Protocol,
                        retryable: false,
                        message: format!("mcp: no such server {server:?}"),
                    })
                    .await
                    .ok();
                ctx.events.send(Event::Idle).await.ok();
                return Ok(());
            };
            match shared.get_prompt(&name, &args).await {
                Ok(text) => {
                    dispatch_turn(
                        commands,
                        &ctx.events,
                        &mut ctx.state,
                        &mut ctx.voice,
                        text,
                        None,
                        Vec::new(),
                        &mut ctx.strand,
                        &ctx.cwd,
                    )
                    .await;
                    settle_context(&mut ctx.voice, &mut ctx.state, &mut ctx.strand, &ctx.events)
                        .await;
                    ctx.events.send(Event::Idle).await.ok();
                }
                Err(e) => {
                    ctx.events
                        .send(Event::Error {
                            class: ErrorClass::Protocol,
                            retryable: false,
                            message: format!("prompt {server}/{name}: {e}"),
                        })
                        .await
                        .ok();
                    ctx.events.send(Event::Idle).await.ok();
                }
            }
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
            ctx.events.send(Event::Idle).await.ok();
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
            ctx.events.send(Event::Idle).await.ok();
        }
        Command::SetEffort { level } => {
            ctx.state.effort = Some(level);
            let _ = ctx.strand.append(ka_strand::Record::Change {
                id: ka_strand::new_record_id(),
                model: None,
                effort: Some(level),
                mode: None,
            });
            ctx.events.send(Event::EffortChanged { level }).await?;
            ctx.events.send(Event::Idle).await.ok();
        }
        Command::Interject { text } => ctx.state.interjections.push(text),
        Command::Defer { text } => ctx.state.deferrals.push_back(text),
        Command::Abort => {}
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
                    // fresh sessions stay completely silent — only an
                    // existing-session switch earns one terse note
                    if !matches!(choice, StrandChoice::New) {
                        ctx.events
                            .send(Event::Note {
                                message: format!("switched to session {}", strand_id(&ctx.strand)),
                            })
                            .await
                            .ok();
                    }
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
        Command::ForkStrand { turns } => {
            match fork_strand(&ctx.cwd, &ctx.strand, turns) {
                Ok(path) => {
                    match attach_strand(
                        &ctx.events,
                        &mut ctx.state,
                        &mut ctx.voice,
                        &ctx.cwd,
                        &StrandChoice::Path(path),
                    )
                    .await
                    {
                        Ok(fresh) => {
                            ctx.strand = fresh;
                            // the fork carries the "(fork)" placeholder title:
                            // let the fast-role auto-title replace it
                            ctx.state.needs_title = true;
                        }
                        Err(e) => {
                            ctx.events
                                .send(Event::Error {
                                    class: ErrorClass::Protocol,
                                    retryable: false,
                                    message: format!("fork created but attach failed: {e}"),
                                })
                                .await
                                .ok();
                        }
                    }
                }
                Err(e) => {
                    ctx.events
                        .send(Event::Error {
                            class: ErrorClass::Unsupported,
                            retryable: false,
                            message: format!("fork failed: {e}"),
                        })
                        .await
                        .ok();
                }
            }
            ctx.events.send(Event::Idle).await.ok();
        }
        Command::Checkpoint => {
            match crate::checkpoint::snapshot(&ctx.cwd) {
                Ok(id) => {
                    let ts = ka_strand::now_rfc3339();
                    ctx.state.checkpoints.push((id.clone(), ts.clone()));
                    ctx.events
                        .send(Event::Note {
                            message: format!(
                                "checkpoint {} saved ({ts})",
                                crate::checkpoint::short(&id)
                            ),
                        })
                        .await
                        .ok();
                }
                Err(e) => {
                    ctx.events
                        .send(Event::Error {
                            class: ErrorClass::Internal,
                            retryable: false,
                            message: format!("checkpoint failed: {e}"),
                        })
                        .await
                        .ok();
                }
            }
            ctx.events.send(Event::Idle).await.ok();
        }
        Command::RestoreCheckpoint { id } => {
            if id.trim() == "list" {
                let message = if ctx.state.checkpoints.is_empty() {
                    "no checkpoints in this session yet".to_string()
                } else {
                    ctx.state
                        .checkpoints
                        .iter()
                        .map(|(cid, ts)| format!("{}  {ts}", crate::checkpoint::short(cid)))
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                ctx.events.send(Event::Note { message }).await.ok();
            } else {
                let resolved = ctx.state.checkpoints.iter().find(|(cid, _)| {
                    cid == &id
                        || crate::checkpoint::short(cid) == id.trim()
                        || cid.starts_with(id.trim())
                });
                match resolved.cloned() {
                    None => {
                        ctx.events
                            .send(Event::Error {
                                class: ErrorClass::Unsupported,
                                retryable: false,
                                message: format!("no checkpoint matches '{id}' in this session"),
                            })
                            .await
                            .ok();
                    }
                    Some((cid, ts)) => {
                        let answer = engine_ask(
                            &ctx.events,
                            commands,
                            AskId(format!("ckpt-restore-{cid}")),
                            format!(
                                "restore checkpoint {} ({ts})? overwrites working tree",
                                crate::checkpoint::short(&cid)
                            ),
                            vec!["restore".to_string(), "cancel".to_string()],
                        )
                        .await;
                        match answer {
                            Some(0) => match crate::checkpoint::restore(&ctx.cwd, &cid) {
                                Ok(()) => {
                                    ctx.events
                                        .send(Event::Note {
                                            message: format!(
                                                "restored checkpoint {}",
                                                crate::checkpoint::short(&cid)
                                            ),
                                        })
                                        .await
                                        .ok();
                                }
                                Err(e) => {
                                    ctx.events
                                        .send(Event::Error {
                                            class: ErrorClass::Internal,
                                            retryable: false,
                                            message: format!("restore failed: {e}"),
                                        })
                                        .await
                                        .ok();
                                }
                            },
                            _ => {
                                ctx.events
                                    .send(Event::Note {
                                        message: "canceled".to_string(),
                                    })
                                    .await
                                    .ok();
                            }
                        }
                    }
                }
            }
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
        Command::ExportMarkdown { out } => {
            let msg_count = ctx
                .strand
                .records()
                .iter()
                .filter(|r| matches!(r, ka_strand::Record::Message { .. }))
                .count();
            if msg_count == 0 {
                ctx.events
                    .send(Event::Error {
                        class: ErrorClass::Unsupported,
                        retryable: false,
                        message: "nothing to export — session is empty".to_string(),
                    })
                    .await
                    .ok();
            } else {
                let path = match out {
                    Some(p) => p,
                    None => {
                        let id = strand_id(&ctx.strand);
                        let tail: String = id
                            .chars()
                            .rev()
                            .take(8)
                            .collect::<String>()
                            .chars()
                            .rev()
                            .collect();
                        ctx.cwd.join(format!("ka-session-{tail}.md"))
                    }
                };
                let md = ka_strand::render_markdown(ctx.strand.records());
                match std::fs::write(&path, md) {
                    Ok(()) => {
                        ctx.events
                            .send(Event::Note {
                                message: format!(
                                    "exported {msg_count} messages → {}",
                                    path.display()
                                ),
                            })
                            .await
                            .ok();
                    }
                    Err(e) => {
                        ctx.events
                            .send(Event::Error {
                                class: ErrorClass::Internal,
                                retryable: false,
                                message: format!("export failed: {e}"),
                            })
                            .await
                            .ok();
                    }
                }
            }
            ctx.events.send(Event::Idle).await.ok();
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
    // interrupted-turn synthesis — observable when something was recovered
    let recovered = strand.synthesize_aborted().unwrap_or(false);
    // checkpoints and guard latches are session-scoped; re-arm them from
    // the strand we just attached to
    state.checkpoints.clear();
    let session_spend = strand
        .records()
        .iter()
        .filter_map(|r| match r {
            ka_strand::Record::Usage { cost, .. } => Some(*cost),
            _ => None,
        })
        .sum();
    state.guards.reset(session_spend);
    if recovered {
        events
            .send(Event::Note {
                message: "↩ recovered interrupted turn".to_string(),
            })
            .await
            .ok();
    }
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
    // a fresh strand (no prior user turns) may earn an auto-title after
    // its first completed turn; resumed strands never re-title
    state.needs_title = !strand.records().iter().any(|r| {
        matches!(
            r,
            ka_strand::Record::Message {
                role: ka_strand::Role::User,
                ..
            }
        )
    });
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
    // always announce the effective mode at bootstrap so surfaces render
    // the true state instead of an assumed default
    events
        .send(Event::ModeChanged { mode: state.mode })
        .await
        .ok();
    // one source of truth for titles: `ka_strand::title_of` over the
    // records. Surfaces learn the stored/heuristic title at bootstrap;
    // a live auto-title arrives as its own Title event later.
    let title = ka_strand::title_of(strand.records());
    if title != "(empty)" {
        events.send(Event::Title { title }).await.ok();
    }
    // replay resumed history so surfaces can rebuild the transcript;
    // emit unconditionally — an empty replay tells surfaces to clear
    // (fresh strands via /new rely on this to reset the transcript)
    let mut messages: Vec<ka_protocol::ReplayedMessage> = Vec::new();
    if voice.has_digest() {
        // the digest boundary replays as a divider row: everything
        // before it is carried by the summary in the system prompt
        messages.push(ka_protocol::ReplayedMessage {
            role: "digest".to_string(),
            content: String::new(),
            digest: true,
        });
    }
    messages.extend(
        voice
            .history
            .iter()
            .filter(|m| !m.content.trim().is_empty())
            .map(|m| ka_protocol::ReplayedMessage {
                role: match m.role {
                    ka_dialect::speaker::TurnRole::User => "user".to_string(),
                    _ => "assistant".to_string(),
                },
                content: m.content.clone(),
                digest: false,
            }),
    );
    events.send(Event::Replay { messages }).await.ok();
    Ok(strand)
}

/// Auto-title budget: one short fast-role call, hard-capped.
const TITLE_TIMEOUT: Duration = Duration::from_secs(10);
const TITLE_SYSTEM: &str = "You write terse conversation titles.";
/// Chars of each side fed to the title prompt.
const TITLE_SIDE_CAP: usize = 2_000;

/// After the first completed turn of a fresh strand, ask the fast role
/// (when configured) for a 3-6 word title, persist it as a
/// [`ka_strand::Record::Title`], and announce it via `Event::Title` so
/// the running surface relabels the session live. Every failure path is
/// silent: the existing first-user-message heuristic stays, and no
/// error ever reaches the transcript. One attempt per strand.
async fn maybe_auto_title(
    voice: &mut Voice,
    state: &mut EngineState,
    strand: &mut ka_strand::StrandFile,
    events: &mpsc::Sender<Event>,
) {
    if !state.needs_title {
        return;
    }
    // a completed turn has an assistant reply; without one (error/abort
    // before any text) the turn is not complete enough to title
    let Some(reply) = voice
        .history
        .iter()
        .rev()
        .find(|m| {
            m.role == ka_dialect::speaker::TurnRole::Assistant && !m.content.trim().is_empty()
        })
        .map(|m| side_view(&m.content))
    else {
        return;
    };
    // one attempt, ever — success or failure, the heuristic takes over
    state.needs_title = false;
    let Some(fast) = state.roles.fast.clone() else {
        return; // fast unconfigured: the first-user-message heuristic stays
    };
    let Some(user) = voice
        .history
        .iter()
        .find(|m| m.role == ka_dialect::speaker::TurnRole::User)
        .map(|m| side_view(&m.content))
    else {
        return;
    };
    let prompt = format!(
        "Generate a 3-6 word title for this conversation. Reply with only the title.\n\n\
         <user>\n{user}\n</user>\n\n<assistant>\n{reply}\n</assistant>"
    );
    let Some(raw) = voice
        .role_complete(&fast, TITLE_SYSTEM, &prompt, TITLE_TIMEOUT)
        .await
    else {
        return; // failed/timed out: silent fallback
    };
    let title = clean_title(&raw);
    if title.is_empty() {
        return;
    }
    let _ = strand.append(ka_strand::Record::Title {
        id: ka_strand::new_record_id(),
        title: title.clone(),
    });
    events.send(Event::Title { title }).await.ok();
}

/// Cap one side of the title prompt.
fn side_view(s: &str) -> String {
    s.chars().take(TITLE_SIDE_CAP).collect()
}

/// Normalize a model-proposed title: first non-empty line, wrapping
/// quotes/markup stripped, whitespace collapsed, capped at the same 60
/// chars [`ka_strand::title_of`] uses.
fn clean_title(raw: &str) -> String {
    let line = raw
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let line = line
        .trim_matches(|c: char| matches!(c, '"' | '\'' | '`' | '*' | '#'))
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    line.chars().take(60).collect()
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
    // speculative zone: pressure ≥80% but not yet tripping — fire the
    // background candidate so the next real digest is instant
    if !voice.context_pressure(window) && voice.context_pressure_frac(window, 80) {
        voice.start_speculative(state.roles.fast.as_deref());
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
    let ratio = voice_ratio(voice);
    // a ready speculative candidate with a matching watermark skips the
    // synchronous summarize entirely
    if focus.is_none() {
        if let Some(summary) = voice.take_speculative().await {
            let _ = voice.apply_digest(summary, ratio);
            persist_delta(voice, state, strand);
            events
                .send(Event::Note {
                    message: "context digested (speculative)".to_string(),
                })
                .await
                .ok();
            return DigestResult::Digested;
        }
    }
    events.send(Event::DigestStarted).await.ok();
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
#[allow(clippy::too_many_arguments)]
async fn dispatch_turn(
    commands: &mut mpsc::Receiver<Command>,
    events: &mpsc::Sender<Event>,
    state: &mut EngineState,
    voice: &mut Voice,
    text: String,
    schema: Option<serde_json::Value>,
    images: Vec<ka_dialect::ImagePart>,
    strand: &mut ka_strand::StrandFile,
    cwd: &std::path::Path,
) {
    if let Err(note) = crate::fshooks::run(HookPoint::PreTurn, cwd, None).await {
        events.send(Event::Note { message: note }).await.ok();
    }
    let usage = if let Some(model) = state.model.clone() {
        voice
            .turn(
                &model,
                text,
                commands,
                events,
                &mut state.interjections,
                &mut state.deferrals,
                &mut state.guards,
                schema,
                images,
            )
            .await
    } else {
        let mut history = std::mem::take(&mut voice.history);
        let usage = turn_canned(commands, events, state, &mut history, text).await;
        voice.history = history;
        usage
    };
    // session spend accounting + the persisted Usage record feed the
    // spend guard and `ka sessions` stats
    state.guards.session_spend += usage.cost;
    let _ = strand.append(ka_strand::Record::Usage {
        id: ka_strand::new_record_id(),
        cost: usage.cost,
        input: usage.input,
        output: usage.output,
        cache_read: usage.cache_read,
        cache_write: usage.cache_write,
    });
    persist_delta(voice, state, strand);
    if let Err(note) = crate::fshooks::run(HookPoint::PostTurn, cwd, None).await {
        events.send(Event::Note { message: note }).await.ok();
    }
    // overflow promotion: apply the same switch path as /model
    if let Some(selector) = voice.take_promotion() {
        state.model = Some(selector.clone());
        voice.pathfinder_slot().write().model = Some(selector.clone());
        let _ = strand.append(ka_strand::Record::Change {
            id: ka_strand::new_record_id(),
            model: Some(selector.clone()),
            effort: None,
            mode: None,
        });
        events.send(Event::ModelChanged { selector }).await.ok();
    }
}

/// Copy the current strand into a new strand file truncated to drop the
/// last `turns` user turns (0 = exact copy), titled "<original> (fork)".
fn fork_strand(
    cwd: &std::path::Path,
    current: &ka_strand::StrandFile,
    turns: u32,
) -> Result<std::path::PathBuf, String> {
    let records = current.records();
    let user_idx: Vec<usize> = records
        .iter()
        .enumerate()
        .filter(|(_, r)| {
            matches!(
                r,
                ka_strand::Record::Message {
                    role: ka_strand::Role::User,
                    ..
                }
            )
        })
        .map(|(i, _)| i)
        .collect();
    if turns as usize > user_idx.len() {
        return Err(format!(
            "cannot drop {turns} turn(s): session has {} user turn(s)",
            user_idx.len()
        ));
    }
    let cut = match turns {
        0 => records.len(),
        n => user_idx[user_idx.len() - n as usize],
    };
    let mut fork =
        ka_strand::StrandFile::create(cwd, repo_snapshot(cwd)).map_err(|e| e.to_string())?;
    let parent_id = records.iter().find_map(|r| match r {
        ka_strand::Record::Header { id, .. } => Some(id.0.clone()),
        _ => None,
    });
    if let Some(parent_id) = &parent_id {
        fork.set_parent(parent_id);
    }
    for record in &records[..cut] {
        if matches!(record, ka_strand::Record::Header { .. }) {
            continue;
        }
        fork.append(record.clone()).map_err(|e| e.to_string())?;
    }
    let title = format!("{} (fork)", ka_strand::title_of(records));
    fork.append(ka_strand::Record::Title {
        id: ka_strand::new_record_id(),
        title,
    })
    .map_err(|e| e.to_string())?;
    fork.path()
        .map(std::path::Path::to_path_buf)
        .ok_or_else(|| "fork was never materialized".to_string())
}

/// Pose a question and wait for the surface's answer. `None` = the
/// surface aborted or went away.
async fn engine_ask(
    events: &mpsc::Sender<Event>,
    commands: &mut mpsc::Receiver<Command>,
    id: AskId,
    question: String,
    options: Vec<String>,
) -> Option<usize> {
    events
        .send(Event::Ask {
            id: id.clone(),
            questions: vec![AskQuestion {
                text: question,
                options,
            }],
        })
        .await
        .ok();
    loop {
        tokio::select! {
            maybe = commands.recv() => {
                match maybe {
                    None => return None,
                    Some(Command::Abort) => return None,
                    Some(Command::Answer { question: q, choice }) if q == id => {
                        return Some(choice);
                    }
                    Some(_) => {}
                }
            }
        }
    }
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
/// mid-stream. Used when no model is configured. Returns the turn's
/// (estimated) usage.
async fn turn_canned(
    commands: &mut mpsc::Receiver<Command>,
    events: &mpsc::Sender<Event>,
    state: &mut EngineState,
    history: &mut Vec<ka_dialect::speaker::TurnMessage>,
    text: String,
) -> Usage {
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
                        return Usage::default();
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
        return Usage::default();
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
    let usage = Usage {
        input: est_in,
        output: est_out,
        ..Usage::default()
    };
    events
        .send(Event::TurnFinished {
            stop: Stop::Done,
            usage,
        })
        .await
        .ok();
    usage
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use ka_protocol::{Command, ErrorClass, Event, Stop};
    use std::time::Duration;
    use tokio::sync::mpsc;

    use crate::config::Config;
    use crate::engine::{EngineHandle, spawn};

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
                schema: None,
                images: Vec::new(),
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
                schema: None,
                images: Vec::new(),
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
                schema: None,
                images: Vec::new(),
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
                schema: None,
                images: Vec::new(),
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
                schema: None,
                images: Vec::new(),
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
    async fn export_markdown_writes_and_rejects_empty_sessions() {
        use crate::engine::{StrandChoice, spawn_full};

        let data = std::env::temp_dir().join(format!("ka-eng-export-{}", std::process::id()));
        let work = data.join("work");
        let _ = std::fs::remove_dir_all(&data);
        std::fs::create_dir_all(&work).unwrap();
        ka_strand::set_data_dir_for_tests(data.join("data"));

        let cfg = Config {
            cwd: Some(work.display().to_string()),
            ..Default::default()
        };
        let mut h = spawn_full(
            cfg.clone(),
            ka_dialect::Catalog::embedded(),
            StrandChoice::New,
        );
        h.commands
            .send(Command::Prompt {
                text: "hi".into(),
                schema: None,
                images: Vec::new(),
            })
            .await
            .unwrap();
        while let Some(evt) = h.events.recv().await {
            if matches!(evt, Event::TurnFinished { .. }) {
                break;
            }
        }
        let out = work.join("out.md");
        h.commands
            .send(Command::ExportMarkdown {
                out: Some(out.clone()),
            })
            .await
            .unwrap();
        let mut saw_note = false;
        while let Some(evt) = h.events.recv().await {
            if let Event::Note { message } = evt {
                assert!(
                    message.contains("exported") && message.contains("out.md"),
                    "{message}"
                );
                saw_note = true;
                break;
            }
        }
        assert!(saw_note, "export must announce its output path");
        let md = std::fs::read_to_string(&out).unwrap();
        assert!(md.contains("### **you**"), "{md}");
        assert!(md.contains("hi"), "{md}");
        drop(h);
        tokio::time::sleep(Duration::from_millis(50)).await;

        // empty session: error, and no default-named file appears in cwd
        let mut h2 = spawn_full(cfg, ka_dialect::Catalog::embedded(), StrandChoice::New);
        h2.commands
            .send(Command::ExportMarkdown { out: None })
            .await
            .unwrap();
        let mut saw_err = false;
        while let Some(evt) = h2.events.recv().await {
            if let Event::Error { message, .. } = evt {
                assert!(message.contains("nothing to export"), "{message}");
                saw_err = true;
                break;
            }
        }
        assert!(saw_err, "empty session export must error");
        drop(h2);
        tokio::time::sleep(Duration::from_millis(50)).await;
        let leftovers: Vec<String> = std::fs::read_dir(&work)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with("ka-session-"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        let _ = std::fs::remove_dir_all(&data);
    }

    #[tokio::test]
    async fn fresh_bootstrap_emits_single_empty_replay() {
        use crate::engine::{StrandChoice, spawn_full};
        use ka_protocol::Command;

        let data = std::env::temp_dir().join(format!("ka-eng-boot-replay-{}", std::process::id()));
        let work = data.join("work");
        let _ = std::fs::remove_dir_all(&data);
        std::fs::create_dir_all(&work).unwrap();
        ka_strand::set_data_dir_for_tests(data.clone());

        let cfg = Config {
            cwd: Some(work.display().to_string()),
            ..Default::default()
        };
        // fresh session: exactly one Replay, with zero messages
        let mut h = spawn_full(
            cfg.clone(),
            ka_dialect::Catalog::embedded(),
            StrandChoice::New,
        );
        let mut replays: Vec<usize> = Vec::new();
        loop {
            match tokio::time::timeout(Duration::from_millis(300), h.events.recv()).await {
                Ok(Some(Event::Replay { messages })) => replays.push(messages.len()),
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            }
        }
        assert_eq!(
            replays,
            vec![0],
            "fresh bootstrap: one empty replay, got {replays:?}"
        );
        drop(h);
        tokio::time::sleep(Duration::from_millis(50)).await;

        // resumed session with content: exactly one Replay, populated
        let mut h2 = spawn_full(
            cfg.clone(),
            ka_dialect::Catalog::embedded(),
            StrandChoice::Latest,
        );
        h2.commands
            .send(Command::Prompt {
                text: "hello".into(),
                schema: None,
                images: Vec::new(),
            })
            .await
            .unwrap();
        while let Some(evt) = h2.events.recv().await {
            if matches!(evt, Event::Idle) {
                break;
            }
        }
        drop(h2);
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut h3 = spawn_full(cfg, ka_dialect::Catalog::embedded(), StrandChoice::Latest);
        let mut replays: Vec<usize> = Vec::new();
        loop {
            match tokio::time::timeout(Duration::from_millis(300), h3.events.recv()).await {
                Ok(Some(Event::Replay { messages })) => replays.push(messages.len()),
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            }
        }
        assert_eq!(
            replays.len(),
            1,
            "resumed bootstrap: exactly one replay, got {replays:?}"
        );
        assert_eq!(
            replays[0], 2,
            "resumed replay covers the prior exchange, got {replays:?}"
        );
        drop(h3);
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
                    schema: None,
                    images: Vec::new(),
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
                schema: None,
                images: Vec::new(),
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
                | Event::Replay { .. }
                | Event::Inventory { .. }
                | Event::Todos { .. } => continue,
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

    /// Drain events until the next Idle, collecting what came before.
    async fn drain_to_idle(handle: &mut EngineHandle) -> Vec<Event> {
        let mut seen = Vec::new();
        loop {
            let evt = tokio::time::timeout(Duration::from_millis(2_000), handle.events.recv())
                .await
                .unwrap()
                .unwrap();
            let idle = matches!(evt, Event::Idle);
            seen.push(evt);
            if idle {
                return seen;
            }
        }
    }

    async fn prompt_and_settle(handle: &mut EngineHandle, text: &str) {
        handle
            .commands
            .send(Command::Prompt {
                text: text.to_string(),
                schema: None,
                images: Vec::new(),
            })
            .await
            .unwrap();
        drain_to_idle(handle).await;
    }

    #[tokio::test]
    async fn fork_truncates_copies_and_rejects_overlarge_turns() {
        use crate::engine::{StrandChoice, spawn_full};

        let data = std::env::temp_dir().join(format!("ka-eng-fork-{}", std::process::id()));
        let work = data.join("work");
        let _ = std::fs::remove_dir_all(&data);
        std::fs::create_dir_all(&work).unwrap();
        ka_strand::set_data_dir_for_tests(data.clone());

        let cfg = Config {
            cwd: Some(work.display().to_string()),
            ..Default::default()
        };
        let mut handle = spawn_full(cfg, ka_dialect::Catalog::embedded(), StrandChoice::New);
        prompt_and_settle(&mut handle, "first question").await;
        prompt_and_settle(&mut handle, "second question").await;

        let sessions = ka_strand::list(&work).unwrap();
        assert_eq!(sessions.len(), 1);
        let original_id = sessions[0].id.clone();
        let original_path = sessions[0].path.clone();

        // fork dropping the last user turn: copy ends after exchange one,
        // titled "<first user message> (fork)", and the engine switches to it
        handle
            .commands
            .send(Command::ForkStrand { turns: 1 })
            .await
            .unwrap();
        let events = drain_to_idle(&mut handle).await;
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::SessionInfo { id } if *id != original_id)),
            "fork must announce a new session: {events:?}"
        );
        let sessions = ka_strand::list(&work).unwrap();
        assert_eq!(sessions.len(), 2, "{sessions:?}");
        let fork = sessions.iter().find(|s| s.id != original_id).unwrap();
        assert_eq!(fork.title, "first question (fork)");
        assert_eq!(fork.messages, 2, "last user turn dropped: {fork:?}");

        // overlarge turns: Error + Idle, no new strand, engine unchanged
        handle
            .commands
            .send(Command::ForkStrand { turns: 3 })
            .await
            .unwrap();
        let events = drain_to_idle(&mut handle).await;
        assert!(
            events.iter().any(|e| matches!(e, Event::Error { .. })),
            "overlarge fork must error: {events:?}"
        );
        assert_eq!(ka_strand::list(&work).unwrap().len(), 2);

        // exact copy (turns = 0) of the ACTIVE strand (the first fork):
        // same messages, fork-of-fork title
        handle
            .commands
            .send(Command::ForkStrand { turns: 0 })
            .await
            .unwrap();
        drain_to_idle(&mut handle).await;
        let sessions = ka_strand::list(&work).unwrap();
        assert_eq!(sessions.len(), 3, "{sessions:?}");
        let copy = sessions
            .iter()
            .find(|s| s.id != original_id && s.path != fork.path)
            .unwrap();
        assert_eq!(copy.title, "first question (fork) (fork)");
        assert_eq!(copy.messages, fork.messages, "exact copy keeps messages");
        let copy_records = ka_strand::read(&copy.path).unwrap();
        let fork_records = ka_strand::read(&fork.path).unwrap();
        let msg_contents = |records: &[ka_strand::Record]| -> Vec<String> {
            records
                .iter()
                .filter_map(|r| match r {
                    ka_strand::Record::Message { content, .. } => Some(content.clone()),
                    _ => None,
                })
                .collect()
        };
        assert_eq!(msg_contents(&copy_records), msg_contents(&fork_records));

        // the ORIGINAL strand file is untouched by all three forks
        let original = ka_strand::read(&original_path).unwrap();
        assert_eq!(msg_contents(&original).len(), 4);
        drop(handle);
        let _ = std::fs::remove_dir_all(&data);
    }

    /// Spawn with `cwd` pinned to a fresh empty dir, drop the command
    /// side, and collect everything the engine emits at bootstrap.
    async fn drain_bootstrap(tag: &str, mut cfg: Config) -> Vec<Event> {
        let root = std::env::temp_dir().join(format!(
            "ka-eng-inv-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        cfg.cwd = Some(root.to_string_lossy().into_owned());
        let mut handle = spawn(cfg);
        drop(handle.commands);
        let mut seen = Vec::new();
        while let Some(evt) = handle.events.recv().await {
            seen.push(evt);
        }
        let _ = std::fs::remove_dir_all(&root);
        seen
    }

    #[tokio::test]
    async fn bootstrap_emits_exactly_one_inventory() {
        let seen = drain_bootstrap("bare", Config::default()).await;
        let mut inventories = seen.iter().filter(|e| matches!(e, Event::Inventory { .. }));
        assert!(
            inventories.next().is_some(),
            "bootstrap must emit an Inventory: {seen:?}"
        );
        assert!(
            inventories.next().is_none(),
            "exactly one Inventory expected: {seen:?}"
        );
        // the ad-hoc inventory notes are gone
        assert!(
            seen.iter().all(|e| !matches!(
                e,
                Event::Note { message } if message.contains("agents available")
            )),
            "no agents-available note expected: {seen:?}"
        );
    }

    #[tokio::test]
    async fn bootstrap_inventory_lists_agents_skills_and_failed_mcp() {
        let root = std::env::temp_dir().join(format!(
            "ka-eng-inv-rich-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".ka/agents")).unwrap();
        // project-scope skills are gated on the trust store: redirect the
        // store to a fresh file and trust the injected project dir
        let _trust = crate::trust::test_support::trust_guard();
        crate::trust::approve(&root);
        std::fs::write(
            root.join(".ka/agents/reviewer.md"),
            "---\nname: reviewer\ndescription: reviews code\n---\nBe harsh.\n",
        )
        .unwrap();
        for name in ["demo", "second"] {
            std::fs::create_dir_all(root.join(format!(".ka/skills/{name}"))).unwrap();
            std::fs::write(
                root.join(format!(".ka/skills/{name}/SKILL.md")),
                format!("---\ndescription: {name} skill\n---\nDo things.\n"),
            )
            .unwrap();
        }
        let cfg = Config {
            cwd: Some(root.to_string_lossy().into_owned()),
            mcp: vec![crate::mcp::McpServerConfig {
                name: "ghost".into(),
                command: Some("ka-definitely-not-a-binary-9x7".into()),
                args: Vec::new(),
                env: Default::default(),
                url: None,
                headers: Vec::new(),
            }],
            ..Config::default()
        };
        let mut handle = spawn(cfg);
        drop(handle.commands);
        let mut seen = Vec::new();
        while let Some(evt) = handle.events.recv().await {
            seen.push(evt);
        }
        let _ = std::fs::remove_dir_all(&root);

        // the spawn failure stays visible as an Error naming the server
        let errors: Vec<String> = seen
            .iter()
            .filter_map(|e| match e {
                Event::Error { message, .. } => Some(message.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(errors.len(), 1, "one mcp failure error: {seen:?}");
        assert!(errors[0].contains("ghost"), "error names the server");

        // exactly one Inventory carrying the injected counts
        let inventories: Vec<&Event> = seen
            .iter()
            .filter(|e| matches!(e, Event::Inventory { .. }))
            .collect();
        assert_eq!(inventories.len(), 1, "exactly one Inventory: {seen:?}");
        let Event::Inventory {
            tools,
            mcp,
            agents,
            skills,
            prompts: _,
        } = inventories[0]
        else {
            unreachable!("filtered above")
        };
        assert_eq!(agents, &["reviewer".to_string()]);
        // discovery also sees the user HOME layer, so only the injected
        // names are asserted exactly
        assert!(skills.contains(&"demo".to_string()), "skills: {skills:?}");
        assert!(skills.contains(&"second".to_string()), "skills: {skills:?}");
        assert_eq!(
            mcp,
            &[ka_protocol::McpSummary {
                name: "ghost".into(),
                ok: false,
                tools: 0,
            }]
        );
        // 7 built-ins + todo + jobs + the delegate hand the discovered
        // agent adds
        assert_eq!(tools.len(), 10, "tools: {tools:?}");
        assert!(tools.contains(&"delegate".to_string()), "tools: {tools:?}");
        assert!(tools.contains(&"todo".to_string()), "tools: {tools:?}");
        assert!(tools.contains(&"jobs".to_string()), "tools: {tools:?}");
        // and the ad-hoc notes stay gone
        assert!(seen.iter().all(
            |e| !matches!(e, Event::Note { message } if message.contains("agents available") || message.contains("tool(s)"))
        ));
    }
    // ── auto-titles ([roles] fast) ────────────────────────────────────

    const TURN_SSE: &str = concat!(
        "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"Sure thing.\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2}}\n\n",
        "data: [DONE]\n\n",
    );
    const TITLE_SSE: &str = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"Fix the parser\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    );

    /// Serve a queue of canned SSE bodies over a blocking-socket listener;
    /// each HTTP request pops the next body and is captured.
    fn serve_sse(
        bodies: Vec<&'static str>,
        captured: std::sync::Arc<parking_lot::Mutex<Vec<String>>>,
    ) -> std::net::SocketAddr {
        use std::io::{Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for body in bodies {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let mut buf: Vec<u8> = Vec::new();
                let mut tmp = [0u8; 8192];
                loop {
                    match stream.read(&mut tmp) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            buf.extend_from_slice(&tmp[..n]);
                            if request_complete(&buf) {
                                break;
                            }
                        }
                    }
                }
                captured
                    .lock()
                    .push(String::from_utf8_lossy(&buf).into_owned());
                let http = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: \
                     close\r\n\r\n{body}"
                );
                let _ = stream.write_all(http.as_bytes());
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
        });
        addr
    }

    fn request_complete(buf: &[u8]) -> bool {
        let Ok(s) = std::str::from_utf8(buf) else {
            return false;
        };
        let Some(pos) = s.find("\r\n\r\n") else {
            return false;
        };
        s.to_ascii_lowercase()
            .lines()
            .find_map(|l| l.strip_prefix("content-length:"))
            .and_then(|v| v.trim().parse::<usize>().ok())
            .is_none_or(|cl| buf.len() >= pos + 4 + cl)
    }

    fn live_catalog(
        main_addr: std::net::SocketAddr,
        fast_addr: Option<std::net::SocketAddr>,
    ) -> ka_dialect::Catalog {
        let mut text = format!(
            "[dialects.\"test/main\"]\nwire = \"openai_chat\"\nbase_url = \
             \"http://{main_addr}/v1\"\ncontext = 100000\n"
        );
        if let Some(fast) = fast_addr {
            text.push_str(&format!(
                "\n[dialects.\"test/fast\"]\nwire = \"openai_chat\"\nbase_url = \
                 \"http://{fast}/v1\"\ncontext = 100000\n"
            ));
        }
        ka_dialect::Catalog::parse(&text).unwrap()
    }

    fn title_workdir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ka-eng-title-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn title_cfg(work: &std::path::Path, fast: Option<&str>) -> Config {
        Config {
            model: Some("test/main".into()),
            roles: crate::config::Roles {
                default: None,
                fast: fast.map(str::to_string),
            },
            cwd: Some(work.display().to_string()),
            ..Config::default()
        }
    }

    fn title_events(events: &[Event]) -> Vec<&str> {
        events
            .iter()
            .filter_map(|e| match e {
                Event::Title { title } => Some(title.as_str()),
                _ => None,
            })
            .collect()
    }

    fn stored_titles(work: &std::path::Path) -> Vec<String> {
        let sessions = ka_strand::list(work).unwrap();
        sessions
            .first()
            .map(|s| {
                ka_strand::read(&s.path)
                    .unwrap()
                    .iter()
                    .filter_map(|r| match r {
                        ka_strand::Record::Title { title, .. } => Some(title.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn auto_title_generated_via_fast_role_and_persisted() {
        use crate::engine::{StrandChoice, spawn_full};
        let captured = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let addr = serve_sse(vec![TURN_SSE, TITLE_SSE], captured.clone());
        let work = title_workdir("ok");
        let cfg = title_cfg(&work, Some("test/main"));
        let mut h = spawn_full(cfg, live_catalog(addr, None), StrandChoice::New);
        h.commands
            .send(Command::Prompt {
                text: "please fix the parser in src/x.rs".into(),
                schema: None,
                images: Vec::new(),
            })
            .await
            .unwrap();
        let events = drain_to_idle(&mut h).await;
        assert_eq!(
            title_events(&events),
            vec!["Fix the parser"],
            "one cleaned title event: {events:?}"
        );
        assert!(
            events.iter().all(|e| !matches!(e, Event::Error { .. })),
            "title flow must never surface errors: {events:?}"
        );
        // exactly two wire calls: the turn, then the title
        assert_eq!(captured.lock().len(), 2, "requests: {:?}", captured.lock());
        // the record is persisted and the summary reads it back
        assert_eq!(stored_titles(&work), vec!["Fix the parser".to_string()]);
        let sessions = ka_strand::list(&work).unwrap();
        assert_eq!(sessions[0].title, "Fix the parser");
        let _ = std::fs::remove_dir_all(&work);
    }

    #[tokio::test]
    async fn auto_title_failure_is_silent_and_keeps_heuristic() {
        use crate::engine::{StrandChoice, spawn_full};
        let captured = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let addr = serve_sse(vec![TURN_SSE], captured.clone());
        // a port with no listener: the fast call fails to connect
        let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let dead_addr = dead.local_addr().unwrap();
        drop(dead);
        let work = title_workdir("fail");
        let cfg = title_cfg(&work, Some("test/fast"));
        let mut h = spawn_full(cfg, live_catalog(addr, Some(dead_addr)), StrandChoice::New);
        h.commands
            .send(Command::Prompt {
                text: "please fix the parser in src/x.rs".into(),
                schema: None,
                images: Vec::new(),
            })
            .await
            .unwrap();
        let events = drain_to_idle(&mut h).await;
        assert!(
            title_events(&events).is_empty(),
            "failed title must stay silent: {events:?}"
        );
        assert!(
            events.iter().all(|e| !matches!(e, Event::Error { .. })),
            "title failures never reach the transcript: {events:?}"
        );
        assert_eq!(captured.lock().len(), 1, "only the turn ran");
        assert!(
            stored_titles(&work).is_empty(),
            "no Title record on failure"
        );
        // the heuristic survives: summary title = first user message
        let sessions = ka_strand::list(&work).unwrap();
        assert!(sessions[0].title.starts_with("please fix the parser"));
        let _ = std::fs::remove_dir_all(&work);
    }

    #[tokio::test]
    async fn auto_title_unconfigured_keeps_heuristic() {
        use crate::engine::{StrandChoice, spawn_full};
        let captured = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let addr = serve_sse(vec![TURN_SSE], captured.clone());
        let work = title_workdir("nofast");
        let cfg = title_cfg(&work, None);
        let mut h = spawn_full(cfg, live_catalog(addr, None), StrandChoice::New);
        h.commands
            .send(Command::Prompt {
                text: "please fix the parser in src/x.rs".into(),
                schema: None,
                images: Vec::new(),
            })
            .await
            .unwrap();
        let events = drain_to_idle(&mut h).await;
        assert!(
            title_events(&events).is_empty(),
            "no title without [roles] fast: {events:?}"
        );
        assert_eq!(captured.lock().len(), 1, "only the turn ran");
        assert!(stored_titles(&work).is_empty());
        let sessions = ka_strand::list(&work).unwrap();
        assert!(sessions[0].title.starts_with("please fix the parser"));
        let _ = std::fs::remove_dir_all(&work);
    }

    #[tokio::test]
    async fn auto_title_never_refires_on_resume() {
        use crate::engine::{StrandChoice, spawn_full};
        let work = title_workdir("resume");
        // session 1: the title fires (turn + title calls)
        let cap_a = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let addr_a = serve_sse(vec![TURN_SSE, TITLE_SSE], cap_a.clone());
        let cfg = title_cfg(&work, Some("test/main"));
        let mut h1 = spawn_full(cfg.clone(), live_catalog(addr_a, None), StrandChoice::New);
        h1.commands
            .send(Command::Prompt {
                text: "please fix the parser in src/x.rs".into(),
                schema: None,
                images: Vec::new(),
            })
            .await
            .unwrap();
        let first = drain_to_idle(&mut h1).await;
        assert_eq!(title_events(&first), vec!["Fix the parser"]);
        drop(h1);
        // session 2 (resume): the stored title is announced once at
        // bootstrap, no second call ever fires
        let cap_b = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let addr_b = serve_sse(vec![TURN_SSE], cap_b.clone());
        let mut h2 = spawn_full(cfg, live_catalog(addr_b, None), StrandChoice::Latest);
        // bootstrap ends at Replay; the stored title must echo there
        let mut bootstrap = Vec::new();
        while let Ok(Some(evt)) =
            tokio::time::timeout(Duration::from_secs(5), h2.events.recv()).await
        {
            let done = matches!(evt, Event::Replay { .. });
            bootstrap.push(evt);
            if done {
                break;
            }
        }
        assert_eq!(
            title_events(&bootstrap),
            vec!["Fix the parser"],
            "bootstrap carries the stored title: {bootstrap:?}"
        );
        h2.commands
            .send(Command::Prompt {
                text: "second question".into(),
                schema: None,
                images: Vec::new(),
            })
            .await
            .unwrap();
        let second = drain_to_idle(&mut h2).await;
        assert!(
            title_events(&second).is_empty(),
            "resume must not re-title: {second:?}"
        );
        assert_eq!(
            cap_b.lock().len(),
            1,
            "only the second turn ran: {:?}",
            cap_b.lock()
        );
        assert_eq!(
            stored_titles(&work),
            vec!["Fix the parser".to_string()],
            "exactly one Title record, ever"
        );
        let _ = std::fs::remove_dir_all(&work);
    }

    #[test]
    fn fork_records_parent_linkage() {
        let work = std::env::temp_dir().join(format!("ka-fork-parent-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&work);
        std::fs::create_dir_all(&work).unwrap();
        let mut parent = ka_strand::StrandFile::create(&work, None).unwrap();
        parent
            .append(ka_strand::Record::Message {
                id: ka_strand::new_record_id(),
                role: ka_strand::Role::User,
                content: "hello".into(),
                calls: Vec::new(),
                results: Vec::new(),
            })
            .unwrap();

        let child_path = super::fork_strand(&work, &parent, 0).expect("fork created");
        let child = ka_strand::StrandFile::open(&child_path).expect("child opens");
        let parent_id = parent
            .records()
            .iter()
            .find_map(|r| match r {
                ka_strand::Record::Header { id, .. } => Some(id.0.clone()),
                _ => None,
            })
            .unwrap();
        let child_parent = child.records().iter().find_map(|r| match r {
            ka_strand::Record::Header { parent, .. } => parent.clone(),
            _ => None,
        });
        assert_eq!(child_parent.as_deref(), Some(parent_id.as_str()));
        let _ = std::fs::remove_dir_all(&work);
    }
}
