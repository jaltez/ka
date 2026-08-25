//! Provider registry: known vendors with their wire protocol, endpoint,
//! and API-key environment variable. Powers `ka providers`, the settings
//! panel, and synthetic dialects for unseeded models — any
//! `provider/model` selector on a registered provider works without a
//! catalog row.
//!
//! Curation (v1): direct OpenAI-compatible endpoints with static base
//! URLs and plain env keys, Anthropic's native wire, Google's
//! OpenAI-compatible bridge, and local runtimes. Excluded for now:
//! Azure/Bedrock/Vertex (sigv4/Entra machinery), proxies (LiteLLM,
//! Cloudflare), and JWT-only flows.

use crate::dialects::{Dialect, Wire};

/// One known provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provider {
    /// Selector vendor prefix (`groq/llama-…`).
    pub name: &'static str,
    /// Wire protocol spoken at the endpoint.
    pub wire: Wire,
    /// Base URL (OpenAI-compatible path included).
    pub base_url: &'static str,
    /// Env var holding the API key (None = keyless local).
    pub key_env: Option<&'static str>,
    /// One-line note for listings.
    pub note: &'static str,
}

/// The v1 registry.
pub const PROVIDERS: &[Provider] = &[
    // ── first-party ──────────────────────────────────────────────
    Provider {
        name: "openai",
        wire: Wire::OpenaiChat,
        base_url: "https://api.openai.com/v1",
        key_env: Some("OPENAI_API_KEY"),
        note: "GPT models",
    },
    Provider {
        name: "anthropic",
        wire: Wire::AnthropicMessages,
        base_url: "https://api.anthropic.com",
        key_env: Some("ANTHROPIC_API_KEY"),
        note: "Claude models",
    },
    Provider {
        name: "google",
        wire: Wire::OpenaiChat,
        base_url: "https://generativelanguage.googleapis.com/v1beta/openai",
        key_env: Some("GEMINI_API_KEY"),
        note: "Gemini via OpenAI-compatible bridge",
    },
    // ── hosted inference ─────────────────────────────────────────
    Provider {
        name: "mistral",
        wire: Wire::OpenaiChat,
        base_url: "https://api.mistral.ai/v1",
        key_env: Some("MISTRAL_API_KEY"),
        note: "Mistral models",
    },
    Provider {
        name: "groq",
        wire: Wire::OpenaiChat,
        base_url: "https://api.groq.com/openai/v1",
        key_env: Some("GROQ_API_KEY"),
        note: "ultra-fast open-model inference",
    },
    Provider {
        name: "cerebras",
        wire: Wire::OpenaiChat,
        base_url: "https://api.cerebras.ai/v1",
        key_env: Some("CEREBRAS_API_KEY"),
        note: "very fast Llama/Qwen inference",
    },
    Provider {
        name: "deepseek",
        wire: Wire::OpenaiChat,
        base_url: "https://api.deepseek.com",
        key_env: Some("DEEPSEEK_API_KEY"),
        note: "DeepSeek models",
    },
    Provider {
        name: "qwen",
        wire: Wire::OpenaiChat,
        base_url: "https://dashscope.aliyuncs.com/compatible-mode/v1",
        key_env: Some("DASHSCOPE_API_KEY"),
        note: "Alibaba Qwen (DashScope)",
    },
    Provider {
        name: "moonshot",
        wire: Wire::OpenaiChat,
        base_url: "https://api.moonshot.cn/v1",
        key_env: Some("MOONSHOT_API_KEY"),
        note: "Kimi models",
    },
    Provider {
        name: "xai",
        wire: Wire::OpenaiChat,
        base_url: "https://api.x.ai/v1",
        key_env: Some("XAI_API_KEY"),
        note: "Grok models",
    },
    Provider {
        name: "zhipu",
        wire: Wire::OpenaiChat,
        base_url: "https://open.bigmodel.cn/api/paas/v4",
        key_env: Some("ZHIPU_API_KEY"),
        note: "GLM models",
    },
    Provider {
        name: "nvidia",
        wire: Wire::OpenaiChat,
        base_url: "https://integrate.api.nvidia.com/v1",
        key_env: Some("NVIDIA_API_KEY"),
        note: "NVIDIA AI Foundation models",
    },
    // ── aggregators ──────────────────────────────────────────────
    Provider {
        name: "openrouter",
        wire: Wire::OpenaiChat,
        base_url: "https://openrouter.ai/api/v1",
        key_env: Some("OPENROUTER_API_KEY"),
        note: "gateway to 100+ models",
    },
    Provider {
        name: "together",
        wire: Wire::OpenaiChat,
        base_url: "https://api.together.xyz/v1",
        key_env: Some("TOGETHER_API_KEY"),
        note: "hosted open models",
    },
    Provider {
        name: "fireworks",
        wire: Wire::OpenaiChat,
        base_url: "https://api.fireworks.ai/inference/v1",
        key_env: Some("FIREWORKS_API_KEY"),
        note: "fast open-model inference",
    },
    // ── local runtimes ───────────────────────────────────────────
    Provider {
        name: "ollama",
        wire: Wire::OpenaiChat,
        base_url: "http://127.0.0.1:11434/v1",
        key_env: None,
        note: "local runtime",
    },
    Provider {
        name: "lmstudio",
        wire: Wire::OpenaiChat,
        base_url: "http://127.0.0.1:1234/v1",
        key_env: None,
        note: "local server",
    },
    Provider {
        name: "llamacpp",
        wire: Wire::OpenaiChat,
        base_url: "http://127.0.0.1:8080/v1",
        key_env: None,
        note: "llama.cpp server",
    },
    Provider {
        name: "vllm",
        wire: Wire::OpenaiChat,
        base_url: "http://127.0.0.1:8000/v1",
        key_env: None,
        note: "vLLM server (key optional at the endpoint)",
    },
];

