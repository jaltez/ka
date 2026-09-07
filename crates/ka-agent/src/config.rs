//! Layered strict-TOML configuration.
//!
//! Chain (lowest → highest): built-in defaults → user `~/.config/ka/ka.toml`
//! → project `.ka/ka.toml` → environment (`KA_MODEL`, `KA_MODE`) → CLI flags.
//! Every textual layer is parsed strictly: unknown keys are hard errors that
//! carry the TOML position, so typos never silently pass.
//!
//! Optional spend/context guards live under `[guards]` (both default off):
//!
//! ```toml
//! [guards]
//! spend_usd = 2.0   # ask before the session's total cost crosses $2.00
//! context_pct = 90  # ask when the context meter first passes 90%
//! ```
//!
//! On the first crossing per session per guard the engine poses the
//! existing `Event::Ask` flow with `[continue, stop]`; `stop` aborts the
//! current turn cleanly. Each guard latches once per session.

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

/// One hook: a shell command run around tool calls or at turn end.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Hook {
    /// When to run.
    pub event: HookEvent,
    /// Only this tool (None = every tool).
    #[serde(default)]
    pub tool: Option<String>,
    /// Shell command. Tool-call hooks receive the tool name + arguments
    /// on stdin; `stop` hooks receive `{"event": "stop", "stop": "done"
    /// | "aborted" | "error"}`. Exit 2 blocks the call (pre_tool_use)
    /// with stderr as the reason.
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
    /// After a turn finishes, any stop kind.
    Stop,
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

/// Persistent tool allowlist (`[permissions] allow = [...]` in a ka.toml
/// layer). Listed tools skip the permission ask for the rest of the
/// session; picking "always" on an ask appends here (project layer).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct Permissions {
    /// Tool names auto-allowed without asking.
    pub allow: Vec<String>,
}

/// Spend/context guard thresholds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct Guards {
    /// USD total for the session at which the engine asks to continue
    /// (None = disabled).
    pub spend_usd: Option<f64>,
    /// Context-window percentage (1–100) at which the engine asks to
    /// continue (None = disabled).
    pub context_pct: Option<u64>,
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

/// Per-tool settings (`[tools]` in a ka.toml layer).
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields, default)]
pub struct Tools {
    /// Bash tool settings.
    pub bash: BashTools,
    /// Read tool settings.
    pub read: ReadTools,
    /// Web tool settings.
    pub web: WebTools,
}

/// Read tool tuning (`[tools.read]`).
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields, default)]
pub struct ReadTools {
    /// Image size cap in MB for the read hand (None = 5; 0 = unlimited).
    pub max_image_mb: Option<u32>,
}

/// Web tool tuning (`[tools.web]`).
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields, default)]
pub struct WebTools {
    /// Allow fetching private/loopback addresses (None = false).
    pub allow_private_hosts: Option<bool>,
}

/// Bash tool tuning (`[tools.bash]`).
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields, default)]
pub struct BashTools {
    /// Auto-background a bash command still running after this many
    /// milliseconds: it keeps running, the model gets a normal result
    /// pointing at the `jobs` tool. None = the default (30000); 0 = never
    /// background.
    pub background_after_ms: Option<u64>,
}

/// Default image size cap for the read hand (5 MB).
pub const DEFAULT_MAX_IMAGE_MB: u32 = 5;

/// Default bash auto-background threshold (30s).
pub const DEFAULT_BACKGROUND_AFTER_MS: u64 = 30_000;

/// Self-update settings ([update]).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct Update {
    /// GitHub repo (`owner/name`) releases are fetched from.
    pub repo: Option<String>,
}

/// One web-search provider (`[[search]]`; first entry is active).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct SearchProvider {
    /// `tavily` | `brave` | `bing`.
    pub provider: String,
    /// Env var holding the API key.
    pub api_key_env: String,
    /// API base override (fixture tests, proxies).
    pub base_url: Option<String>,
}

