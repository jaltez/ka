//! Dialect catalog: per-model wire profiles as data (`dialects.toml`), plus
//! model-selector parsing. Strictness rule: every table rejects unknown
//! keys, so typos in user overlays fail with the offending key name.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The embedded seed catalog.
pub const EMBEDDED: &str = include_str!("../dialects.toml");

/// Which wire protocol a model speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Wire {
    /// Anthropic Messages API.
    AnthropicMessages,
    /// OpenAI Chat Completions (and every compatible endpoint).
    OpenaiChat,
}

/// Local-endpoint discovery probe for a dialect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Discovery {
    /// Ollama native API probe.
    Ollama,
    /// LM Studio probe.
    LmStudio,
    /// vLLM probe.
    Vllm,
}

/// Input modality.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Modality {
    /// Text.
    Text,
    /// Images.
    Image,
}

/// How the wire supports prompt caching.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cache {
    /// No caching support known.
    #[default]
    Off,
    /// Anthropic-style `cache_control` breakpoints.
    Control,
    /// Provider-side key/implicit caching.
    Key,
}

/// Tool-choice downgrade behavior for models with weak native support.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoice {
    /// Full native tool choice.
    #[default]
    Auto,
    /// Only a pinned tool or plain auto (local-server quirk).
    PinOrAuto,
}

/// Per-mtok pricing (placeholder seed values; verify before release).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Price {
    /// USD per million input tokens.
    #[serde(default)]
    pub input_per_mtok: f64,
    /// USD per million output tokens.
    #[serde(default)]
    pub output_per_mtok: f64,
}

/// Per-model behavioral flags — the growing dialect system.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Flags {
    /// Wire field carrying reasoning config (empty = not supported).
    #[serde(default)]
    pub reasoning_field: Option<String>,
    /// Whether a developer/system role separate from user is supported.
    #[serde(default)]
    pub developer_role: bool,
    /// Whether tool results must re-state the tool name.
    #[serde(default)]
    pub requires_tool_result_name: bool,
    /// Re-emit reasoning content byte-exact for KV-cache prefix reuse.
    #[serde(default)]
    pub replay_reasoning: bool,
    /// Tool-choice downgrade mode.
    #[serde(default)]
    pub tool_choice: ToolChoice,
    /// Request field for output-token cap (None = `max_tokens`).
    #[serde(default)]
    pub max_tokens_field: Option<String>,
}

/// One model's wire profile.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Dialect {
    /// Wire protocol.
    pub wire: Wire,
    /// Endpoint base URL (required for speaking; catalog seeds carry it).
    #[serde(default)]
    pub base_url: Option<String>,
    /// Environment variable holding the API token (None = keyless local).
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Model id override sent on the wire (None = selector model part).
    #[serde(default)]
    pub wire_model: Option<String>,
    /// Local discovery probe, if any.
    #[serde(default)]
    pub discovery: Option<Discovery>,
    /// Context window (tokens; 0 = unknown, discovered).
    #[serde(default)]
    pub context: u32,
    /// Max output tokens.
    #[serde(default = "default_max_output")]
    pub max_output: u32,
    /// Supported effort levels (empty = not controllable).
    #[serde(default)]
    pub efforts: Vec<String>,
    /// Input modalities.
    #[serde(default)]
    pub input: Vec<Modality>,
    /// Prompt-caching mode.
    #[serde(default)]
    pub cache: Cache,
    /// Chars-per-token estimate for meters and digest triggers.
    #[serde(default = "default_ratio")]
    pub ratio: f32,
    /// First-byte timeout in ms (0 = unbounded, for local prefill).
    #[serde(default = "default_first_byte_timeout_ms")]
    pub first_byte_timeout_ms: u64,
    /// Effort → thinking budget (tokens), for wires that take budgets.
    #[serde(default)]
    pub effort_budgets: BTreeMap<String, u32>,
    /// Pricing.
    #[serde(default)]
    pub price: Price,
    /// Whether the pricing is vendor-verified (false = placeholders —
    /// surfaces must not display costs from them).
    #[serde(default)]
    pub priced: bool,
    /// Behavioral flags.
    #[serde(default)]
    pub flags: Flags,
}

fn default_max_output() -> u32 {
    8_192
}
fn default_ratio() -> f32 {
    4.0
}
fn default_first_byte_timeout_ms() -> u64 {
    120_000
}

/// The full catalog: model id (`vendor/model`) → dialect.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Catalog {
    /// All known dialects.
    pub dialects: BTreeMap<String, Dialect>,
}

impl Catalog {
    /// Parse a catalog from TOML text (strict: unknown keys error).
    pub fn parse(text: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(text)
    }

    /// The embedded seed catalog. Panics only if the compiled-in table is
    /// invalid, which is a build-time guarantee.
    pub fn embedded() -> Self {
        match Self::parse(EMBEDDED) {
            Ok(c) => c,
            Err(e) => panic!("embedded dialect catalog is invalid: {e}"),
        }
    }

