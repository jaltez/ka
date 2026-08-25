//! Layered strict-TOML configuration.
//!
//! Chain (lowest → highest): built-in defaults → user `~/.config/ka/ka.toml`
//! → project `.ka/ka.toml` → environment (`KA_MODEL`, `KA_MODE`) → CLI flags.
//! Every textual layer is parsed strictly: unknown keys are hard errors that
//! carry the TOML position, so typos never silently pass.

use ka_protocol::{Effort, Mode};
use serde::{Deserialize, Serialize};

/// One permission rule: first matching rule wins at gate time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    /// Tool the rule applies to (`bash`, `read`, ...).
    pub tool: String,
    /// Glob pattern matched against the call's primary argument (bash:
    /// command line; file tools: path; search tools: pattern). `None`
    /// matches every call of the tool.
    #[serde(default)]
    pub pattern: Option<String>,
    /// What to do when it matches.
    pub verdict: Verdict,
}

/// One hook: a shell command run around tool calls.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Hook {
    /// When to run.
    pub event: HookEvent,
    /// Only this tool (None = every tool).
    #[serde(default)]
    pub tool: Option<String>,
    /// Shell command. Receives JSON on stdin (tool name + arguments);
    /// exit 2 blocks the call (pre_tool_use) with stderr as the reason.
    pub command: String,
}

/// Hook trigger points.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    /// Before a tool executes; exit 2 blocks it.
    PreToolUse,
    /// After a tool finished; exit 2 marks the result an error.
    PostToolUse,
}

/// Rule verdicts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Run without asking.
    Allow,
    /// Always ask, regardless of mode.
    Ask,
    /// Refuse outright.
    Deny,
}

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
    /// Permission rules, evaluated first-match-wins before mode logic.
    #[serde(default)]
    pub rules: Vec<Rule>,
    /// Tool-call hooks (exit-2 block contract).
    #[serde(default)]
    pub hooks: Vec<Hook>,
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
        if !other.rules.is_empty() {
            self.rules = other.rules;
        }
        if !other.hooks.is_empty() {
            self.hooks = other.hooks;
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

/// The user config layer path (`~/.config/ka/ka.toml`, XDG-aware).
pub fn user_config_path() -> std::path::PathBuf {
    std::env::var("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
        .join("ka/ka.toml")
}

/// Persist default settings to the user layer, preserving unrelated keys
/// (rules, hooks, roles) from an existing file. Returns the path written.
pub fn save_user_settings(
    model: Option<&str>,
    effort: Option<Effort>,
    mode: Option<Mode>,
) -> Result<std::path::PathBuf, String> {
    save_settings_to(&user_config_path(), model, effort, mode)
}

/// [`save_user_settings`] against an explicit path (tests, layers).
pub fn save_settings_to(
    path: &std::path::Path,
    model: Option<&str>,
    effort: Option<Effort>,
    mode: Option<Mode>,
) -> Result<std::path::PathBuf, String> {
    let mut layer = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| Config::parse_layer(&text, "user").ok())
        .unwrap_or_default();
    if model.is_some() {
        layer.model = model.map(str::to_string);
    }
    if effort.is_some() {
        layer.effort = effort;
    }
    if mode.is_some() {
        layer.mode = mode;
    }
    let mut text = String::from("# ka user config — written by /settings\n\n");
    text.push_str(&toml::to_string_pretty(&layer).map_err(|e| e.to_string())?);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
    }
    std::fs::write(path, text).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(path.to_path_buf())
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

    #[test]
    fn save_settings_preserves_unrelated_keys() {
        let dir = std::env::temp_dir().join(format!("ka-cfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ka.toml");
        std::fs::write(&path, "model = \"old/model\"\nmax_steps = 7\n").unwrap();
        save_settings_to(
            &path,
            Some("groq/llama-3.3-70b"),
            Some(Effort::High),
            Some(Mode::Free),
        )
        .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let layer = Config::parse_layer(&text, "saved").unwrap();
        assert_eq!(layer.model.as_deref(), Some("groq/llama-3.3-70b"));
        assert_eq!(layer.effort, Some(Effort::High));
        assert_eq!(layer.mode, Some(Mode::Free));
        assert_eq!(layer.max_steps, Some(7), "unrelated keys preserved");
        let _ = std::fs::remove_dir_all(&dir);
    }

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
    fn rules_parse_and_match_shape() {
        let c = Config::parse_layer(
            "[[rules]]\ntool = \"bash\"\npattern = \"cargo *\"\nverdict = \"allow\"\n\n[[rules]]\ntool = \"write\"\nverdict = \"deny\"\n",
            "user",
        )
        .unwrap();
        assert_eq!(c.rules.len(), 2);
        assert_eq!(c.rules[0].verdict, Verdict::Allow);
        assert_eq!(c.rules[1].pattern, None);
        assert_eq!(c.rules[1].verdict, Verdict::Deny);
        // unknown verdict rejected
        let bad = Config::parse_layer("[[rules]]\ntool = \"bash\"\nverdict = \"maybe\"\n", "user");
        assert!(bad.is_err());
    }

    #[test]
    fn hooks_parse() {
        let c = Config::parse_layer(
            "[[hooks]]\nevent = \"pre_tool_use\"\ntool = \"bash\"\ncommand = \"guard.sh\"\n",
            "user",
        )
        .unwrap();
        assert_eq!(c.hooks.len(), 1);
        assert_eq!(c.hooks[0].event, HookEvent::PreToolUse);
        assert_eq!(c.hooks[0].tool.as_deref(), Some("bash"));
        let bad = Config::parse_layer("[[hooks]]\nevent = \"whenever\"\ncommand = \"x\"\n", "u");
        assert!(bad.is_err());
    }

    #[test]
    fn schema_emits() {
        let schema = Config::schema_json().unwrap();
        assert!(schema.contains("\"Config\""), "got: {schema}");
    }
}
