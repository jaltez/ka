//! The Speaker trait: what every wire must implement. One interface, two
//! Phase-1 implementations (openai-chat, anthropic-messages). A Speaker
//! streams normalized [`StreamEvent`]s into a channel — failures arrive as
//! `Failed` events, not as errors, so the engine treats both alike.

use std::future::Future;
use std::pin::Pin;

use ka_protocol::{ErrorClass, Stop, Usage};
use serde_json::Value;
use tokio::sync::mpsc;

use crate::dialects::Dialect;

/// Conversation message in ka's neutral shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnMessage {
    /// Who spoke.
    pub role: TurnRole,
    /// What they said (plain text; tool blocks arrive in Phase 2).
    pub content: String,
}

/// Neutral roles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnRole {
    /// Human input.
    User,
    /// Model output.
    Assistant,
}

/// A complete, parsed tool call (arguments accumulated dialect-side).
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    /// Provider call id.
    pub id: String,
    /// Tool name.
    pub tool: String,
    /// Parsed arguments (repaired if the stream truncated).
    pub arguments: Value,
}

/// Everything a Speaker needs for one request.
#[derive(Debug, Clone)]
pub struct SpeakRequest {
    /// `vendor/model` id.
    pub model_id: String,
    /// Resolved dialect (wire + flags + budgets).
    pub dialect: Dialect,
    /// Optional effort level.
    pub effort: Option<String>,
    /// System prompt (placement is wire-specific).
    pub system: String,
    /// Conversation so far (user/assistant).
    pub messages: Vec<TurnMessage>,
    /// Bearer/API token, if the endpoint needs one.
    pub token: Option<String>,
    /// Cache key for providers that take one.
    pub cache_key: Option<String>,
}

/// Normalized streaming events emitted by any wire.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    /// Visible text delta.
    Text(String),
    /// Reasoning/thought delta.
    Thought(String),
    /// A tool call was fully received (arguments parsed).
    Call(ToolCall),
    /// The stream finished.
    Finished {
        /// Normalized stop reason.
        stop: Stop,
        /// Token usage (best known at finish; cost computed engine-side).
        usage: Usage,
    },
    /// The wire failed before finishing.
    Failed {
        /// Classification.
        class: ErrorClass,
        /// Whether a retry could help.
        retryable: bool,
        /// Human-readable detail.
        message: String,
    },
}

/// A boxed future as returned by [`Speaker::speak`].
pub type SpeakFuture<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

/// What every wire implements. The channel-based shape keeps streaming
/// live, cancellation natural (drop the receiver), and the trait dyn-safe.
pub trait Speaker: Send + Sync {
    /// Speak one request, streaming events into `out` until `Finished` or
    /// `Failed` (always exactly one of those last).
    fn speak<'a>(&'a self, req: SpeakRequest, out: mpsc::Sender<StreamEvent>) -> SpeakFuture<'a>;
}

/// Convert a [`WireError`](crate::client::WireError) into a `Failed` event.
pub fn speak_failed(e: crate::client::WireError) -> StreamEvent {
    StreamEvent::Failed {
        class: e.class,
        retryable: e.retryable,
        message: e.message,
    }
}