    /// Overlay another catalog on top (user wins per key).
    pub fn overlay(&mut self, other: Catalog) {
        for (k, v) in other.dialects {
            self.dialects.insert(k, v);
        }
    }

    /// Look a dialect up by exact `vendor/model` id.
    pub fn get(&self, model_id: &str) -> Option<&Dialect> {
        self.dialects.get(model_id)
    }
}

/// A parsed model selector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selector {
    /// Vendor prefix.
    pub vendor: String,
    /// Model id within the vendor.
    pub model: String,
    /// Optional effort suffix.
    pub effort: Option<String>,
}

impl Selector {
    /// The `vendor/model` id without effort.
    pub fn model_id(&self) -> String {
        format!("{}/{}", self.vendor, self.model)
    }
}

/// Selector parse failures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectorError {
    /// Missing the `vendor/model` slash.
    MissingVendor,
    /// Empty vendor or model component.
    EmptyPart,
}

impl std::fmt::Display for SelectorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SelectorError::MissingVendor => write!(f, "selector needs vendor/model, got no '/'"),
            SelectorError::EmptyPart => write!(f, "selector vendor and model must be non-empty"),
        }
    }
}
impl std::error::Error for SelectorError {}

/// Parse `vendor/model[@effort]`. Effort uses `@` because model ids may
/// contain colons (e.g. Ollama tags like `qwen3:32b`).
pub fn parse_selector(s: &str) -> Result<Selector, SelectorError> {
    let (base, effort) = match s.rsplit_once('@') {
        Some((b, e)) => (b, Some(e.to_string())),
        None => (s, None),
    };
    let Some((vendor, model)) = base.split_once('/') else {
        return Err(SelectorError::MissingVendor);
    };
    if vendor.is_empty() || model.is_empty() {
        return Err(SelectorError::EmptyPart);
    }
    Ok(Selector {
        vendor: vendor.to_string(),
        model: model.to_string(),
        effort,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn seeded_rows_are_unpriced_placeholders() {
        let c = Catalog::embedded();
        for (id, d) in &c.dialects {
            assert!(!d.priced, "{id} seeds placeholder pricing");
        }
    }

    #[test]
    fn embedded_catalog_parses() {
        let c = Catalog::embedded();
        assert!(c.dialects.len() >= 3);
        let d = c.get("anthropic/claude-sonnet-5").unwrap();
        assert_eq!(d.wire, Wire::AnthropicMessages);
        assert_eq!(d.cache, Cache::Control);
        assert_eq!(d.base_url.as_deref(), Some("https://api.anthropic.com"));
        assert_eq!(d.api_key_env.as_deref(), Some("ANTHROPIC_API_KEY"));
        assert_eq!(d.flags.reasoning_field.as_deref(), Some("thinking"));
        assert!(d.flags.developer_role);
        assert_eq!(d.effort_budgets.get("high"), Some(&16_384));
        let o = c.get("openai/gpt-5.1").unwrap();
        assert_eq!(
            o.flags.max_tokens_field.as_deref(),
            Some("max_completion_tokens")
        );
        let q = c.get("ollama/qwen3-32b").unwrap();
        assert_eq!(q.first_byte_timeout_ms, 0);
        assert!(q.flags.replay_reasoning);
        assert_eq!(q.flags.tool_choice, ToolChoice::PinOrAuto);
    }

    #[test]
    fn unknown_keys_rejected() {
        let bad = "[dialects.\"x/y\"]\nwire = \"openai_chat\"\ncontext = 1\nbogus_key = true\n";
        let err = Catalog::parse(bad).unwrap_err().to_string();
        assert!(err.contains("bogus_key"), "got: {err}");
    }

    #[test]
    fn unknown_flag_rejected() {
        let bad = "[dialects.\"x/y\"]\nwire = \"openai_chat\"\ncontext = 1\n[dialects.\"x/y\".flags]\nmystery = 1\n";
        let err = Catalog::parse(bad).unwrap_err().to_string();
        assert!(err.contains("mystery"), "got: {err}");
    }

    #[test]
    fn overlay_user_wins() {
        let mut c = Catalog::embedded();
        let over = Catalog::parse(
            "[dialects.\"anthropic/claude-sonnet-5\"]\nwire = \"anthropic_messages\"\ncontext = 123\n",
        )
        .unwrap();
        c.overlay(over);
        assert_eq!(c.get("anthropic/claude-sonnet-5").unwrap().context, 123);
    }

    #[test]
    fn selectors_parse() {
        let s = parse_selector("openai/gpt-5.1@high").unwrap();
        assert_eq!(s.model_id(), "openai/gpt-5.1");
        assert_eq!(s.effort.as_deref(), Some("high"));
        let tagged = parse_selector("ollama/qwen3:32b").unwrap();
        assert_eq!(tagged.model_id(), "ollama/qwen3:32b");
        assert!(tagged.effort.is_none());
        assert!(parse_selector("nopath").unwrap_err() == SelectorError::MissingVendor);
        assert!(parse_selector("/model").unwrap_err() == SelectorError::EmptyPart);
    }
}
