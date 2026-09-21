//! ka wire protocol: the `Command`/`Event` contract between surfaces and the
//! engine. Both enums are NDJSON-serializable — the headless surface literally
//! prints [`Event`] lines to stdout, and any future server reuses the same
//! types. This crate deliberately has no runtime dependencies beyond serde.

use serde::{Deserialize, Serialize};

/// Opaque strand (session file) identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
pub struct StrandId(pub String);

/// Opaque record identifier within a strand.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RecordId(pub String);

/// Opaque ask (interactive question) identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AskId(pub String);

/// Reasoning-effort level for the active model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Effort {
    /// No reasoning.
    Off,
    /// Low effort.
    Low,
    /// Medium effort.
    Medium,
    /// High effort.
    High,
    /// Maximum effort.
    Max,
}

/// Permission mode for the engine.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Confirm exec-tier actions and non-allowlisted writes.
    #[default]
    Guarded,
    /// Auto-apply file edits; commands still confirm.
    AcceptEdits,
    /// Auto-approve everything except hardstops.
    Free,
    /// Research mode: read-only except the plans directory.
    Plan,
}

/// Why a turn finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Stop {
    /// The model produced a final reply.
    Done,
    /// Output hit the model's output limit.
    Length,
    /// The user aborted mid-turn.
    Aborted,
    /// The turn ended in an error.
    Error,
}

/// Token usage and cost for one turn.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Usage {
    /// Input tokens (estimate until the provider's usage arrives).
    pub input: u64,
    /// Output tokens.
    pub output: u64,
    /// Cache-read tokens.
    pub cache_read: u64,
    /// Cache-write tokens.
    pub cache_write: u64,
    /// Cost in USD for the turn.
    pub cost: f64,
}

/// One attached image: base64 payload and its IANA media type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ImagePart {
    /// Base64-encoded image bytes.
    pub data: String,
    /// Media type, e.g. `image/png`.
    pub media_type: String,
}

/// Snapshot of context-window consumption.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
pub struct ContextMeter {
    /// Estimated tokens in context.
    pub used: u64,
    /// Context window of the active model (0 = unknown).
    pub window: u64,
}

/// Engine error classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ErrorClass {
    /// Authentication/authorization failure.
    Auth,
    /// Rate limiting / quota.
    RateLimit,
    /// Context window exceeded.
    Overflow,
    /// Transport-level failure.
    Network,
    /// Malformed provider traffic.
    Protocol,
    /// Command not wired in this build/phase.
    Unsupported,
    /// Anything else.
    Internal,
}

/// One streaming delta from the engine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DeltaKind {
    /// Visible assistant text.
    Text(String),
    /// Reasoning/thought text.
    Thought(String),
    /// A tool call started (arguments stream later, per tool).
    Call {
        /// Tool name.
        tool: String,
        /// Call identifier.
        id: String,
    },
}

/// An interactive question posed to the user mid-turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AskQuestion {
    /// The question text.
    pub text: String,
    /// Selectable answers (index-referenced in [`Command::Answer`]).
    pub options: Vec<String>,
    /// Optional rendered detail (e.g. a unified diff) shown above the
    /// options. Additive: absent in older strands, never required.
    #[serde(default)]
    pub detail: Option<String>,
}