/// Look a provider up by vendor prefix.
pub fn find(vendor: &str) -> Option<&'static Provider> {
    PROVIDERS.iter().find(|p| p.name == vendor)
}

/// Synthesize a dialect for `provider/model` when the catalog has no row.
/// Context window unknown (0); conservative defaults elsewhere.
pub fn synthetic_dialect(provider: &Provider, model: &str) -> Dialect {
    Dialect {
        wire: provider.wire,
        base_url: Some(provider.base_url.to_string()),
        api_key_env: provider.key_env.map(str::to_string),
        wire_model: Some(model.to_string()),
        discovery: None,
        context: 0,
        max_output: 8_192,
        efforts: Vec::new(),
        input: Vec::new(),
        cache: Default::default(),
        ratio: 3.6,
        price: Default::default(),
        flags: Default::default(),
        effort_budgets: Default::default(),
        first_byte_timeout_ms: 120_000,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn registry_covers_expected_vendors() {
        for name in [
            "openai",
            "anthropic",
            "google",
            "groq",
            "deepseek",
            "qwen",
            "openrouter",
            "ollama",
        ] {
            assert!(find(name).is_some(), "missing provider {name}");
        }
    }

    #[test]
    fn every_provider_name_is_unique() {
        let mut names: Vec<_> = PROVIDERS.iter().map(|p| p.name).collect();
        names.sort_unstable();
        let n = names.len();
        names.dedup();
        assert_eq!(n, names.len());
    }

    #[test]
    fn synthetic_dialect_carries_endpoint_and_key() {
        let p = find("groq").unwrap();
        let d = synthetic_dialect(p, "llama-3.3-70b-versatile");
        assert_eq!(
            d.base_url.as_deref(),
            Some("https://api.groq.com/openai/v1")
        );
        assert_eq!(d.api_key_env.as_deref(), Some("GROQ_API_KEY"));
        assert_eq!(d.wire, Wire::OpenaiChat);
        assert_eq!(d.wire_model.as_deref(), Some("llama-3.3-70b-versatile"));
    }

    #[test]
    fn keyless_locals_have_no_env() {
        for name in ["ollama", "lmstudio", "llamacpp", "vllm"] {
            assert!(find(name).unwrap().key_env.is_none());
        }
    }
}
