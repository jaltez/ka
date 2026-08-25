//! Converser: the engine-facing factory that resolves a selector to a
//! ready-to-speak [`Speaker`] plus the dialect data (window, pricing) the
//! engine needs for meters and cost.

use crate::speaker::Speaker;
use crate::{
    AnthropicMessages, Catalog, Dialect, EnvLookup, OpenaiChat, RetryPolicy, Wire, parse_selector,
};

/// Factory failures (no wire traffic involved).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConverserError {
    /// Selector syntax error.
    BadSelector(String),
    /// No such dialect in the catalog.
    UnknownModel(String),
    /// `auth_env` configured but the variable resolves to nothing.
    NoAuth { model: String, env_var: String },
}

impl std::fmt::Display for ConverserError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConverserError::BadSelector(s) => write!(f, "bad selector: {s}"),
            ConverserError::UnknownModel(m) => write!(f, "unknown model {m:?} (check `ka models`)"),
            ConverserError::NoAuth { model, env_var } => write!(
                f,
                "no API key for {model:?}: set {env_var} (or an .env file with it)"
            ),
        }
    }
}
impl std::error::Error for ConverserError {}

/// Builds Speakers from a catalog + env ladder.
#[derive(Debug, Clone)]
pub struct Converser {
    catalog: Catalog,
    env: EnvLookup,
    policy: RetryPolicy,
}

impl Converser {
    /// New converser over a catalog and env lookup.
    pub fn new(catalog: Catalog, env: EnvLookup) -> Self {
        Self {
            catalog,
            env,
            policy: RetryPolicy::new(),
        }
    }

    /// The catalog in use.
    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    /// Parse a selector string.
    pub fn selector(&self, s: &str) -> Result<crate::Selector, ConverserError> {
        parse_selector(s).map_err(|e| ConverserError::BadSelector(e.to_string()))
    }

    /// Resolve a selector to its dialect row, falling back to a synthetic
    /// row from the provider registry when the catalog has no seed.
    pub fn dialect_for(&self, selector: &str) -> Result<Dialect, ConverserError> {
        let sel = self.selector(selector)?;
        self.catalog
            .get(&sel.model_id())
            .cloned()
            .or_else(|| {
                crate::providers::find(&sel.vendor)
                    .map(|p| crate::providers::synthetic_dialect(p, &sel.model))
            })
            .ok_or_else(|| ConverserError::UnknownModel(sel.model_id()))
    }

    pub fn speaker_for(&self, selector: &str) -> Result<Box<dyn Speaker>, ConverserError> {
        let sel = self.selector(selector)?;
        let id = sel.model_id();
        let dialect = self
            .catalog
            .get(&id)
            .cloned()
            .or_else(|| {
                crate::providers::find(&sel.vendor)
                    .map(|p| crate::providers::synthetic_dialect(p, &sel.model))
            })
            .ok_or_else(|| ConverserError::UnknownModel(id.clone()))?;
        // the model id the wire sends is everything after the vendor prefix
        let wire_model = sel.model.clone();
        let max_output = dialect.max_output;
        let first_byte = dialect.first_byte_timeout_ms;
        match dialect.wire {
            Wire::AnthropicMessages => {
                let api_key = self.resolve_key(&dialect, &id)?;
                Ok(Box::new(AnthropicMessages {
                    client: reqwest::Client::new(),
                    base_url: dialect
                        .base_url
                        .unwrap_or_else(|| "https://api.anthropic.com".to_string()),
                    api_key: api_key.unwrap_or_default(),
                    model: wire_model,
                    max_output,
                    first_byte_timeout_ms: first_byte,
                    policy: self.policy,
                }))
            }
            Wire::OpenaiChat => {
                let api_key = self.resolve_key(&dialect, &id)?;
                Ok(Box::new(OpenaiChat {
                    client: reqwest::Client::new(),
                    base_url: dialect
                        .base_url
                        .unwrap_or_else(|| "https://api.openai.com/v1".to_string()),
                    api_key,
                    model: wire_model,
                    max_output,
                    supported_efforts: dialect.efforts.clone(),
                    first_byte_timeout_ms: first_byte,
                    policy: self.policy,
                }))
            }
        }
    }