/// Surface → engine commands.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    /// Start a turn with a user prompt.
    Prompt {
        /// Prompt text.
        text: String,
        /// Structured-output JSON schema the reply must satisfy
        /// (None = free-form text).
        #[serde(default)]
        schema: Option<serde_json::Value>,
        /// Images attached to the prompt (base64 data + media type).
        #[serde(default)]
        images: Vec<ImagePart>,
    },
    /// Re-run tools/list on every MCP server (watchdog-adjacent).
    RefreshMcp,
    /// Fetch an MCP prompt and start a turn with its rendered text.
    CallPrompt {
        /// Owning server name.
        server: String,
        /// Prompt name on the server.
        name: String,
        /// Prompt arguments.
        #[serde(default)]
        args: std::collections::HashMap<String, String>,
    },
    /// Deliver user input mid-turn (steering), between tool batches.
    Interject {
        /// Interjection text.
        text: String,
    },
    /// Queue user input for after the current turn settles.
    Defer {
        /// Deferred text.
        text: String,
    },
    /// Run a user-typed shell command directly (the TUI's `!`
    /// passthrough). Output shows in the transcript and rides the next
    /// prompt as context. No permission gate — the user typed it.
    ///
    /// TRUST INVARIANT: only the local TUI may send this. Remote
    /// surfaces (serve, ACP) construct a fixed command subset and must
    /// never forward arbitrary protocol commands, or this becomes an
    /// ungated exec primitive for whoever can reach the transport.
    Shell {
        /// Command line for `sh -c`.
        command: String,
    },
    /// Abort the current turn (partial work is kept).
    Abort,
    /// Snapshot background tasks/jobs for the /tasks dashboard.
    ListTasks,
    /// Switch the active model.
    SetModel {
        /// Model selector, e.g. `vendor/model:effort`.
        selector: String,
    },
    /// Set the reasoning effort.
    SetEffort {
        /// Effort level.
        level: Effort,
    },
    /// Switch permission mode.
    SetMode {
        /// New mode.
        mode: Mode,
    },
    /// Trigger a digest (manual compaction).
    Compact {
        /// Optional focus instructions for the summary.
        focus: Option<String>,
    },
    /// Rewind the conversation to before the Nth-last user message.
    Rewind {
        /// How many user turns back (1 = erase the last exchange).
        turns: u32,
    },
    /// Copy the current strand into a NEW strand file truncated to drop
    /// the last `turns` user turns (`0` = exact copy) and switch to it.
    ForkStrand {
        /// How many trailing user turns the fork drops.
        turns: u32,
    },
    /// Snapshot the working tree (git-based, non-destructive: never
    /// touches the index, stash, or HEAD).
    Checkpoint,
    /// Restore a checkpoint made with [`Command::Checkpoint`]. The id
    /// `"list"` asks for a note listing known ids instead.
    RestoreCheckpoint {
        /// Checkpoint id, or `list`.
        id: String,
    },
    /// Switch the engine to another strand ("new" = fresh session, else a
    /// strand id / id prefix / existing file path).
    SwitchStrand {
        /// Target session reference.
        id: String,
    },
    /// Restore the latest pre-mutation snapshot of this session.
    UndoFile,
    /// Save an API key to the user env layer (~/.config/ka/.env) and the
    /// live process (used by the TUI key prompt).
    SaveApiKey {
        /// Env var name, e.g. `ZHIPU_API_KEY`.
        env_var: String,
        /// The key value (never echoed to the transcript).
        value: String,
    },
    /// Export this session to markdown. `out` overrides the default
    /// `ka-session-<tail>.md` file in the working directory.
    ExportMarkdown {
        /// Output file path (None = default name in cwd).
        out: Option<std::path::PathBuf>,
        /// Render a self-contained HTML page instead of markdown.
        #[serde(default)]
        html: bool,
    },
    /// Persist settings to the user config layer (~/.config/ka/ka.toml).
    SaveSettings {
        /// Default model selector.
        model: Option<String>,
        /// Default reasoning effort.
        effort: Option<Effort>,
        /// Default permission mode.
        mode: Option<Mode>,
    },
    /// Ask for a context-usage breakdown (see [`Event::ContextBreakdown`]).
    ContextBreakdown,
    /// Answer an outstanding ask.
    Answer {
        /// Which ask is being answered.
        question: AskId,
        /// Chosen option index.
        choice: usize,
    },
}

/// One replayed tool call: the header the live turn showed plus its
/// merged result note (filled in from the following tool message).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ReplayedCall {
    /// Engine call id.
    pub id: String,
    /// Tool name.
    pub tool: String,
    /// Argument summary (the CallStarted detail).
    pub detail: String,
    /// First-line result excerpt once the tool message landed.
    pub result: Option<String>,
    /// Whether the tool reported an error.
    pub is_error: bool,
}

/// A replayed historical message (emitted on resume so surfaces can
/// reconstruct the transcript).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ReplayedMessage {
    /// `user`, `assistant`, or `digest` (a compaction divider).
    pub role: String,
    /// Message text.
    pub content: String,
    /// Whether this row is a digest divider (additive).
    #[serde(default)]
    pub digest: bool,
    /// Assistant reasoning captured with the message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    /// Tool calls issued with this assistant message; their results
    /// merge in from the following tool-role messages.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub calls: Vec<ReplayedCall>,
}

/// Per-MCP-server line of the bootstrap [`Event::Inventory`] card.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct McpSummary {
    /// Configured server name.
    pub name: String,
    /// Whether spawn + handshake + tool listing succeeded.
    pub ok: bool,
    /// Tools the server advertises (0 when `ok` is false).
    pub tools: usize,
}

