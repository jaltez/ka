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

/// Attachment riding along with a prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Attachment {
    /// Human-readable label for the attachment.
    pub name: String,
    /// What kind of payload this is.
    #[serde(rename = "type")]
    pub kind: AttachmentKind,
}

/// Kind of an [`Attachment`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentKind {
    /// A filesystem path.
    Path,
    /// Inline text.
    Text,
    /// An image path.
    Image,
}

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
    /// Auto-approve everything except hardstops.
    Free,
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
}

/// Surface → engine commands.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Command {
    /// Start a turn with a user prompt.
    Prompt {
        /// Prompt text.
        text: String,
        /// Attachments (paths, inline text, images).
        attachments: Vec<Attachment>,
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
    /// Abort the current turn (partial work is kept).
    Abort,
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
    /// Persist a session-scoped always-allow rule.
    AlwaysAllow {
        /// Rule identifier as presented by the engine.
        rule: String,
    },
    /// Resume a strand, optionally at an offshoot tip.
    Resume {
        /// Strand to resume.
        strand: StrandId,
        /// Tip record to restore (latest if absent).
        tip: Option<RecordId>,
    },
    /// Trigger a digest (manual compaction).
    Compact {
        /// Optional focus instructions for the summary.
        focus: Option<String>,
    },
    /// Answer an outstanding ask.
    Answer {
        /// Which ask is being answered.
        question: AskId,
        /// Chosen option index.
        choice: usize,
    },
}

/// Engine → surface events.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
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
    fn commands_roundtrip() {
        roundtrip_command(Command::Prompt {
            text: "hi".into(),
            attachments: vec![Attachment {
                name: "notes".into(),
                kind: AttachmentKind::Path,
            }],
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
        roundtrip_command(Command::AlwaysAllow {
            rule: "bash:cargo *".into(),
        });
        roundtrip_command(Command::Resume {
            strand: StrandId("abc".into()),
            tip: Some(RecordId("r9".into())),
        });
        roundtrip_command(Command::Compact {
            focus: Some("keep API notes".into()),
        });
        roundtrip_command(Command::Answer {
            question: AskId("q1".into()),
            choice: 0,
        });
    }

    #[test]
    fn events_roundtrip() {
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
            }],
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
        roundtrip_event(Event::Error {
            class: ErrorClass::Unsupported,
            retryable: false,
            message: "not wired in phase 0".into(),
        });
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

    #[test]
    fn ids_serialize_as_bare_strings() {
        let line = to_line(&Command::Resume {
            strand: StrandId("s".into()),
            tip: None,
        })
        .unwrap();
        assert!(line.contains("\"strand\":\"s\""), "got: {line}");
    }
}