    /// Cost in USD for a turn's usage against a dialect's pricing
    /// (associated form usable without a Converser instance).
    pub fn cost_standalone(dialect: &Dialect, usage: crate::speaker::SpeakerUsage) -> f64 {
        let ip = dialect.price.input_per_mtok / 1_000_000.0;
        let op = dialect.price.output_per_mtok / 1_000_000.0;
        usage.input as f64 * ip
            + usage.cache_read as f64 * ip * 0.1
            + usage.cache_write as f64 * ip * 1.25
            + usage.output as f64 * op
    }

    /// Cost in USD for a turn's usage against a dialect's pricing.
    /// Cached input is billed at 0.1×, cache writes at 1.25× (Anthropic-style
    /// ratios; refinement deferred until real invoices say otherwise).
    pub fn cost(&self, dialect: &Dialect, usage: crate::speaker::SpeakerUsage) -> f64 {
        Self::cost_standalone(dialect, usage)
    }

    fn resolve_key(
        &self,
        dialect: &Dialect,
        model_id: &str,
    ) -> Result<Option<String>, ConverserError> {
        match &dialect.auth_env {
            None => Ok(None),
            Some(var) => match self.env.get(var) {
                Some(k) => Ok(Some(k)),
                None => Err(ConverserError::NoAuth {
                    model: model_id.to_string(),
                    env_var: var.clone(),
                }),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn converser() -> Converser {
        Converser::new(Catalog::embedded(), EnvLookup::empty())
    }

    #[test]
    fn unseeded_provider_model_resolves_via_registry() {
        let d = converser().dialect_for("groq/llama-3.3-70b-versatile").unwrap();
        assert_eq!(d.base_url.as_deref(), Some("https://api.groq.com/openai/v1"));
        assert_eq!(d.api_key_env.as_deref(), Some("GROQ_API_KEY"));
    }

    #[test]
    fn unknown_vendor_still_errors() {
        assert!(converser().dialect_for("nosuch/model-x").is_err());
    }

    #[test]
    fn seeded_row_wins_over_registry() {
        let d = converser().dialect_for("ollama/qwen3-32b").unwrap();
        assert_eq!(d.context, 131_072);
    }

    #[test]
    fn selector_must_be_known() {
        assert!(matches!(
            converser().speaker_for("nope/zzz"),
            Err(ConverserError::UnknownModel(_))
        ));
        assert!(matches!(
            converser().speaker_for("bad"),
            Err(ConverserError::BadSelector(_))
        ));
    }

    #[test]
    fn auth_required_for_keyed_dialects() {
        match converser().speaker_for("anthropic/claude-sonnet-5") {
            Err(e @ ConverserError::NoAuth { .. }) => {
                assert!(e.to_string().contains("ANTHROPIC_API_KEY"));
            }
            other => panic!("expected NoAuth, got ok={}", other.is_ok()),
        }
    }

    #[test]
    fn local_dialects_speak_keyless() {
        let c = converser();
        // the embedded ollama row needs no key: building must not fail on auth
        let d = c.dialect_for("ollama/qwen3-32b").unwrap();
        assert!(d.auth_env.is_none());
    }

    #[test]
    fn cost_math() {
        let c = converser();
        let d = c.dialect_for("anthropic/claude-sonnet-5").unwrap();
        let cost = c.cost(
            &d,
            crate::speaker::SpeakerUsage {
                input: 1_000_000,
                output: 1_000_000,
                cache_read: 1_000_000,
                cache_write: 0,
            },
        );
        assert!((cost - (3.0 + 0.3 + 15.0)).abs() < 1e-9, "got {cost}");
    }
}