/// Sandbox policy ([sandbox]).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct Sandbox {
    /// `"off"` (default) or `"fs"`.
    pub mode: Option<String>,
}

/// TUI appearance ([tui]).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct Tui {
    /// Title glyph for the transcript window and sidebar (default ◆).
    pub header_glyph: Option<String>,
}

impl Sandbox {
    /// Convert to the sandbox crate's policy config.
    pub fn to_policy_config(&self) -> ka_sandbox::SandboxConfig {
        ka_sandbox::SandboxConfig {
            mode: self.mode.clone(),
        }
    }
}

/// Context-window policy ([context]).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct Context {
    /// Auto-promote to a bigger-context sibling on overflow
    /// (None = default true; false opts out).
    pub promote: Option<bool>,
}

/// Fallback model chain ([fallback]). When a turn fails on the active
/// model with a provider/auth error after retries are exhausted, the
/// engine re-dispatches the same messages on the next entry.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct Fallback {
    /// Fallback selectors (`vendor/model[:effort]`) in try-order; at
    /// most 2 hops are taken per turn.
    pub models: Vec<String>,
}

/// Engine configuration. All fields optional at the data level; resolution
/// order is applied by [`Config::overlay`] consumers.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    /// Default model selector (`vendor/model:effort`).
    pub model: Option<String>,
    /// Default reasoning effort.
    pub effort: Option<Effort>,
    /// Permission mode: `guarded` | `accept_edits` | `free` | `plan`
    /// (defaults to free when absent everywhere).
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
    /// MCP servers (stdio): tools appear as `<name>.<tool>` hands.
    #[serde(default)]
    pub mcp: Vec<crate::mcp::McpServerConfig>,
    /// Persistent tool allowlist ([permissions] allow).
    #[serde(default)]
    pub permissions: Permissions,
    /// Spend/context guard thresholds ([guards]; both default off).
    #[serde(default)]
    pub guards: Guards,
    /// Fallback model chain ([fallback]).
    #[serde(default)]
    pub fallback: Fallback,
    /// Self-update settings ([update]).
    #[serde(default)]
    pub update: Update,
    /// Context-window policy ([context]).
    #[serde(default)]
    pub context: Context,
    /// Web-search providers ([[search]]; first entry is active).
    #[serde(default)]
    pub search: Vec<SearchProvider>,
    /// Sandbox policy ([sandbox]).
    #[serde(default)]
    pub sandbox: Sandbox,
    /// TUI appearance ([tui]).
    #[serde(default)]
    pub tui: Tui,
    /// Per-tool settings ([tools]).
    #[serde(default)]
    pub tools: Tools,
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
        if !other.permissions.allow.is_empty() {
            self.permissions.allow = other.permissions.allow;
        }
        if other.guards.context_pct.is_some() {
            self.guards.context_pct = other.guards.context_pct;
        }
        if !other.mcp.is_empty() {
            self.mcp = other.mcp;
        }
        if other.tools.bash.background_after_ms.is_some() {
            self.tools.bash.background_after_ms = other.tools.bash.background_after_ms;
        }
        if other.tools.read.max_image_mb.is_some() {
            self.tools.read.max_image_mb = other.tools.read.max_image_mb;
        }
        if !other.fallback.models.is_empty() {
            self.fallback.models = other.fallback.models;
        }
        if other.update.repo.is_some() {
            self.update.repo = other.update.repo;
        }
        if other.context.promote.is_some() {
            self.context.promote = other.context.promote;
        }
    }
    /// Effective step cap (default 20).
    pub fn effective_max_steps(&self) -> u32 {
        self.max_steps.unwrap_or(20)
    }

    /// Effective bash auto-background threshold (default 30000, 0 = off).
    pub fn effective_bash_background_after_ms(&self) -> u64 {
        self.tools
            .bash
            .background_after_ms
            .unwrap_or(DEFAULT_BACKGROUND_AFTER_MS)
    }
    /// The title glyph (default ◆).
    pub fn effective_header_glyph(&self) -> String {
        self.tui
            .header_glyph
            .clone()
            .unwrap_or_else(|| "\u{25c6}".to_string())
    }

    /// Whether web fetches may target private hosts (default false).
    pub fn effective_web_allow_private(&self) -> bool {
        self.tools.web.allow_private_hosts.unwrap_or(false)
    }

    /// The active search provider, if any.
    pub fn effective_search(&self) -> Option<SearchProvider> {
        self.search.first().cloned()
    }

    /// Whether overflow promotion is enabled (default true).
    pub fn effective_context_promote(&self) -> bool {
        self.context.promote.unwrap_or(true)
    }

    /// Effective read-hand image cap in MB (default 5, 0 = unlimited).
    pub fn effective_max_image_mb(&self) -> u32 {
        self.tools.read.max_image_mb.unwrap_or(DEFAULT_MAX_IMAGE_MB)
    }

    /// The effective permission mode (free unless set in a layer).
    pub fn effective_mode(&self) -> Mode {
        self.mode.unwrap_or(Mode::Free)
    }

    /// JSON schema for editor integration (`ka config schema`).
    pub fn schema_json() -> Result<String, serde_json::Error> {
        let schema = schemars::schema_for!(Config);
        serde_json::to_string_pretty(&schema)
    }
}

