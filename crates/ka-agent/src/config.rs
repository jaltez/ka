//! Layered strict-TOML configuration.
//!
//! Chain (lowest → highest): built-in defaults → user `~/.config/ka/ka.toml`
//! → project `.ka/ka.toml` → environment (`KA_MODEL`, `KA_MODE`) → CLI flags.
//! Every textual layer is parsed strictly: unknown keys are hard errors that
//! carry the TOML position, so typos never silently pass.

use ka_protocol::{Effort, Mode};
use serde::{Deserialize, Serialize};

/// Role → model-selector mappings.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct Roles {
    /// Selector for the default (main) model.
    pub default: Option<String>,
    /// Selector for the fast (cheap) role.
    pub fast: Option<String>,
}

/// Engine configuration. All fields optional at the data level; resolution
/// order is applied by [`Config::overlay`] consumers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    /// Default model selector (`vendor/model:effort`).
    pub model: Option<String>,
    /// Default reasoning effort.
    pub effort: Option<Effort>,
    /// Permission mode (defaults to guarded when absent everywhere).
    pub mode: Option<Mode>,
    /// Role mappings.
    pub roles: Roles,
    /// Maximum tool-execution steps per prompt before forcing a text reply.
    #[serde(default)]
    pub max_steps: Option<u32>,
    /// Working directory override (default: process cwd).
    #[serde(default)]
    pub cwd: Option<String>,
}

impl Config {
    /// Parse one strict TOML layer. `origin` names the source in errors.
    pub fn parse_layer(text: &str, origin: &str) -> Result<Self, ConfigError> {
        toml::from_str(text).map_err(|e| ConfigError::Parse {
            origin: origin.to_string(),
            message: e.to_string(),
        })
    }

    /// Apply `other` on top of `self`: set fields win, unset fields keep.
    pub fn overlay(&mut self, other: Config) {
        if other.model.is_some() {
            self.model = other.model;
        }
        if other.effort.is_some() {
            self.effort = other.effort;
        }
        if other.mode.is_some() {
            self.mode = other.mode;
        }
        if other.roles.default.is_some() {
            self.roles.default = other.roles.default;
        }
        if other.roles.fast.is_some() {
            self.roles.fast = other.roles.fast;
        }
        if other.max_steps.is_some() {
            self.max_steps = other.max_steps;
        }
        if other.cwd.is_some() {
            self.cwd = other.cwd;
        }
    }

    /// Effective step cap (default 20).
    pub fn effective_max_steps(&self) -> u32 {
        self.max_steps.unwrap_or(20)
    }

    /// The effective permission mode (guarded unless explicitly freed).
    pub fn effective_mode(&self) -> Mode {
        self.mode.unwrap_or_default()
    }

    /// JSON schema for editor integration (`ka config schema`).
    pub fn schema_json() -> Result<String, serde_json::Error> {
        let schema = schemars::schema_for!(Config);
        serde_json::to_string_pretty(&schema)
    }
}

/// Configuration failure.
#[derive(Debug)]
pub enum ConfigError {
    /// A layer failed strict parsing.
    Parse {
        /// Which layer (path or description).
        origin: String,
        /// Parser detail, including TOML position.
        message: String,
    },
    /// A layer file could not be read.
    Io {
        /// Which layer.
        origin: String,
        /// I/O detail.
        message: String,
    },
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Parse { origin, message } => write!(f, "{origin}: {message}"),
            ConfigError::Io { origin, message } => write!(f, "{origin}: {message}"),
        }
    }
}
impl std::error::Error for ConfigError {}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn defaults_are_empty() {
        let c = Config::default();
        assert_eq!(c.effective_mode(), Mode::Guarded);
        assert!(c.model.is_none());
    }

    #[test]
    fn layer_overlay_set_fields_win() {
        let mut base =
            Config::parse_layer("model = \"a/x\"\nmode = \"guarded\"\n", "base").unwrap();
        let over = Config::parse_layer("mode = \"free\"\n", "over").unwrap();
        base.overlay(over);
        assert_eq!(base.model.as_deref(), Some("a/x"));
        assert_eq!(base.effective_mode(), Mode::Free);
    }

    #[test]
    fn unknown_key_is_hard_error_with_position() {
        let err = Config::parse_layer("modle = \"a/x\"\n", "user")
            .unwrap_err()
            .to_string();
        assert!(err.starts_with("user:"), "got: {err}");
        assert!(
            err.contains("unknown field") || err.contains("modle"),
            "got: {err}"
        );
    }

    #[test]
    fn unknown_nested_key_rejected() {
        let err = Config::parse_layer("[roles]\ndefualt = \"a/x\"\n", "user")
            .unwrap_err()
            .to_string();
        assert!(err.contains("defualt"), "got: {err}");
    }

    #[test]
    fn env_shaped_layer_parses() {
        // what ka-cli builds from KA_MODEL/KA_MODE before overlaying
        let c =
            Config::parse_layer("model = \"openai/gpt-5.1\"\nmode = \"free\"\n", "env").unwrap();
        assert_eq!(c.model.as_deref(), Some("openai/gpt-5.1"));
        assert_eq!(c.effective_mode(), Mode::Free);
    }

    #[test]
    fn schema_emits() {
        let schema = Config::schema_json().unwrap();
        assert!(schema.contains("\"Config\""), "got: {schema}");
    }
}