/// One entry of the model-maintained todo list ([`Event::Todos`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TodoItem {
    /// What to do.
    pub text: String,
    /// Whether it is finished.
    pub state: TodoState,
}

/// Completion state of a [`TodoItem`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TodoState {
    /// Still to do.
    Pending,
    /// Finished.
    Done,
}

/// One component of the context breakdown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ContextPart {
    /// Component name (`system`, `user`, `assistant`, `tools`).
    pub name: String,
    /// Estimated tokens in this component.
    pub tokens: u64,
}

/// Engine → surface events.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// Resumed history replay (emitted at startup and on session switch;
    /// surfaces rebuild the transcript from it).
    Replay {
        /// Prior messages, oldest first.
        messages: Vec<ReplayedMessage>,
    },
    /// Live context meter during a turn (emitted per model step so
    /// surfaces show usage while the turn runs, not only at the end).
    ContextMeter {
        /// Tokens currently in context.
        used: u64,
        /// Window size (0 = unknown).
        window: u64,
    },
    /// Reasoning effort changed.
    EffortChanged {
        /// Effort level.
        level: Effort,
    },
    /// The active session (strand) changed or was announced. Surfaces use
    /// it to label the session and the /session picker.
    SessionInfo {
        /// Active strand id.
        id: String,
    },
    /// The strand's display title (stored `Record::Title` or the
    /// auto-generated one). Emitted at bootstrap when the strand already
    /// has a title and after a live auto-title lands, so surfaces can
    /// relabel the session without replay. Additive: absent `title`
    /// deserializes empty (surfaces ignore empty titles).
    Title {
        /// The display title.
        #[serde(default)]
        title: String,
    },
    /// A turn began.
    TurnStarted {
        /// Context consumption snapshot.
        context: ContextMeter,
    },
    /// A streaming delta arrived.
    Delta {
        /// What kind of delta.
        kind: DeltaKind,
    },
    /// A tool call began executing.
    CallStarted {
        /// Tool name.
        tool: String,
        /// Call identifier.
        id: String,
        /// Short argument summary for transcript headers (empty when the
        /// tool has nothing worth showing or the sender predates the
        /// field). Additive: absent JSON deserializes as empty.
        #[serde(default)]
        detail: String,
    },
    /// A tool call finished.
    CallFinished {
        /// Tool name.
        tool: String,
        /// Call identifier.
        id: String,
        /// Whether the call succeeded.
        ok: bool,
    },
    /// A tool call produced output (capped excerpt for surfaces).
    CallOutput {
        /// Tool name.
        tool: String,
        /// Call identifier.
        id: String,
        /// Capped output excerpt (full output may live in a spill file).
        excerpt: String,
        /// Whether the output represents an error result.
        is_error: bool,
        /// Spill pointer when the full output was parked on disk.
        spill: Option<String>,
    },
    /// The engine needs user input.
    Ask {
        /// Ask identifier.
        id: AskId,
        /// Questions to present.
        questions: Vec<AskQuestion>,
    },
    /// A turn finished.
    TurnFinished {
        /// Why it finished.
        stop: Stop,
        /// Usage accounting for the turn.
        usage: Usage,
    },
    /// A digest (compaction) started.
    DigestStarted,
    /// A digest finished; kept history starts at `kept`.
    DigestFinished {
        /// First kept record.
        kept: RecordId,
    },
    /// Permission mode changed.
    ModeChanged {
        /// New mode.
        mode: Mode,
    },
    /// Active model changed.
    ModelChanged {
        /// Selector that was applied.
        selector: String,
    },
    /// The full prompt cycle (turn + settling) is complete; the engine is
    /// idle. Long-lived surfaces keep running; one-shot surfaces may exit.
    Idle,
    /// Session bootstrap inventory: what this conversation can use.
    Inventory {
        /// Built-in + MCP tool names as the model sees them.
        tools: Vec<String>,
        /// Per configured MCP server: name, connect ok, tool count.
        mcp: Vec<McpSummary>,
        /// Discovered subagent names.
        agents: Vec<String>,
        /// Discovered skill names.
        skills: Vec<String>,
        /// Advertised MCP prompts as `server/name (args)` (additive).
        #[serde(default)]
        prompts: Vec<String>,
    },
    /// The model's live todo list (the `todo` hand). Whole-list
    /// replacement: each event supersedes the previous one.
    Todos {
        /// Current items, in display order.
        items: Vec<TodoItem>,
    },
    /// Informational note (pruning/digest notices, etc.).
    Note {
        /// Note text.
        message: String,
    },
    /// Output of a user `!` shell passthrough (redacted, capped).
    ShellOutput {
        /// The command line that ran.
        command: String,
        /// Capped combined stdout+stderr (may be empty).
        output: String,
        /// Failure note (spawn error / non-zero exit / timeout), when
        /// there is one.
        #[serde(default)]
        note: Option<String>,
    },
    /// /tasks dashboard snapshot: background delegate tasks and bash
    /// jobs, pre-rendered rows.
    Tasks {
        /// Rendered rows.
        rows: Vec<String>,
    },
    /// Context-usage breakdown (/context): estimated tokens per
    /// component plus the active window.
    ContextBreakdown {
        /// Estimated tokens per component.
        parts: Vec<ContextPart>,
        /// Context window of the active model (0 = unknown).
        window: u64,
    },
    /// Engine-level error report.
    Error {
        /// Error classification.
        class: ErrorClass,
        /// Whether retrying could help.
        retryable: bool,
        /// Human-readable detail.
        message: String,
    },
}