/// Append `tool` to `[permissions] allow` in the PROJECT config layer
/// (`<cwd>/.ka/ka.toml`), preserving all other keys. Already-listed
/// tools are a no-op (`None`). Best-effort: parse or write failures
/// return `None` rather than clobbering a file we could not read.
pub fn save_project_permission(cwd: &std::path::Path, tool: &str) -> Option<std::path::PathBuf> {
    let path = cwd.join(".ka/ka.toml");
    let mut layer = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| Config::parse_layer(&text, "project").ok())
        .unwrap_or_default();
    if layer.permissions.allow.iter().any(|t| t == tool) {
        return None;
    }
    layer.permissions.allow.push(tool.to_string());
    let mut text = String::from("# ka project config — extended by an \"always\" permission\n\n");
    text.push_str(&toml::to_string_pretty(&layer).ok()?);
    std::fs::create_dir_all(path.parent()?).ok()?;
    std::fs::write(&path, text).ok()?;
    Some(path)
}

/// The user config layer path (`~/.config/ka/ka.toml`, XDG-aware).
/// Upsert an API key into `~/.config/ka/.env` (created 0600) and the
/// live dotenv layer, so the key works immediately without a restart.
pub fn save_api_key(env_var: &str, value: &str) -> std::io::Result<std::path::PathBuf> {
    let path = user_config_path().with_file_name(".env");
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut lines: Vec<String> = std::fs::read_to_string(&path)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect();
    let pattern = format!("{env_var}=");
    match lines
        .iter_mut()
        .find(|l| l.trim_start().starts_with(&pattern))
    {
        Some(slot) => *slot = format!("{env_var}={value}"),
        None => lines.push(format!("{env_var}={value}")),
    }
    let mut text = lines.join("\n");
    text.push('\n');
    std::fs::write(&path, text)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    ka_dialect::auth::set_dotenv_key(env_var, value);
    Ok(path)
}

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
        assert_eq!(c.effective_mode(), Mode::Free);
        assert!(c.model.is_none());
    }

    #[test]
    fn mode_defaults_to_free_but_explicit_still_wins() {
        assert_eq!(Config::default().effective_mode(), Mode::Free);
        let guarded = Config::parse_layer("mode = \"guarded\"\n", "user").unwrap();
        assert_eq!(guarded.effective_mode(), Mode::Guarded);
        let accept = Config::parse_layer("mode = \"accept_edits\"\n", "user").unwrap();
        assert_eq!(accept.effective_mode(), Mode::AcceptEdits);
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
    fn unknown_key_rejected() {
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
    fn roles_parse_overlay_and_land_in_schema() {
        // both role selectors parse
        let c = Config::parse_layer("[roles]\ndefault = \"a/x\"\nfast = \"b/y@low\"\n", "user")
            .unwrap();
        assert_eq!(c.roles.default.as_deref(), Some("a/x"));
        assert_eq!(c.roles.fast.as_deref(), Some("b/y@low"));
        // empty table is valid: roles default to None
        let empty = Config::parse_layer("[roles]\n", "user").unwrap();
        assert_eq!(empty.roles, Roles::default());
        // layered overlay: set fields win, unset fields keep
        let mut base =
            Config::parse_layer("[roles]\ndefault = \"a/x\"\nfast = \"b/y\"\n", "base").unwrap();
        let over = Config::parse_layer("[roles]\nfast = \"c/z\"\n", "over").unwrap();
        base.overlay(over);
        assert_eq!(base.roles.default.as_deref(), Some("a/x"));
        assert_eq!(base.roles.fast.as_deref(), Some("c/z"));
        // unknown keys under [roles] hard-error (typo safety)
        let err = Config::parse_layer("[roles]\nfassst = \"b/y\"\n", "user")
            .unwrap_err()
            .to_string();
        assert!(err.contains("fassst"), "got: {err}");
        // the editor schema exposes the roles table with both keys
        let schema = Config::schema_json().unwrap();
        assert!(schema.contains("\"Roles\""), "got: {schema}");
        let roles_part = schema.split("\"Roles\"").nth(1).expect("Roles definition");
        assert!(roles_part.contains("default"), "got: {roles_part}");
        assert!(roles_part.contains("fast"), "got: {roles_part}");
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
        let stop = Config::parse_layer(
            "[[hooks]]\nevent = \"stop\"\ncommand = \"notify.sh\"\n",
            "user",
        )
        .unwrap();
        assert_eq!(stop.hooks[0].event, HookEvent::Stop);
        assert!(stop.hooks[0].tool.is_none());
        let bad = Config::parse_layer("[[hooks]]\nevent = \"whenever\"\ncommand = \"x\"\n", "u");
        assert!(bad.is_err());
    }

    #[test]
    fn schema_emits() {
        let schema = Config::schema_json().unwrap();
        assert!(schema.contains("\"Config\""), "got: {schema}");
    }

    #[test]
    fn bash_background_threshold_defaults_overlays_and_rejects_unknown() {
        // default 30000 when absent everywhere
        assert_eq!(
            Config::default().effective_bash_background_after_ms(),
            DEFAULT_BACKGROUND_AFTER_MS
        );
        // set + layered overlay (set fields win)
        let mut base =
            Config::parse_layer("[tools.bash]\nbackground_after_ms = 1000\n", "base").unwrap();
        assert_eq!(base.effective_bash_background_after_ms(), 1000);
        let over =
            Config::parse_layer("[tools.bash]\nbackground_after_ms = 250\n", "over").unwrap();
        base.overlay(over);
        assert_eq!(base.effective_bash_background_after_ms(), 250);
        // unset upper layer keeps the lower layer's value
        let lower =
            Config::parse_layer("[tools.bash]\nbackground_after_ms = 1000\n", "lower").unwrap();
        let mut merged = Config::default();
        merged.overlay(lower);
        assert_eq!(merged.effective_bash_background_after_ms(), 1000);
        // unknown keys still hard-error
        let err = Config::parse_layer("[tools.bash]\nbackground_after_sec = 1\n", "user")
            .unwrap_err()
            .to_string();
        assert!(err.contains("background_after_sec"), "got: {err}");
        let err = Config::parse_layer("[tools]\nbashh = {}\n", "user")
            .unwrap_err()
            .to_string();
        assert!(err.contains("bashh"), "got: {err}");
    }

    #[test]
    fn schema_contains_background_after_ms() {
        let schema = Config::schema_json().unwrap();
        assert!(schema.contains("background_after_ms"), "got: {schema}");
    }
}
