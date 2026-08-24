//! ka dialect crate: per-model wire profiles as data, Speaker
//! implementations for the Phase-1 wires (openai-chat,
//! anthropic-messages), local-endpoint discovery, and token plumbing.

pub mod auth;
pub mod client;
pub mod dialects;
pub mod discovery;
pub mod json_repair;
pub mod speaker;
mod sse;
pub mod wire_anthropic;
pub mod wire_openai;

pub use dialects::{Catalog, Dialect, Discovery, Selector, SelectorError, Wire, parse_selector};
pub use discovery::{FoundModel, discover_lmstudio, discover_ollama, discover_openai_compatible};
pub use speaker::{
    SpeakFuture, SpeakRequest, Speaker, StreamEvent, ToolCall, TurnMessage, TurnRole,
};
pub use wire_anthropic::AnthropicMessages;
pub use wire_openai::OpenaiChat;

/// Build the right Speaker for a dialect's wire.
pub fn speaker_for(wire: dialects::Wire) -> std::sync::Arc<dyn Speaker> {
    match wire {
        dialects::Wire::OpenaiChat => std::sync::Arc::new(OpenaiChat::new()),
        dialects::Wire::AnthropicMessages => std::sync::Arc::new(AnthropicMessages::new()),
    }
}