/// Serialize a value as one NDJSON line (trailing newline included).
pub fn to_line<T: Serialize>(value: &T) -> Result<String, serde_json::Error> {
    let mut line = serde_json::to_string(value)?;
    line.push('\n');
    Ok(line)
}

/// Parse one NDJSON line into `T`.
pub fn from_line<T: for<'de> Deserialize<'de>>(line: &str) -> Result<T, serde_json::Error> {
    serde_json::from_str(line.trim_end())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn roundtrip_command(cmd: Command) {
        let line = to_line(&cmd).unwrap();
        assert!(line.ends_with('\n'));
        let back: Command = from_line(&line).unwrap();
        assert_eq!(
            serde_json::to_string(&cmd).unwrap(),
            serde_json::to_string(&back).unwrap()
        );
    }

    fn roundtrip_event(evt: Event) {
        let line = to_line(&evt).unwrap();
        let back: Event = from_line(&line).unwrap();
        assert_eq!(
            serde_json::to_string(&evt).unwrap(),
            serde_json::to_string(&back).unwrap()
        );
    }

    #[test]
    fn ids_serialize_as_bare_strings() {
        let line = to_line(&Event::DigestFinished {
            kept: RecordId("r7".into()),
        })
        .unwrap();
        assert!(line.contains("\"kept\":\"r7\""), "got: {line}");
    }

    #[test]
    fn commands_roundtrip() {
        roundtrip_command(Command::Prompt {
            text: "hi".into(),
            schema: None,
            images: Vec::new(),
        });
        roundtrip_command(Command::Interject {
            text: "use tabs".into(),
        });
        roundtrip_command(Command::Defer {
            text: "then run tests".into(),
        });
        roundtrip_command(Command::Abort);
        roundtrip_command(Command::SetModel {
            selector: "anthropic/claude-sonnet-5:high".into(),
        });
        roundtrip_command(Command::SetEffort { level: Effort::Max });
        roundtrip_command(Command::SetMode { mode: Mode::Free });
        roundtrip_command(Command::Compact {
            focus: Some("keep API notes".into()),
        });
        roundtrip_command(Command::Answer {
            question: AskId("q1".into()),
            choice: 0,
        });
        roundtrip_command(Command::ExportMarkdown {
            out: Some(std::path::PathBuf::from("x.md")),
            html: false,
        });
        roundtrip_command(Command::ExportMarkdown {
            out: None,
            html: false,
        });
        roundtrip_command(Command::ExportMarkdown {
            out: None,
            html: true,
        });
    }

    #[test]
    fn mode_accept_edits_wire_name_and_roundtrip() {
        let cmd = Command::SetMode {
            mode: Mode::AcceptEdits,
        };
        let line = to_line(&cmd).unwrap();
        assert!(line.contains("\"accept_edits\""), "wire name: {line}");
        let back: Command = from_line(&line).unwrap();
        assert_eq!(
            serde_json::to_string(&back).unwrap(),
            serde_json::to_string(&cmd).unwrap()
        );
        let evt = Event::ModeChanged {
            mode: Mode::AcceptEdits,
        };
        let line = to_line(&evt).unwrap();
        let back: Event = from_line(&line).unwrap();
        assert_eq!(
            serde_json::to_string(&back).unwrap(),
            serde_json::to_string(&evt).unwrap()
        );
    }

    #[test]
    fn events_roundtrip() {
        roundtrip_event(Event::Replay {
            messages: vec![ReplayedMessage {
                role: "user".into(),
                content: "before the crash".into(),
                digest: false,
                thinking: None,
                calls: Vec::new(),
            }],
        });
        roundtrip_event(Event::Replay {
            messages: vec![ReplayedMessage {
                role: "assistant".into(),
                content: String::new(),
                digest: false,
                thinking: Some("weighing options".into()),
                calls: vec![ReplayedCall {
                    id: "c1".into(),
                    tool: "read".into(),
                    detail: "lib.rs".into(),
                    result: Some("fn main() {}".into()),
                    is_error: false,
                }],
            }],
        });
        roundtrip_event(Event::TurnStarted {
            context: ContextMeter {
                used: 12,
                window: 200_000,
            },
        });
        roundtrip_event(Event::Delta {
            kind: DeltaKind::Text("hello".into()),
        });
        roundtrip_event(Event::Delta {
            kind: DeltaKind::Thought("thinking...".into()),
        });
        roundtrip_event(Event::Delta {
            kind: DeltaKind::Call {
                tool: "read".into(),
                id: "c1".into(),
            },
        });
        roundtrip_event(Event::CallStarted {
            tool: "bash".into(),
            id: "c2".into(),
            detail: "cargo test --workspace".into(),
        });
        roundtrip_event(Event::CallFinished {
            tool: "bash".into(),
            id: "c2".into(),
            ok: true,
        });
        roundtrip_event(Event::Ask {
            id: AskId("q1".into()),
            questions: vec![AskQuestion {
                text: "Proceed?".into(),
                options: vec!["yes".into(), "no".into()],
                detail: Some("--- a/f.rs\n+++ b/f.rs\n@@\n".into()),
            }],
        });
        roundtrip_event(Event::ContextBreakdown {
            parts: vec![ContextPart {
                name: "system".into(),
                tokens: 950,
            }],
            window: 128_000,
        });
        roundtrip_event(Event::TurnFinished {
            stop: Stop::Done,
            usage: Usage {
                input: 100,
                output: 20,
                cache_read: 0,
                cache_write: 0,
                cost: 0.001,
            },
        });
        roundtrip_event(Event::Title {
            title: "fix the parser".into(),
        });
        roundtrip_event(Event::DigestStarted);
        roundtrip_event(Event::DigestFinished {
            kept: RecordId("r4".into()),
        });
        roundtrip_event(Event::ModeChanged {
            mode: Mode::Guarded,
        });
        roundtrip_event(Event::ModelChanged {
            selector: "openai/gpt-5.1".into(),
        });
        roundtrip_event(Event::Note {
            message: "pruned ~2k tokens".into(),
        });
        roundtrip_command(Command::Shell {
            command: "cargo test".into(),
        });
        roundtrip_command(Command::ListTasks);
        roundtrip_event(Event::ShellOutput {
            command: "cargo test".into(),
            output: "test result: ok".into(),
            note: Some("exit 101".into()),
        });
        roundtrip_event(Event::Tasks {
            rows: vec!["t-1  running  12s  reviewer — audit".into()],
        });
        roundtrip_event(Event::Error {
            class: ErrorClass::Unsupported,
            retryable: false,
            message: "not wired in phase 0".into(),
        });
        roundtrip_event(Event::Inventory {
            tools: vec!["read".into(), "demo.fetch".into()],
            mcp: vec![
                McpSummary {
                    name: "demo".into(),
                    ok: true,
                    tools: 2,
                },
                McpSummary {
                    name: "jira".into(),
                    ok: false,
                    tools: 0,
                },
            ],
            agents: vec!["coder".into()],
            skills: vec!["rust-docs".into()],
            prompts: Vec::new(),
        });
        roundtrip_event(Event::Todos {
            items: vec![
                TodoItem {
                    text: "scaffold the parser".into(),
                    state: TodoState::Done,
                },
                TodoItem {
                    text: "wire the sidebar".into(),
                    state: TodoState::Pending,
                },
            ],
        });
        roundtrip_event(Event::Todos { items: Vec::new() });
        roundtrip_event(Event::Inventory {
            tools: vec!["read".into()],
            mcp: Vec::new(),
            agents: Vec::new(),
            skills: Vec::new(),
            prompts: Vec::new(),
        });
    }

    #[test]
    fn call_started_detail_defaults_when_absent() {
        // pre-detail senders: the additive field must deserialize empty
        let back: Event = from_line(r#"{"type":"call_started","tool":"bash","id":"c9"}"#).unwrap();
        match back {
            Event::CallStarted { tool, id, detail } => {
                assert_eq!((tool.as_str(), id.as_str()), ("bash", "c9"));
                assert!(detail.is_empty(), "absent detail must default empty");
            }
            other => panic!("wrong event: {other:?}"),
        }
    }
    #[test]
    fn title_defaults_when_absent_and_tags_snake_case() {
        let line = to_line(&Event::Title {
            title: "fix the parser".into(),
        })
        .unwrap();
        assert!(line.contains("\"type\":\"title\""), "got: {line}");
        // pre-title senders: the additive field must deserialize empty
        let back: Event = from_line(r#"{"type":"title"}"#).unwrap();
        match back {
            Event::Title { title } => assert!(title.is_empty()),
            other => panic!("expected title, got {other:?}"),
        }
    }

    #[test]
    fn todos_wire_shape_is_snake_case() {
        let line = to_line(&Event::Todos {
            items: vec![TodoItem {
                text: "ship it".into(),
                state: TodoState::Done,
            }],
        })
        .unwrap();
        assert!(line.contains("\"type\":\"todos\""), "got: {line}");
        assert!(line.contains("\"state\":\"done\""), "got: {line}");
        let back: Event = from_line(&line).unwrap();
        match back {
            Event::Todos { items } => {
                assert_eq!(items.len(), 1);
                assert_eq!(items[0].text, "ship it");
                assert_eq!(items[0].state, TodoState::Done);
            }
            other => panic!("expected todos, got {other:?}"),
        }
    }

    #[test]
    fn todos_decode_from_wire_line() {
        let line = concat!(
            r#"{"type":"todos","items":[{"text":"a","state":"pending"},"#,
            r#"{"text":"b","state":"done"}]}"#,
        );
        let evt: Event = from_line(line).unwrap();
        match evt {
            Event::Todos { items } => assert_eq!(
                items,
                vec![
                    TodoItem {
                        text: "a".into(),
                        state: TodoState::Pending
                    },
                    TodoItem {
                        text: "b".into(),
                        state: TodoState::Done
                    },
                ]
            ),
            other => panic!("expected todos, got {other:?}"),
        }
    }
    #[test]
    fn inventory_decodes_from_wire_line() {
        let line = concat!(
            r#"{"type":"inventory","tools":["read","write","#,
            r#""demo.fetch"],"mcp":[{"name":"demo","ok":true,"tools":2}],"#,
            r#""agents":[],"skills":["demo"]}"#,
        );
        let evt: Event = from_line(line).unwrap();
        match evt {
            Event::Inventory {
                tools,
                mcp,
                agents,
                skills,
                prompts: _,
            } => {
                assert_eq!(
                    tools,
                    vec![
                        "read".to_string(),
                        "write".to_string(),
                        "demo.fetch".to_string()
                    ]
                );
                assert_eq!(
                    mcp,
                    vec![McpSummary {
                        name: "demo".into(),
                        ok: true,
                        tools: 2,
                    }]
                );
                assert!(agents.is_empty());
                assert_eq!(skills, vec!["demo".to_string()]);
            }
            other => panic!("expected inventory, got {other:?}"),
        }
    }

    #[test]
    fn inventory_tags_snake_case_on_wire() {
        let line = to_line(&Event::Inventory {
            tools: Vec::new(),
            mcp: Vec::new(),
            agents: Vec::new(),
            skills: Vec::new(),
            prompts: Vec::new(),
        })
        .unwrap();
        assert!(line.contains("\"type\":\"inventory\""), "got: {line}");
    }

    #[test]
    fn tagging_is_snake_case() {
        let line = to_line(&Command::Abort).unwrap();
        assert!(line.contains("\"type\":\"abort\""), "got: {line}");
        let line = to_line(&Event::TurnStarted {
            context: ContextMeter { used: 1, window: 2 },
        })
        .unwrap();
        assert!(line.contains("\"type\":\"turn_started\""), "got: {line}");
    }
}
