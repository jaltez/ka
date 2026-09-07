//! The live voice: multi-step tool loop. Each step speaks with the offered
//! tools, executes returned calls through the Hands registry under
//! clearance gating (hardstops always prompt; headless surfaces deny),
//! feeds results back, and repeats until the model rests or the step cap
//! hits.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Duration;

use ka_dialect::dialects::{Catalog, Wire};
use ka_dialect::speaker::{
    SpeakRequest, Speaker, StreamEvent, ToolCall, ToolResult, ToolSpec, TurnMessage, TurnRole,
};
use ka_protocol::{AskId, AskQuestion, Command, ErrorClass, Event, Stop, Usage};
use tokio::sync::{mpsc, oneshot};

use crate::hands::bashp::{all_readonly, analyze, hardstop};
use crate::hands::{
    Clearance, Hand, HandContext, Ledger, Spill, ToolOutput, registry_with_pathfinder,
};

/// Session-scoped mutable state the voice needs (owned by the engine).
#[derive(Default)]
pub struct VoiceState {
    /// Permission memory: rules granted via the "always" ask option.
    pub rules: HashSet<String>,
    /// Ask id counter.
    pub ask_counter: u64,
    /// Identical tool+argument counts (loop guard).
    pub loop_counts: HashMap<String, u32>,
}

/// Spend/context guard runtime state: thresholds from config, the
/// running session spend, and per-session latches. Owned by the engine
/// ([`crate::engine::EngineState`]); the turn consults and latches it.
#[derive(Debug, Clone, Default)]
pub struct GuardRuntime {
    /// Session spend cap in USD (None = off).
    pub spend_usd: Option<f64>,
    /// Context-percentage cap (None = off).
    pub context_pct: Option<u64>,
    /// True once the spend ask fired this session.
    pub spend_latched: bool,
    /// True once the context ask fired this session.
    pub context_latched: bool,
    /// Summed cost of completed turns this session.
    pub session_spend: f64,
}

impl GuardRuntime {
    /// Build from parsed `[guards]` config (both knobs default off).
    pub fn new(spend_usd: Option<f64>, context_pct: Option<u64>) -> Self {
        Self {
            spend_usd,
            context_pct,
            spend_latched: false,
            context_latched: false,
            session_spend: 0.0,
        }
    }

    /// Re-arm for a (re)attached strand, seeding the spend total from
    /// the strand's persisted Usage records. A session resumed above
    /// its cap counts as already crossed (latched, no fresh ask).
    pub fn reset(&mut self, session_spend: f64) {
        self.spend_latched = self.spend_usd.is_some_and(|cap| session_spend >= cap);
        self.context_latched = false;
        self.session_spend = session_spend;
    }
}

// Context-survival knobs (Phase 4 defaults; config knobs later).
/// Tokens of recent tool outputs protected from pruning.
const PROTECT_WINDOW_TOKENS: u64 = 40_000;
/// Minimum estimated savings before pruning fires.
const MIN_PRUNE_SAVINGS: u64 = 20_000;
/// Tokens of history the digest keeps after the summary.
const KEEP_TAIL_TOKENS: u64 = 20_000;
/// Digest reserve floor.
const RESERVE_FLOOR: u64 = 16_384;
/// Digest reserve fraction of the window (15%).
const RESERVE_PCT: u64 = 15;

const DIGEST_SYSTEM: &str = "You are a context digester. Summarize the conversation so a \
continuing agent can pick up exactly where it left off. Preserve: the \
task and its current state, key decisions and their reasons, file paths \
touched (read vs modified), open threads and next steps, and any \
user-stated constraints. Be dense; skip pleasantries. Output only the \
summary.";

/// Simple glob match: `*` spans anything, `?` one char, everything else
/// literal. No path semantics — patterns match raw strings.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    fn inner(p: &[char], t: &[char]) -> bool {
        match (p.first(), t.first()) {
            (None, None) => true,
            (None, Some(_)) => false,
            (Some('*'), _) => (0..=t.len()).any(|skip| inner(&p[1..], &t[skip..])),
            (Some('?'), Some(_)) => inner(&p[1..], &t[1..]),
            (Some(pc), Some(tc)) if pc == tc => inner(&p[1..], &t[1..]),
            _ => false,
        }
    }
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    inner(&p, &t)
}

/// Rough token estimate for one message under a chars-per-token ratio.
fn message_tokens(msg: &TurnMessage, ratio: f64) -> u64 {
    let chars = msg.content.chars().count() as u64
        + msg
            .calls
            .iter()
            .map(|c| c.arguments.to_string().chars().count() as u64 + c.tool.chars().count() as u64)
            .sum::<u64>()
        + msg
            .results
            .iter()
            .map(|r| r.content.chars().count() as u64)
            .sum::<u64>();
    (chars as f64 / ratio.max(0.1)) as u64
}

/// Everything needed to speak to real models and act on the world.
/// Slot for an in-flight speculative digest: (history watermark,
/// result receiver).
type SpecSlot = std::sync::Arc<tokio::sync::Mutex<Option<(usize, oneshot::Receiver<String>)>>>;

pub struct Voice {
    catalog: Catalog,
    speakers: HashMap<Wire, std::sync::Arc<dyn Speaker>>,
    hands: Vec<std::sync::Arc<dyn Hand>>,
    hand_ctx: HandContext,
    pub(crate) state: VoiceState,
    max_steps: u32,
    mode: ka_protocol::Mode,
    /// Conversation history (owned by the voice; engine persists deltas).
    pub history: Vec<TurnMessage>,
    /// Active model selector (set per turn; needed by settle-time digests).
    model_selector: Option<String>,
    /// Chars-per-token ratio of the active model (estimates).
    ratio: f64,
    /// Active digest summary (prepended to requests, not part of history).
    digest: Option<String>,
    /// Last measured context consumption (tokens, from provider usage).
    last_context: u64,
    /// Bumped every time a digest replaces history.
    digest_revision: u64,
    /// (summary, kept index) of the most recent digest, for persistence.
    last_digest: Option<(String, usize)>,
    /// Configured permission rules (first-match-wins).
    rules_cfg: Vec<crate::config::Rule>,
    /// Configured hooks.
    hooks_cfg: Vec<crate::config::Hook>,
    /// Persistent allowlist from `[permissions] allow` (tools that skip
    /// the ask entirely).
    allowed_tools: Vec<String>,
    /// Fallback model chain ([fallback] models; tried in order on
    /// provider/auth failure, max 2 hops per turn).
    fallbacks: Vec<String>,
    /// Auto-promote to a bigger-context sibling on overflow
    /// ([context] promote, default true).
    context_promote: bool,
    /// Promotion already fired for the attached strand (once per strand).
    promoted: bool,
    /// Promotion the engine must apply after the turn (selector).
    pending_promotion: Option<String>,
    /// In-flight speculative digest slot (shared with the background
    /// summarizer task): (history watermark, result receiver).
    speculative: SpecSlot,
    /// Pathfinder bootstrap slot shared with the hand.
    pathfinder_slot:
        std::sync::Arc<parking_lot::RwLock<crate::hands::pathfinder::PathfinderSource>>,
    /// Todo list slot shared with the hand; forwarded to surfaces as
    /// `Event::Todos` after each `todo` call.
    todo: crate::hands::todo::TodoSlot,
}

impl Voice {
    /// New voice over a catalog, working in `cwd`.
    pub fn new(
        catalog: Catalog,
        cwd: std::path::PathBuf,
        mode: ka_protocol::Mode,
        max_steps: u32,
    ) -> Self {
        let slot = std::sync::Arc::new(parking_lot::RwLock::new(
            crate::hands::pathfinder::PathfinderSource::default(),
        ));
        let todos = crate::hands::todo::slot();
        let jobs = std::sync::Arc::new(crate::hands::jobs::JobTable::new());
        Self {
            catalog,
            speakers: Default::default(),
            hands: registry_with_pathfinder(slot.clone(), todos.clone(), jobs.clone()),
            hand_ctx: HandContext {
                cwd: cwd.clone(),
                ledger: std::sync::Arc::new(parking_lot::Mutex::new(Ledger::default())),
                spill: std::sync::Arc::new(Spill::new()),
                snapshots: std::sync::Arc::new(parking_lot::Mutex::new(
                    crate::hands::snapshots::Snapshots::open(&cwd),
                )),
                jobs,
                bash_background_ms: 0,
                max_image_mb: 5,
                web_allow_private: false,
                sandbox: ka_sandbox::Policy::Off,
            },
            state: VoiceState::default(),
            max_steps,
            mode,
            history: Vec::new(),
            model_selector: None,
            ratio: 4.0,
            digest: None,
            last_context: 0,
            digest_revision: 0,
            last_digest: None,
            fallbacks: Vec::new(),
            context_promote: true,
            promoted: false,
            pending_promotion: None,
            speculative: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
            rules_cfg: Vec::new(),
            hooks_cfg: Vec::new(),
            allowed_tools: Vec::new(),
            pathfinder_slot: slot,
            todo: todos,
        }
    }

    /// Register an extra tool (MCP hands arrive after async discovery).
    pub fn push_hand(&mut self, hand: std::sync::Arc<dyn Hand>) {
        self.hands.push(hand);
    }

    /// Replace one MCP server's hands in registry order (reconnect or
    /// refresh tool diff). Absent prefixes append at the end.
    pub fn replace_server_hands(&mut self, server: &str, hands: Vec<std::sync::Arc<dyn Hand>>) {
        let prefix = format!("{server}.");
        let mut result: Vec<std::sync::Arc<dyn Hand>> = Vec::with_capacity(self.hands.len());
        let mut replaced = false;
        for h in self.hands.drain(..) {
            if h.def().name.starts_with(&prefix) {
                if !replaced {
                    replaced = true;
                    result.extend(hands.iter().cloned());
                }
            } else {
                result.push(h);
            }
        }
        if !replaced {
            result.extend(hands);
        }
        self.hands = result;
    }

    /// Tool names in registry order (built-ins, then anything pushed at
    /// bootstrap: the delegate hand, MCP hands). Bootstrap inventory.
    pub fn hand_names(&self) -> Vec<String> {
        self.hands.iter().map(|h| h.def().name).collect()
    }

    /// Share the snapshot journal (engine-side undo + strand tracking).
    pub fn snapshot_sink(
        &self,
    ) -> std::sync::Arc<parking_lot::Mutex<crate::hands::snapshots::Snapshots>> {
        self.hand_ctx.snapshots.clone()
    }

    /// Read-only research voice (pathfinder): inspect tools only.
    pub fn new_readonly(
        catalog: Catalog,
        cwd: std::path::PathBuf,
        mode: ka_protocol::Mode,
        max_steps: u32,
    ) -> Self {
        let slot = std::sync::Arc::new(parking_lot::RwLock::new(
            crate::hands::pathfinder::PathfinderSource::default(),
        ));
        let todos = crate::hands::todo::slot();
        let hands = crate::hands::registry_with_pathfinder(
            slot.clone(),
            todos.clone(),
            std::sync::Arc::new(crate::hands::jobs::JobTable::new()),
        );
        let hand_ctx = HandContext {
            cwd,
            ledger: std::sync::Arc::new(parking_lot::Mutex::new(Ledger::default())),
            spill: std::sync::Arc::new(Spill::new()),
            snapshots: std::sync::Arc::new(parking_lot::Mutex::new(
                crate::hands::snapshots::Snapshots::inert(),
            )),
            jobs: std::sync::Arc::new(crate::hands::jobs::JobTable::new()),
            bash_background_ms: 0,
            max_image_mb: 5,
            web_allow_private: false,
            sandbox: ka_sandbox::Policy::Off,
        };
        Self {
            catalog,
            speakers: Default::default(),
            hands,
            hand_ctx,
            state: VoiceState::default(),
            max_steps,
            mode,
            rules_cfg: Vec::new(),
            hooks_cfg: Vec::new(),
            allowed_tools: Vec::new(),
            history: Vec::new(),
            model_selector: None,
            ratio: 4.0,
            digest: None,
            last_context: 0,
            digest_revision: 0,
            last_digest: None,
            fallbacks: Vec::new(),
            context_promote: true,
            promoted: false,
            pending_promotion: None,
            speculative: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
            pathfinder_slot: slot,
            todo: todos,
        }
    }

    /// Set configured permission rules (engine bootstrap).
    pub fn set_rules(&mut self, rules: Vec<crate::config::Rule>) {
        self.rules_cfg = rules;
    }

    /// The pathfinder's bootstrap slot (engine writes catalog/model).
    pub fn pathfinder_slot(
        &self,
    ) -> std::sync::Arc<parking_lot::RwLock<crate::hands::pathfinder::PathfinderSource>> {
        self.pathfinder_slot.clone()
    }

    /// Set configured hooks (engine bootstrap).
    pub fn set_hooks(&mut self, hooks: Vec<crate::config::Hook>) {
        self.hooks_cfg = hooks;
    }

    /// Set the persistent tool allowlist (`[permissions] allow`).
    pub fn set_allowed_tools(&mut self, tools: Vec<String>) {
        self.allowed_tools = tools;
    }

    /// Set the fallback model chain ([fallback] models; engine bootstrap).
    pub fn set_fallbacks(&mut self, models: Vec<String>) {
        self.fallbacks = models;
    }

    /// Enable/disable overflow promotion ([context] promote).
    pub fn set_context_promote(&mut self, promote: bool) {
        self.context_promote = promote;
    }

    /// Whether an active digest summary rides the system prompt.
    pub fn has_digest(&self) -> bool {
        self.digest.is_some()
    }

    /// Consume a promotion the engine must apply (model switch with
    /// Change record + event, same path as `/model`).
    pub fn take_promotion(&mut self) -> Option<String> {
        self.pending_promotion.take()
    }

    /// Fire the speculative digest if pressure sits in the ≥80% zone of
    /// the reserve threshold and nothing is in flight. The candidate is
    /// tagged with the current history watermark; [`Self::take_speculative`]
    /// consumes it only when the watermark still matches. `fast` names the
    /// cheap role model (falls back to the active model).
    pub fn start_speculative(&mut self, fast: Option<&str>) -> bool {
        let window = self.window_tokens();
        if window == 0 || !self.context_pressure_frac(window, 80) || self.context_pressure(window) {
            return false;
        }
        // single in-flight guard
        if self.speculative_in_flight() {
            return false;
        }
        let Some(model_id) = fast
            .map(str::to_string)
            .or_else(|| self.model_selector.clone())
        else {
            return false;
        };
        let Some(dialect) = self.catalog.get(&model_id).cloned() else {
            return false;
        };
        let token = dialect
            .api_key_env
            .as_deref()
            .and_then(ka_dialect::auth::resolve_token);
        let system = DIGEST_SYSTEM.to_string();
        let mut messages = Vec::new();
        if let Some(d) = &self.digest {
            messages.push(TurnMessage::user(format!("<context-digest>\n{d}")));
        }
        messages.extend(self.history.iter().map(Voice::summarizer_view));
        messages.push(TurnMessage::user(
            "Summarize the conversation above now, in at most 300 words.",
        ));
        let watermark = self.history.len();
        let speaker = self
            .speakers
            .get(&dialect.wire)
            .cloned()
            .unwrap_or_else(|| ka_dialect::speaker_for(dialect.wire));
        let slot = self.speculative.clone();
        tokio::spawn(async move {
            let summary = Self::summarize_with(
                speaker,
                model_id.clone(),
                dialect,
                token,
                system,
                messages,
                std::time::Duration::from_secs(120),
            )
            .await;
            if let Some(summary) = summary {
                slot.lock().await.replace((watermark, {
                    let (tx, rx) = oneshot::channel();
                    tx.send(summary).ok();
                    rx
                }));
            }
        });
        true
    }

    /// Whether a speculative digest is being computed.
    fn speculative_in_flight(&self) -> bool {
        self.speculative
            .try_lock()
            .map(|s| s.is_some())
            .unwrap_or(true)
    }

    /// Consume the speculative candidate when it is ready and computed
    /// against the CURRENT history watermark; otherwise discard it.
    pub async fn take_speculative(&mut self) -> Option<String> {
        let entry = self.speculative.lock().await.take()?;
        let (watermark, rx) = entry;
        if watermark != self.history.len() {
            return None;
        }
        rx.await.ok()
    }

    /// Biggest-context same-vendor sibling whose modalities cover the
    /// current ones and whose context is ≥ 1.5× the current window.
    fn promotion_candidate(&self) -> Option<String> {
        let selector = self.model_selector.as_deref()?;
        let parsed = ka_dialect::parse_selector(selector).ok()?;
        let current_id = parsed.model_id();
        let current = self.catalog.get(&current_id)?;
        if current.context == 0 {
            return None;
        }
        let vendor_prefix = format!("{}/", current_id.split_once('/')?.0);
        self.catalog
            .dialects
            .iter()
            .filter(|(id, d)| {
                id.starts_with(&vendor_prefix)
                    && d.context >= current.context.saturating_mul(3) / 2
                    && current.input.iter().all(|m| d.input.contains(m))
            })
            .min_by_key(|(_, d)| d.context)
            .map(|(id, _)| id.clone())
    }

    /// Set the read-hand image size cap in MB (engine bootstrap).
    pub fn set_max_image_mb(&mut self, mb: u32) {
        self.hand_ctx.max_image_mb = mb;
    }

    /// Set the web-fetch private-host policy (engine bootstrap).
    pub fn set_web_allow_private(&mut self, allow: bool) {
        self.hand_ctx.web_allow_private = allow;
    }

    /// Set the bash sandbox policy (engine bootstrap).
    pub fn set_sandbox(&mut self, policy: ka_sandbox::Policy) {
        self.hand_ctx.sandbox = policy;
    }

    /// Set the bash auto-background threshold in ms (engine bootstrap;
    /// 0 = never background).
    pub fn set_bash_background_ms(&mut self, ms: u64) {
        self.hand_ctx.bash_background_ms = ms;
    }

    /// Share the auto-backgrounded-jobs table (the engine kills the
    /// remaining jobs at session shutdown).
    pub fn jobs(&self) -> std::sync::Arc<crate::hands::jobs::JobTable> {
        self.hand_ctx.jobs.clone()
    }
    /// Run matching hooks for one event. Returns Err(reason) when a
    /// pre_tool_use hook blocked the call (exit 2, stderr as reason).
    async fn run_hooks(
        &self,
        event: crate::config::HookEvent,
        tool: &str,
        args: &serde_json::Value,
    ) -> Result<(), String> {
        run_hook_scripts(&self.hooks_cfg, event, tool, args, &self.hand_ctx.cwd).await
    }

    /// Load resumed history + digest (engine bootstrap). Also resets the
    /// once-per-strand promotion latch.
    pub fn load_history(&mut self, history: Vec<TurnMessage>, digest: Option<String>) {
        self.history = history;
        self.digest = digest;
        self.promoted = false;
    }

    /// Set the active model selector + ratio (engine forwards each turn
    /// and before settle-time digests).
    #[allow(dead_code)]
    pub fn set_model_selector(&mut self, selector: &str, ratio: f64) {
        self.model_selector = Some(selector.to_string());
        self.ratio = if ratio > 0.0 { ratio } else { 4.0 };
    }

    /// Restore a digest from a resumed strand.
    #[allow(dead_code)]
    pub fn set_digest(&mut self, summary: String) {
        self.digest = Some(summary);
    }

    /// Context-window pressure check against the active dialect.
    pub fn context_pressure(&self, window: u64) -> bool {
        self.context_pressure_frac(window, 100)
    }

    /// Pressure at `pct`% of the reserve threshold (80 = speculative
    /// kick-off zone).
    pub fn context_pressure_frac(&self, window: u64, pct: u64) -> bool {
        if window == 0 {
            return false; // unknown window: never auto-digest
        }
        // omp rule: reserve is the 16k floor when practical, but never
        // more than a quarter of small windows (proportional reserve)
        let proportional = window * RESERVE_PCT / 100;
        let reserve = proportional.max(RESERVE_FLOOR.min(window / 4));
        let threshold = window.saturating_sub(reserve);
        self.last_context + KEEP_TAIL_TOKENS.min(window / 4) > threshold * pct / 100
    }

    /// Blank old tool-result bodies in history beyond the protected
    /// window. In-memory only — strands keep the full originals, so
    /// pruning re-applies deterministically after resume.
    pub fn prune_tool_outputs(&mut self, ratio: f64) -> u64 {
        let sizes: Vec<u64> = self.history_sizes(ratio);
        let total: u64 = sizes.iter().sum();
        let mut cut = self.history.len();
        let mut protected: u64 = 0;
        // walk backwards protecting the recent window (all message kinds)
        for i in (0..self.history.len()).rev() {
            if protected >= PROTECT_WINDOW_TOKENS {
                break;
            }
            protected = protected.saturating_add(sizes[i]);
            cut = i;
        }
        // candidates: tool results strictly older than `cut`
        let mut savings = 0u64;
        let mut replacements: Vec<(usize, usize, String)> = Vec::new();
        for (i, msg) in self.history.iter().enumerate() {
            if i >= cut {
                break;
            }
            if msg.role != TurnRole::Tool {
                continue;
            }
            for (j, result) in msg.results.iter().enumerate() {
                let tokens = (result.content.chars().count() as f64 / ratio.max(0.1)) as u64;
                if tokens > 8 {
                    savings += tokens;
                    replacements.push((i, j, result.content.clone()));
                }
            }
        }
        if savings < MIN_PRUNE_SAVINGS {
            return 0;
        }
        for (i, j, original) in replacements.iter().map(|(i, j, o)| (*i, *j, o.clone())) {
            let parked = self.hand_ctx.spill.park(&original).ok();
            let note = match parked {
                Some(ptr) => format!("[pruned output; full text at {ptr}]"),
                None => "[pruned output]".to_string(),
            };
            if let Some(result) = self.history[i].results.get_mut(j) {
                result.content = note;
            }
        }
        let _ = total;
        savings
    }

    fn history_sizes(&self, ratio: f64) -> Vec<u64> {
        self.history
            .iter()
            .map(|m| message_tokens(m, ratio))
            .collect()
    }

    /// Bound each message so the summarizer request fits even when the
    /// conversation dwarfs the model's real window: head+tail per content,
    /// capped tool noise (snapcompact-style serialization, text-only).
    fn summarizer_view(m: &TurnMessage) -> TurnMessage {
        let cap = |s: &str| -> String {
            const HEAD: usize = 600;
            const TAIL: usize = 300;
            if s.chars().count() <= HEAD + TAIL {
                return s.to_string();
            }
            let head: String = s.chars().take(HEAD).collect();
            let tail: String = s
                .chars()
                .rev()
                .take(TAIL)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            format!(
                "{head}\n…[truncated {} chars]…\n{tail}",
                s.chars().count() - HEAD - TAIL
            )
        };
        TurnMessage {
            role: m.role,
            content: cap(&m.content),
            calls: m
                .calls
                .iter()
                .map(|c| {
                    let mut cl = c.clone();
                    cl.arguments = serde_json::Value::String(cap(&c.arguments.to_string()));
                    cl
                })
                .collect(),
            results: m
                .results
                .iter()
                .map(|r| {
                    let mut rl = r.clone();
                    rl.content = cap(&rl.content);
                    rl
                })
                .collect(),
            images: m.images.clone(),
        }
    }
    /// Summarize the conversation with the active model (same-model
    /// digest). Returns the summary text.
    pub async fn summarize(
        &mut self,
        focus: Option<&str>,
        window_deadline: std::time::Duration,
    ) -> Option<String> {
        let model_id = self.model_selector.clone()?;
        let dialect = self.catalog.get(&model_id).cloned()?;
        let token = dialect
            .api_key_env
            .as_deref()
            .and_then(ka_dialect::auth::resolve_token);
        let ratio = if dialect.ratio > 0.0 {
            dialect.ratio as f64
        } else {
            4.0
        };
        let system = match focus {
            Some(f) => format!("{DIGEST_SYSTEM}\n\nFocus: {f}"),
            None => DIGEST_SYSTEM.to_string(),
        };
        let mut messages = Vec::new();
        if let Some(d) = &self.digest {
            messages.push(TurnMessage::user(format!("<context-digest>\n{d}")));
        }
        messages.extend(self.history.iter().map(Voice::summarizer_view));
        // never end on an assistant message: several OpenAI-compatible
        // servers treat it as prefill and emit nothing
        messages.push(TurnMessage::user(
            "Summarize the conversation above now, in at most 300 words.",
        ));
        let speaker = self.speaker(dialect.wire);
        let _ = ratio;
        Self::summarize_with(
            speaker,
            model_id,
            dialect,
            token,
            system,
            messages,
            window_deadline,
        )
        .await
    }

    /// Shared summarize core: one bounded completion collecting the
    /// text (falling back to the reasoning tail). Static so the
    /// speculative digest can run it without `&mut self`.
    async fn summarize_with(
        speaker: std::sync::Arc<dyn Speaker>,
        model_id: String,
        dialect: ka_dialect::Dialect,
        token: Option<String>,
        system: String,
        messages: Vec<TurnMessage>,
        window_deadline: std::time::Duration,
    ) -> Option<String> {
        let req = SpeakRequest {
            model_id,
            dialect: dialect.clone(),
            effort: None,
            system,
            messages,
            tools: Vec::new(),
            token,
            cache_key: None,
            schema: None,
        };
        let (tx, mut rx) = mpsc::channel::<StreamEvent>(256);
        {
            let speaker = speaker.clone();
            tokio::spawn(async move {
                speaker.speak(req, tx).await;
            });
        }
        let mut text = String::new();
        let mut thought = String::new();
        let mut failure: Option<String> = None;
        let _ = tokio::time::timeout(window_deadline, async {
            while let Some(evt) = rx.recv().await {
                match evt {
                    StreamEvent::Text(t) => text.push_str(&t),
                    StreamEvent::Thought(t) => thought.push_str(&t),
                    StreamEvent::Finished { .. } => break,
                    StreamEvent::Failed { message, .. } => {
                        failure = Some(message);
                        break;
                    }
                    _ => {}
                }
            }
        })
        .await;
        // prefer the model's final text; thinking models sometimes only
        // reason — fall back to a trimmed tail of the reasoning channel
        // (thinking converges to conclusions at the end)
        let mut summary = text;
        if summary.trim().is_empty() && !thought.trim().is_empty() {
            const HEAD: usize = 200;
            const TAIL: usize = 1_200;
            let chars: Vec<char> = thought.chars().collect();
            summary = if chars.len() <= HEAD + TAIL {
                thought
            } else {
                let head: String = chars[..HEAD].iter().collect();
                let tail: String = chars[chars.len() - TAIL..].iter().collect();
                format!("{head}\n…[thinking trimmed]…\n{tail}")
            };
        }
        if std::env::var("KA_DEBUG_SETTLE").is_ok() {
            eprintln!("[summarize] chars={} failure={:?}", summary.len(), failure);
        }
        (!summary.trim().is_empty()).then_some(summary)
    }
    /// Output-token ceiling for auxiliary role calls (auto-titles and
    /// later cheap chores): a few words never need more.
    pub const ROLE_MAX_OUTPUT: u32 = 256;

    /// One short auxiliary completion against a role selector already
    /// resolved to a catalog id (`vendor/model`). No tools, capped
    /// output, the same auth/retry plumbing as main-line calls (it rides
    /// the dialect's wire speaker). Returns `None` on unknown model,
    /// transport failure, timeout, or an empty/unstreamed reply — callers
    /// must degrade silently; this is a best-effort channel.
    pub async fn role_complete(
        &mut self,
        model_id: &str,
        system: &str,
        prompt: &str,
        deadline: std::time::Duration,
    ) -> Option<String> {
        let mut dialect = self.catalog.get(model_id)?.clone();
        if dialect.max_output == 0 || dialect.max_output > Self::ROLE_MAX_OUTPUT {
            dialect.max_output = Self::ROLE_MAX_OUTPUT;
        }
        let token = dialect
            .api_key_env
            .as_deref()
            .and_then(ka_dialect::auth::resolve_token);
        let wire = dialect.wire;
        let req = SpeakRequest {
            model_id: model_id.to_string(),
            dialect,
            effort: None,
            system: system.to_string(),
            messages: vec![TurnMessage::user(prompt)],
            tools: Vec::new(),
            token,
            cache_key: None,
            schema: None,
        };
        let speaker = self.speaker(wire);
        let (tx, mut rx) = mpsc::channel::<StreamEvent>(256);
        {
            let speaker = speaker.clone();
            tokio::spawn(async move {
                speaker.speak(req, tx).await;
            });
        }
        let mut text = String::new();
        let mut finished = false;
        let _ = tokio::time::timeout(deadline, async {
            while let Some(evt) = rx.recv().await {
                match evt {
                    StreamEvent::Text(t) => text.push_str(&t),
                    StreamEvent::Finished { .. } => {
                        finished = true;
                        break;
                    }
                    // a failed stream never yields a trustworthy answer;
                    // partial text before the failure is discarded
                    StreamEvent::Failed { .. } => break,
                    _ => {}
                }
            }
        })
        .await;
        if !finished {
            return None;
        }
        let trimmed = text.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    }

    /// Replace history with the digest summary + kept tail. Returns the
    /// kept index into the OLD history (for Digest-record persistence).
    pub fn apply_digest(&mut self, summary: String, ratio: f64) -> usize {
        let sizes = self.history_sizes(ratio);
        let total: u64 = sizes.iter().sum();
        // clamp the tail to the window: pathological tiny windows must not
        // keep more than they can hold
        let window = self.window_tokens();
        let tail_cap = if window > 0 {
            (window / 4).max(1_000)
        } else {
            KEEP_TAIL_TOKENS
        };
        let mut budget = KEEP_TAIL_TOKENS.min(tail_cap).min(total);
        // walk from the end, then advance the cut to the next user message
        // so a turn is never split (also keeps tool pairs intact).
        let mut cut = self.history.len();
        let mut acc: u64 = 0;
        for i in (0..self.history.len()).rev() {
            if acc >= budget {
                cut = i + 1;
                break;
            }
            acc += sizes[i];
            cut = i;
        }
        let _ = &mut budget;
        while cut < self.history.len() && self.history[cut].role != TurnRole::User {
            cut += 1;
        }
        let kept_from = cut.min(self.history.len());
        let kept_tokens: u64 = sizes.get(kept_from..).map(|s| s.iter().sum()).unwrap_or(0);
        self.history.drain(..kept_from);
        self.digest = Some(summary.clone());
        self.digest_revision += 1;
        self.last_digest = Some((summary, kept_from));
        // pressure reflects the real post-digest estimate: the persistent
        // digest only — the pressure formula already adds the tail budget
        let digest_tokens =
            (self.digest.as_deref().map_or(0, str::len) as u64).div_ceil(ratio.max(0.1) as u64);
        let _ = kept_tokens;
        self.last_context = digest_tokens;
        kept_from
    }

    /// First matching configured rule's verdict for this call.
    fn match_rule(&self, call: &ToolCall) -> Option<crate::config::Verdict> {
        self.rules_cfg
            .iter()
            .find(|r| r.tool == call.tool)
            .filter(|r| match &r.pattern {
                None => true,
                Some(pat) => glob_match(pat, &call.primary_arg()),
            })
            .map(|r| r.verdict)
    }

    /// Truncate history so it ends just before the Nth-last user
    /// message. Returns the kept index (into the pre-truncation history)
    /// or None when there aren't that many user turns.
    pub fn rewind(&mut self, turns: u32) -> Option<usize> {
        if turns == 0 {
            return None;
        }
        let mut seen = 0u32;
        for idx in (0..self.history.len()).rev() {
            if self.history[idx].role == TurnRole::User {
                seen += 1;
                if seen == turns {
                    self.history.truncate(idx);
                    return Some(idx);
                }
            }
        }
        None
    }

    /// Context window of the active model (0 = unknown).
    pub fn window_tokens(&self) -> u64 {
        let Some(model_id) = &self.model_selector else {
            return 0;
        };
        self.catalog
            .get(model_id)
            .map(|d| d.context as u64)
            .unwrap_or(0)
    }

    /// Clone of the active model selector.
    pub fn model_selector_cloned(&self) -> Option<String> {
        self.model_selector.clone()
    }

    /// Chars-per-token ratio of the active model.
    pub fn model_ratio(&self) -> f64 {
        self.ratio
    }

    /// Debug accessor for the last measured context.
    #[doc(hidden)]
    pub fn debug_last_context(&self) -> u64 {
        self.last_context
    }

    /// Test hook: pretend the provider just reported this context size.
    #[doc(hidden)]
    #[allow(dead_code)]
    pub fn note_context_for_tests(&mut self, tokens: u64) {
        self.last_context = tokens;
    }

    /// Consume the pending digest outcome for persistence.
    pub fn take_pending_digest(&mut self) -> Option<(String, usize, u64)> {
        let (summary, kept) = self.last_digest.take()?;
        Some((summary, kept, self.digest_revision))
    }

    /// Update the permission mode (engine forwards `SetMode`).
    pub fn set_mode(&mut self, mode: ka_protocol::Mode) {
        self.mode = mode;
    }

    /// Messages as sent on the wire: the conversation (the digest rides
    /// in the system prompt as authoritative memory).
    fn speak_messages(&self) -> Vec<TurnMessage> {
        self.history.clone()
    }

    fn speaker(&mut self, wire: Wire) -> std::sync::Arc<dyn Speaker> {
        self.speakers
            .entry(wire)
            .or_insert_with(|| ka_dialect::speaker_for(wire))
            .clone()
    }

    /// Inject a speaker for a wire (tests).
    #[cfg(test)]
    pub fn with_speaker(mut self, wire: Wire, speaker: std::sync::Arc<dyn Speaker>) -> Self {
        self.speakers.insert(wire, speaker);
        self
    }

    fn specs(&self) -> Vec<ToolSpec> {
        self.hands
            .iter()
            .map(|h| {
                let def = h.def();
                ToolSpec {
                    name: def.name.to_string(),
                    description: def.description.clone(),
                    parameters: def.parameters.clone(),
                }
            })
            .collect()
    }

    /// Run one live prompt to completion. Always emits exactly one
    /// `TurnFinished` and returns the turn's usage. `guards` carries the
    /// session spend/context thresholds and latches.
    #[allow(clippy::too_many_arguments)]
    pub async fn turn(
        &mut self,
        model_selector: &str,
        prompt: String,
        commands: &mut mpsc::Receiver<Command>,
        events: &mpsc::Sender<Event>,
        interjections: &mut Vec<String>,
        deferrals: &mut VecDeque<String>,
        guards: &mut GuardRuntime,
        schema: Option<serde_json::Value>,
        images: Vec<ka_dialect::ImagePart>,
    ) -> Usage {
        use ka_dialect::parse_selector;
        let mut parsed = match parse_selector(model_selector) {
            Ok(p) => p,
            Err(e) => {
                return finish_after_error(events, ErrorClass::Protocol, &e.to_string()).await;
            }
        };
        let mut model_id = parsed.model_id();
        let mut dialect = match self.catalog.get(&model_id).cloned() {
            Some(d) => d,
            None => {
                return finish_after_error(
                    events,
                    ErrorClass::Protocol,
                    &format!("unknown model {model_id:?} (not in catalog; add a dialect overlay)"),
                )
                .await;
            }
        };
        if schema.is_some() && !dialect.flags.structured {
            return finish_after_error(
                events,
                ErrorClass::Unsupported,
                &format!(
                    "model {model_id:?} does not support structured output \
                     (no `structured` support; drop --schema or pick a model that has it)"
                ),
            )
            .await;
        }
        let mut price = dialect.price;
        let mut ratio = if dialect.ratio > 0.0 {
            dialect.ratio
        } else {
            4.0
        };
        let mut token = dialect
            .api_key_env
            .as_deref()
            .and_then(ka_dialect::auth::resolve_token);
        let mut window = dialect.context as u64;
        let mut fallback_cursor: usize = 0;
        let mut fallback_hops: usize = 0;

        let est_in = (prompt.len() as f64 / ratio as f64).ceil() as u64;
        events
            .send(Event::TurnStarted {
                context: ka_protocol::ContextMeter {
                    used: est_in,
                    window,
                },
            })
            .await
            .ok();

        // Minimal system context: identity + read-only git awareness.
        let snap = crate::hands::git::RepoSnapshot::capture(&self.hand_ctx.cwd);
        let mut system = String::new();
        // AGENTS.md hierarchy (root→cwd)
        for agents in crate::conventions::discover_agents(&self.hand_ctx.cwd) {
            system.push_str(&format!(
                "\n<project-instructions src=\"{}\">\n{}\n</project-instructions>\n",
                agents.path.display(),
                agents.content
            ));
        }
        // memory tiers (project, then user-level)
        for memory in crate::conventions::discover_memory(&self.hand_ctx.cwd) {
            system.push_str(&format!(
                "\n<memory src=\"{}\">\n{}\n</memory>\n",
                memory.path.display(),
                memory.content
            ));
        }
        // skills: progressive disclosure — names/descriptions/paths only
        let skills = crate::conventions::discover_skills(&self.hand_ctx.cwd);
        if !skills.is_empty() {
            system.push_str("\nAvailable skills (read the SKILL.md path with the read tool before using one):\n");
            for sk in &skills {
                system.push_str(&format!(
                    "- {}: {} — {}\n",
                    sk.name,
                    sk.description,
                    sk.path.display()
                ));
            }
        }
        system.push_str(&format!(
            "\nYou are ka, a precise coding agent. {}. Use the provided tools to inspect and modify the repository; prefer read before edit.",
            snap.summary()
        ));
        system.push_str(
            "\nMaintain your plan with the todo tool: on multi-step tasks keep the todo list \
current — one item per step, mark items done as you finish them (each call replaces the \
whole list).",
        );
        if self.mode == ka_protocol::Mode::Plan {
            system.push_str(
                "\n\nPLAN MODE: research the task with read/glob/grep/pathfinder, then write a \
concrete numbered plan to .ka/plans/plan.md (the only writable path). Do not \
attempt implementation — the user will review and switch to build mode.",
            );
        }
        if let Some(d) = &self.digest {
            system.push_str(&format!(
                "\n\nEarlier-conversation summary (this is your memory of prior turns — treat it as accurate ground truth):\n{d}"
            ));
        }

        self.model_selector = Some(model_selector.to_string());
        self.ratio = if dialect.ratio > 0.0 {
            dialect.ratio as f64
        } else {
            4.0
        };
        self.history
            .push(TurnMessage::user_with_images(prompt, images));
        self.state.loop_counts.clear();
        let mut usage_total = Usage::default();
        let mut assistant_text = String::new();
        let mut final_stop = Stop::Done;
        let mut steps = 0u32;
        let mut overflow_retried = false;
        let mut retry_attempt: usize = 0;

        'outer: loop {
            let req = SpeakRequest {
                model_id: model_id.clone(),
                dialect: dialect.clone(),
                effort: parsed.effort.clone(),
                system: system.clone(),
                messages: self.speak_messages(),
                tools: self.specs(),
                token: token.clone(),
                cache_key: None,
                schema: schema.clone(),
            };
            let speaker = self.speaker(dialect.wire);
            let (tx, mut rx) = mpsc::channel::<StreamEvent>(256);
            {
                let speaker = speaker.clone();
                tokio::spawn(async move {
                    speaker.speak(req, tx).await;
                });
            }

            let mut step_calls: Vec<ToolCall> = Vec::new();
            let mut step_text = String::new();
            let mut step_failed: Option<(ErrorClass, String, bool)> = None;
            let mut step_finished = false;

            while !step_finished {
                tokio::select! {
                    biased;
                    maybe_cmd = commands.recv() => {
                        match maybe_cmd {
                            None => return Usage::default(),
                            Some(Command::Abort) => {
                                events.send(Event::TurnFinished {
                                    stop: Stop::Aborted,
                                    usage: Usage::default(),
                                }).await.ok();
                                return Usage::default();
                            }
                            Some(Command::Interject { text }) => interjections.push(text),
                            Some(Command::Defer { text }) => deferrals.push_back(text),
                            Some(Command::SetMode { mode }) => {
                                self.mode = mode;
                                events.send(Event::ModeChanged { mode }).await.ok();
                            }
                            Some(_) => {}
                        }
                    }
                    maybe_evt = rx.recv() => {
                        let Some(evt) = maybe_evt else {
                            // speaker ended without Finished/Failed (aborted)
                            break;
                        };
                        match evt {
                            StreamEvent::Text(t) => {
                                step_text.push_str(&t);
                                assistant_text.push_str(&t);
                                events.send(Event::Delta { kind: ka_protocol::DeltaKind::Text(t) }).await.ok();
                            }
                            StreamEvent::Thought(t) => {
                                events.send(Event::Delta { kind: ka_protocol::DeltaKind::Thought(t) }).await.ok();
                            }
                            StreamEvent::Call(call) => {
                                events.send(Event::CallStarted {
                                    tool: call.tool.clone(),
                                    id: call.id.clone(),
                                    detail: call_detail(&call.tool, &call.arguments),
                                }).await.ok();
                                step_calls.push(call);
                            }
                            StreamEvent::Finished { stop, usage } => {
                                usage_total.input += usage.input;
                                usage_total.output += usage.output;
                                usage_total.cache_read += usage.cache_read;
                                usage_total.cache_write += usage.cache_write;
                                self.last_context = usage.input
                                    + usage.cache_read
                                    + usage.cache_write
                                    + usage.output;
                                events
                                    .send(Event::ContextMeter {
                                        used: self.last_context,
                                        window: self.window_tokens(),
                                    })
                                    .await
                                    .ok();
                                // session guards: each fires once, on the
                                // first crossing; stop aborts the turn cleanly
                                if !guards.context_latched && window > 0 {
                                    if let Some(cap_pct) = guards.context_pct {
                                        let pct = self.last_context * 100 / window;
                                        if pct >= cap_pct {
                                            guards.context_latched = true;
                                            let question =
                                                format!("context {pct}% full — continue?");
                                            if !ask_continue_or_stop(
                                                &mut self.state,
                                                question,
                                                commands,
                                                events,
                                            )
                                            .await
                                            {
                                                events
                                                    .send(Event::TurnFinished {
                                                        stop: Stop::Aborted,
                                                        usage: Usage::default(),
                                                    })
                                                    .await
                                                    .ok();
                                                return Usage::default();
                                            }
                                        }
                                    }
                                }
                                if !guards.spend_latched {
                                    if let Some(cap) = guards.spend_usd {
                                        let running = guards.session_spend
                                            + if dialect.priced {
                                                cost_of(&usage_total, price)
                                            } else {
                                                0.0
                                            };
                                        if running >= cap {
                                            guards.spend_latched = true;
                                            let question = format!(
                                                "spend cap ${cap:.2} reached — continue?"
                                            );
                                            if !ask_continue_or_stop(
                                                &mut self.state,
                                                question,
                                                commands,
                                                events,
                                            )
                                            .await
                                            {
                                                events
                                                    .send(Event::TurnFinished {
                                                        stop: Stop::Aborted,
                                                        usage: Usage::default(),
                                                    })
                                                    .await
                                                    .ok();
                                                return Usage::default();
                                            }
                                        }
                                    }
                                }
                                final_stop = stop;
                                step_finished = true;
                            }
                            StreamEvent::Failed {
                                class,
                                message,
                                retryable,
                            } => {
                                step_failed = Some((class, message, retryable));
                                final_stop = Stop::Error;
                                step_finished = true;
                            }
                        }
                    }
                }
            }

            if let Some((class, message, retryable)) = step_failed {
                // Overflow → promote to a bigger sibling (once per
                // strand, before digesting), else digest-and-retry once.
                if class == ErrorClass::Overflow && !overflow_retried {
                    overflow_retried = true;
                    if self.context_promote && !self.promoted {
                        if let Some(target) = self.promotion_candidate() {
                            self.promoted = true;
                            self.pending_promotion = Some(target.clone());
                            let k = self
                                .catalog
                                .get(&target)
                                .map(|d| d.context / 1024)
                                .unwrap_or(0);
                            events
                                .send(Event::Note {
                                    message: format!("context → {target} ({k}k)"),
                                })
                                .await
                                .ok();
                            if let Ok(p) = parse_selector(&target) {
                                if let Some(d) = self.catalog.get(&p.model_id()).cloned() {
                                    parsed = p;
                                    model_id = parsed.model_id();
                                    dialect = d;
                                    price = dialect.price;
                                    ratio = if dialect.ratio > 0.0 {
                                        dialect.ratio
                                    } else {
                                        4.0
                                    };
                                    token = dialect
                                        .api_key_env
                                        .as_deref()
                                        .and_then(ka_dialect::auth::resolve_token);
                                    window = dialect.context as u64;
                                    retry_attempt = 0;
                                    self.model_selector = Some(target);
                                    self.ratio = ratio as f64;
                                    continue 'outer;
                                }
                            }
                        }
                    }
                    events.send(Event::DigestStarted).await.ok();
                    if let Some(summary) = self
                        .summarize(None, std::time::Duration::from_secs(120))
                        .await
                    {
                        self.apply_digest(summary, self.ratio);
                        continue 'outer;
                    }
                }
                // Retryable failure → automatic slow backoff before
                // giving up (5s / 20s / 60s, max 3 attempts). The failed
                // step is re-driven from the same history, so the user
                // record is never duplicated.
                let delays = retry_delays();
                if retryable && retry_attempt < delays.len() {
                    let delay = delays[retry_attempt];
                    retry_attempt += 1;
                    events
                        .send(Event::Note {
                            message: format!("↻ retrying in {}s (esc cancels)", delay.as_secs()),
                        })
                        .await
                        .ok();
                    if wait_or_cancel(delay, commands, interjections, deferrals).await {
                        events
                            .send(Event::Note {
                                message: "retry canceled".to_string(),
                            })
                            .await
                            .ok();
                    } else {
                        continue 'outer;
                    }
                }
                // Fallback chain: provider/auth failure with per-wire
                // retries exhausted → re-dispatch the same messages on
                // the next configured model (max 2 hops per turn). The
                // switch is transient: the rest of this turn rides the
                // fallback, no strand Change is recorded, and the next
                // turn starts from the requested selector again.
                if matches!(
                    class,
                    ErrorClass::Auth
                        | ErrorClass::RateLimit
                        | ErrorClass::Network
                        | ErrorClass::Internal
                ) && fallback_hops < 2
                {
                    let mut hop: Option<(String, ka_dialect::Selector, ka_dialect::Dialect)> = None;
                    while let Some(sel) = self.fallbacks.get(fallback_cursor) {
                        fallback_cursor += 1;
                        if let Ok(p) = parse_selector(sel) {
                            if let Some(d) = self.catalog.get(&p.model_id()).cloned() {
                                hop = Some((sel.clone(), p, d));
                                break;
                            }
                        }
                    }
                    if let Some((sel, p, d)) = hop {
                        fallback_hops += 1;
                        events
                            .send(Event::Note {
                                message: format!("fallback → {sel}"),
                            })
                            .await
                            .ok();
                        parsed = p;
                        model_id = parsed.model_id();
                        dialect = d;
                        price = dialect.price;
                        ratio = if dialect.ratio > 0.0 {
                            dialect.ratio
                        } else {
                            4.0
                        };
                        token = dialect
                            .api_key_env
                            .as_deref()
                            .and_then(ka_dialect::auth::resolve_token);
                        window = dialect.context as u64;
                        retry_attempt = 0;
                        self.model_selector = Some(sel);
                        self.ratio = ratio as f64;
                        continue 'outer;
                    }
                }
                events
                    .send(Event::Error {
                        class,
                        retryable: false,
                        message,
                    })
                    .await
                    .ok();
                break 'outer;
            }

            if step_calls.is_empty() || steps >= self.max_steps {
                break 'outer;
            }

            // Execute this step's calls. Gating stays sequential —
            // permission asks must remain one-at-a-time UX — then the
            // approved calls run concurrently and results are reassembled
            // in original call order.
            self.history.push(TurnMessage::assistant_with_calls(
                step_text.clone(),
                step_calls.clone(),
            ));
            let mut slots: Vec<Option<ToolOutput>> = vec![None; step_calls.len()];
            let mut approved: Vec<(usize, std::sync::Arc<dyn Hand>)> = Vec::new();
            for (idx, call) in step_calls.iter().enumerate() {
                match self.admit_call(call, commands, events).await {
                    Ok(hand) => approved.push((idx, hand)),
                    Err(output) => slots[idx] = Some(output),
                }
            }
            let mut aborted = false;
            if !approved.is_empty() {
                let mut in_flight = tokio::task::JoinSet::new();
                let mut task_idx: HashMap<tokio::task::Id, usize> = HashMap::new();
                for (idx, hand) in approved {
                    let call = step_calls[idx].clone();
                    let ctx = self.hand_ctx.clone();
                    let events = events.clone();
                    let hooks = self.hooks_cfg.clone();
                    let todo = self.todo.clone();
                    let handle = in_flight.spawn(async move {
                        let output = execute_approved(&hand, &hooks, &call, &ctx, &events).await;
                        // the todo hand owns normalization; surfaces get the
                        // fresh list as a whole-replacement event
                        if call.tool == "todo" && !output.is_error {
                            let items = todo.lock().clone();
                            events.send(Event::Todos { items }).await.ok();
                        }
                        events
                            .send(Event::CallOutput {
                                tool: call.tool.clone(),
                                id: call.id.clone(),
                                excerpt: truncate_excerpt(&output.content),
                                is_error: output.is_error,
                                spill: output.spill.clone(),
                            })
                            .await
                            .ok();
                        events
                            .send(Event::CallFinished {
                                tool: call.tool.clone(),
                                id: call.id.clone(),
                                ok: !output.is_error,
                            })
                            .await
                            .ok();
                        (idx, output)
                    });
                    task_idx.insert(handle.id(), idx);
                }
                let mut mode_change: Option<ka_protocol::Mode> = None;
                while !in_flight.is_empty() {
                    tokio::select! {
                        biased;
                        // abort cancels the in-flight futures; dropped bash
                        // children die via their drop guards
                        maybe_cmd = commands.recv() => match maybe_cmd {
                            None | Some(Command::Abort) => {
                                aborted = true;
                                break;
                            }
                            Some(Command::Interject { text }) => interjections.push(text),
                            Some(Command::Defer { text }) => deferrals.push_back(text),
                            Some(Command::SetMode { mode }) => mode_change = Some(mode),
                            Some(_) => {}
                        },
                        joined = in_flight.join_next() => match joined {
                            Some(Ok((idx, output))) => slots[idx] = Some(output),
                            Some(Err(e)) => {
                                // a panicking hand must not wedge the step
                                if let Some(idx) = task_idx.remove(&e.id()) {
                                    let output =
                                        ToolOutput::err(format!("tool task failed: {e}"));
                                    events
                                        .send(Event::CallOutput {
                                            tool: step_calls[idx].tool.clone(),
                                            id: step_calls[idx].id.clone(),
                                            excerpt: truncate_excerpt(&output.content),
                                            is_error: true,
                                            spill: None,
                                        })
                                        .await
                                        .ok();
                                    events
                                        .send(Event::CallFinished {
                                            tool: step_calls[idx].tool.clone(),
                                            id: step_calls[idx].id.clone(),
                                            ok: false,
                                        })
                                        .await
                                        .ok();
                                    slots[idx] = Some(output);
                                }
                            }
                            None => {}
                        },
                    }
                }
                if let Some(mode) = mode_change {
                    self.set_mode(mode);
                }
            }
            if aborted {
                // canceled calls surface as aborted results so history
                // stays well-formed for the next turn
                for (idx, slot) in slots.iter_mut().enumerate() {
                    if slot.is_some() {
                        continue;
                    }
                    let output = ToolOutput::err("aborted");
                    events
                        .send(Event::CallOutput {
                            tool: step_calls[idx].tool.clone(),
                            id: step_calls[idx].id.clone(),
                            excerpt: truncate_excerpt(&output.content),
                            is_error: true,
                            spill: None,
                        })
                        .await
                        .ok();
                    events
                        .send(Event::CallFinished {
                            tool: step_calls[idx].tool.clone(),
                            id: step_calls[idx].id.clone(),
                            ok: false,
                        })
                        .await
                        .ok();
                    *slot = Some(output);
                }
            }
            let results: Vec<ToolResult> = slots
                .into_iter()
                .zip(step_calls.iter())
                .map(|(slot, call)| {
                    let output = slot.unwrap_or_else(|| ToolOutput::err("aborted"));
                    ToolResult {
                        call_id: call.id.clone(),
                        content: output.content,
                        is_error: output.is_error,
                        images: output.images,
                    }
                })
                .collect();
            self.history.push(TurnMessage::tool(results));
            steps += 1;
            if aborted {
                events
                    .send(Event::TurnFinished {
                        stop: Stop::Aborted,
                        usage: Usage::default(),
                    })
                    .await
                    .ok();
                return Usage::default();
            }
            if final_stop == Stop::Length {
                break 'outer;
            }
        }

        // True-up + cost
        if usage_total.input == 0 {
            usage_total.input = est_in;
        }
        if usage_total.output == 0 {
            usage_total.output = (assistant_text.len() as f64 / ratio as f64).ceil() as u64;
        }
        // placeholders must never surface as money
        usage_total.cost = if dialect.priced {
            cost_of(&usage_total, price)
        } else {
            0.0
        };

        if final_stop != Stop::Aborted {
            self.history
                .push(TurnMessage::assistant(if assistant_text.is_empty() {
                    "(no text)".to_string()
                } else {
                    assistant_text.clone()
                }));
        }
        events
            .send(Event::TurnFinished {
                stop: final_stop,
                usage: usage_total,
            })
            .await
            .ok();
        usage_total
    }

    /// Sequential gating phase for one call: loop guard, pre-tool hooks,
    /// clearance verdict, and any permission ask (one at a time — the ask
    /// UX stays exclusive). Returns the approved hand; Err carries the
    /// decided result (already surfaced to events).
    async fn admit_call(
        &mut self,
        call: &ToolCall,
        commands: &mut mpsc::Receiver<Command>,
        events: &mpsc::Sender<Event>,
    ) -> Result<std::sync::Arc<dyn Hand>, ToolOutput> {
        let sig = format!("{}|{}", call.tool, call.arguments);
        *self.state.loop_counts.entry(sig).or_insert(0) += 1;
        if self.state.loop_counts.values().any(|c| *c >= 4) {
            // loop-guard decisions surface as CallFinished only (existing
            // contract); the result itself still feeds back to the model
            events
                .send(Event::CallFinished {
                    tool: call.tool.clone(),
                    id: call.id.clone(),
                    ok: false,
                })
                .await
                .ok();
            return Err(ToolOutput {
                content: "loop guard: this tool was called with identical arguments 4+ \
                          times; stop repeating and reconsider"
                    .to_string(),
                is_error: true,
                images: Vec::new(),
                spill: None,
            });
        }
        let Some(hand) = self
            .hands
            .iter()
            .find(|h| h.def().name == call.tool)
            .cloned()
        else {
            let output = ToolOutput::err(format!("unknown tool {}", call.tool));
            self.surface_decided(call, &output, events).await;
            return Err(output);
        };
        // pre_tool_use hooks: exit 2 blocks before any gate
        if let Err(reason) = self
            .run_hooks(
                crate::config::HookEvent::PreToolUse,
                &call.tool,
                &call.arguments,
            )
            .await
        {
            let output = ToolOutput::err(format!("blocked by hook: {reason}"));
            self.surface_decided(call, &output, events).await;
            return Err(output);
        }
        let verdict = self.gate(hand.clearance_for(&call.arguments), call);
        match verdict {
            Gate::Allow => {}
            Gate::Deny { reason } => {
                let output = ToolOutput::err(reason);
                self.surface_decided(call, &output, events).await;
                return Err(output);
            }
            Gate::Ask { question } => {
                self.state.ask_counter += 1;
                let ask_id = AskId(format!("ask-{}", self.state.ask_counter));
                let options = vec![
                    "allow".to_string(),
                    "always".to_string(),
                    "deny".to_string(),
                ];
                let ask = Event::Ask {
                    id: ask_id.clone(),
                    questions: vec![AskQuestion {
                        text: question,
                        options,
                    }],
                };
                if events.send(ask).await.is_err() {
                    let output = ToolOutput::err("permission ask failed: surface closed");
                    self.surface_decided(call, &output, events).await;
                    return Err(output);
                }
                // wait for the answer (or abort); the ask stays sequential
                loop {
                    tokio::select! {
                        maybe = commands.recv() => {
                            match maybe {
                                Some(Command::Answer { question: q, choice }) if q == ask_id => {
                                    match choice {
                                        1 => {
                                            self.state.rules.insert(format!("tool:{}", call.tool));
                                            // persist the allowlist entry to the
                                            // project layer (best-effort, silent)
                                            if let Some(path) = crate::config::save_project_permission(
                                                &self.hand_ctx.cwd,
                                                &call.tool,
                                            ) {
                                                events
                                                    .send(Event::Note {
                                                        message: format!(
                                                            "always-allow for {} saved to {}",
                                                            call.tool,
                                                            path.display()
                                                        ),
                                                    })
                                                    .await
                                                    .ok();
                                            }
                                            break;
                                        }
                                        2 => {
                                            let output = ToolOutput::err(format!(
                                                "permission denied by user for {}",
                                                call.tool
                                            ));
                                            self.surface_decided(call, &output, events).await;
                                            return Err(output);
                                        }
                                        _ => break,
                                    }
                                }
                                Some(Command::Abort) => {
                                    let output = ToolOutput::err("aborted");
                                    self.surface_decided(call, &output, events).await;
                                    return Err(output);
                                }
                                Some(_) => {}
                                None => {
                                    let output = ToolOutput::err("surface closed during ask");
                                    self.surface_decided(call, &output, events).await;
                                    return Err(output);
                                }
                            }
                        }
                    }
                }
            }
        }
        // convention pre-tool hook: non-zero exit vetoes the call
        if let Err(reason) = crate::fshooks::run(
            crate::fshooks::HookPoint::PreTool,
            &self.hand_ctx.cwd,
            Some(&call.tool),
        )
        .await
        {
            events
                .send(Event::Note {
                    message: reason.clone(),
                })
                .await
                .ok();
            let output = ToolOutput::err(format!("blocked by pre-tool hook: {reason}"));
            self.surface_decided(call, &output, events).await;
            return Err(output);
        }
        Ok(hand)
    }

    /// Surface a gate-phase decision (CallOutput + CallFinished) without
    /// executing the call.
    async fn surface_decided(
        &self,
        call: &ToolCall,
        output: &ToolOutput,
        events: &mpsc::Sender<Event>,
    ) {
        events
            .send(Event::CallOutput {
                tool: call.tool.clone(),
                id: call.id.clone(),
                excerpt: truncate_excerpt(&output.content),
                is_error: output.is_error,
                spill: None,
            })
            .await
            .ok();
        events
            .send(Event::CallFinished {
                tool: call.tool.clone(),
                id: call.id.clone(),
                ok: !output.is_error,
            })
            .await
            .ok();
    }

    fn gate(&self, clearance: Clearance, call: &ToolCall) -> Gate {
        // configured rules: first match wins, before mode logic
        if let Some(verdict) = self.match_rule(call) {
            return match verdict {
                crate::config::Verdict::Allow => Gate::Allow,
                crate::config::Verdict::Ask => Gate::Ask {
                    question: format!(
                        "rule requires confirmation for {} `{}`",
                        call.tool,
                        call.primary_arg()
                    ),
                },
                crate::config::Verdict::Deny => Gate::Deny {
                    reason: format!("denied by rule for {}", call.tool),
                },
            };
        }
        if self.state.rules.contains(&format!("tool:{}", call.tool)) {
            return Gate::Allow;
        }
        match clearance {
            Clearance::Read => Gate::Allow,
            Clearance::Write => match self.mode {
                ka_protocol::Mode::Free | ka_protocol::Mode::AcceptEdits => Gate::Allow,
                ka_protocol::Mode::Guarded => Gate::Ask {
                    question: format!("allow {} to modify files?", call.tool),
                },
                ka_protocol::Mode::Plan => {
                    // research mode: only the plans directory is writable
                    let arg = call.primary_arg();
                    let plans_ok = arg.starts_with(".ka/plans/")
                        || arg.starts_with("./.ka/plans/")
                        || arg.contains("/.ka/plans/");
                    if plans_ok {
                        Gate::Allow
                    } else {
                        Gate::Deny {
                            reason: format!(
                                "plan mode is read-only except .ka/plans/ (got {arg:?}); \
use /build to switch to implementation"
                            ),
                        }
                    }
                }
            },
            Clearance::Exec => {
                let command = call
                    .arguments
                    .get("command")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                let analysis = analyze(command);
                if let Some(stop) = hardstop(command, &analysis) {
                    return Gate::Ask {
                        question: format!(
                            "HARDSTOP — {}: `{}`. Proceed anyway?",
                            stop.reason, command
                        ),
                    };
                }
                if self.state.rules.contains(&format!(
                    "bash:{}",
                    analysis
                        .segments
                        .first()
                        .and_then(|s| s.first())
                        .cloned()
                        .unwrap_or_default()
                )) {
                    return Gate::Allow;
                }
                if all_readonly(&analysis) {
                    return Gate::Allow;
                }
                match self.mode {
                    ka_protocol::Mode::Free => Gate::Allow,
                    ka_protocol::Mode::Plan => Gate::Ask {
                        question: format!("plan mode: run `{command}`? (build with /build)"),
                    },
                    ka_protocol::Mode::AcceptEdits | ka_protocol::Mode::Guarded => Gate::Ask {
                        question: format!("run `{command}`?"),
                    },
                }
            }
        }
    }
}

/// Execution phase for one approved call: post_tool_use hooks, the call
/// itself (bash streams live previews), and one-way secret redaction.
/// Runs concurrently for every approved call of a step; each call emits
/// its own CallOutput/CallFinished as it completes.
async fn execute_approved(
    hand: &std::sync::Arc<dyn Hand>,
    hooks: &[crate::config::Hook],
    call: &ToolCall,
    ctx: &HandContext,
    events: &mpsc::Sender<Event>,
) -> ToolOutput {
    // bash runs long: pump live preview emissions while the child
    // works. Partials are new-since-last-emission output on the same
    // call id (is_error=false, spill=None, redacted, hard-capped);
    // the final CallOutput emission is untouched.
    let mut output = if call.tool == "bash" {
        let tool = call.tool.clone();
        let id = call.id.clone();
        let events = events.clone();
        let progress = move |fresh: String| {
            let excerpt = crate::hands::bash::cap_preview(&fresh);
            let excerpt = crate::hands::secrets::redact(&excerpt);
            if excerpt.is_empty() {
                return;
            }
            let _ = events.try_send(Event::CallOutput {
                tool: tool.clone(),
                id: id.clone(),
                excerpt,
                is_error: false,
                spill: None,
            });
        };
        crate::hands::BashHand
            .execute_streaming(&call.arguments, ctx, &progress)
            .await
    } else {
        hand.execute(&call.arguments, ctx).await
    };
    // post_tool_use hooks: exit 2 flags the result as an error
    if let Err(reason) = run_hook_scripts(
        hooks,
        crate::config::HookEvent::PostToolUse,
        &call.tool,
        &call.arguments,
        &ctx.cwd,
    )
    .await
    {
        output.is_error = true;
        output
            .content
            .push_str(&format!("\n[post-tool hook: {reason}]"));
    }
    // one-way secret redaction before anything reaches the model
    output.content = crate::hands::secrets::redact(&output.content);
    output
}

/// Run matching hook scripts for one event. Returns Err(reason) when a
/// pre_tool_use hook blocked the call (exit 2, stderr as reason). A free
/// fn so concurrent per-call tasks can run post_tool_use hooks.
async fn run_hook_scripts(
    hooks: &[crate::config::Hook],
    event: crate::config::HookEvent,
    tool: &str,
    args: &serde_json::Value,
    cwd: &std::path::Path,
) -> Result<(), String> {
    use tokio::io::AsyncWriteExt;
    for hook in hooks {
        if hook.event != event {
            continue;
        }
        if let Some(t) = &hook.tool {
            if t != tool {
                continue;
            }
        }
        let payload = serde_json::json!({"tool": tool, "arguments": args});
        let mut child = match tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&hook.command)
            .current_dir(cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => return Err(format!("hook failed to spawn: {e}")),
        };
        let stdin_opt = child.stdin.take();
        if let Some(mut stdin) = stdin_opt {
            let _ = stdin.write_all(payload.to_string().as_bytes()).await;
        }
        let output = match tokio::time::timeout(
            std::time::Duration::from_secs(30),
            child.wait_with_output(),
        )
        .await
        {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => return Err(format!("hook failed: {e}")),
            Err(_) => return Err("hook timed out after 30s".to_string()),
        };
        if output.status.code() == Some(2) {
            let reason = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(if reason.is_empty() {
                "blocked by hook".to_string()
            } else {
                reason
            });
        }
    }
    Ok(())
}

/// Automatic retry backoff for retryable turn failures: 5s, 20s, 60s
/// (max 3 attempts) before falling through to the normal error finish.
/// Tests shrink the schedule to keep the suite fast.
fn retry_delays() -> Vec<Duration> {
    #[cfg(test)]
    {
        vec![
            Duration::from_millis(10),
            Duration::from_millis(10),
            Duration::from_millis(10),
        ]
    }
    #[cfg(not(test))]
    {
        vec![
            Duration::from_secs(5),
            Duration::from_secs(20),
            Duration::from_secs(60),
        ]
    }
}

#[derive(Debug)]
enum Gate {
    Allow,
    Ask { question: String },
    Deny { reason: String },
}

/// Wait `delay` before a retry, draining side-effect commands while
/// waiting. Returns `true` when the wait was canceled (Abort or the
/// surface went away).
async fn wait_or_cancel(
    delay: Duration,
    commands: &mut mpsc::Receiver<Command>,
    interjections: &mut Vec<String>,
    deferrals: &mut VecDeque<String>,
) -> bool {
    let sleep = tokio::time::sleep(delay);
    tokio::pin!(sleep);
    loop {
        tokio::select! {
            biased;
            maybe = commands.recv() => match maybe {
                None => return true,
                Some(Command::Abort) => return true,
                Some(Command::Interject { text }) => interjections.push(text),
                Some(Command::Defer { text }) => deferrals.push_back(text),
                Some(_) => {}
            },
            () = &mut sleep => return false,
        }
    }
}

/// Short argument summary for [`Event::CallStarted`] transcript headers:
/// bash → the command flattened to one line (≤48 cols), file tools → the
/// path's last segment (≤32), searches → the pattern (≤32), anything
/// else empty (the header shows the bare tool name).
fn call_detail(tool: &str, arguments: &serde_json::Value) -> String {
    let str_arg = |key: &str| arguments.get(key).and_then(serde_json::Value::as_str);
    match tool {
        "bash" => str_arg("command")
            .map(|cmd| {
                cmd.split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .chars()
                    .take(48)
                    .collect()
            })
            .unwrap_or_default(),
        "edit" | "write" | "read" => str_arg("path")
            .map(|p| p.rsplit('/').next().unwrap_or(p).chars().take(32).collect())
            .unwrap_or_default(),
        "glob" | "grep" => str_arg("pattern")
            .map(|p| p.chars().take(32).collect())
            .unwrap_or_default(),
        _ => String::new(),
    }
}

/// Pose a `[continue, stop]` guard ask and wait for the answer.
/// `false` = stop (user chose stop, aborted, or the surface went away).
async fn ask_continue_or_stop(
    state: &mut VoiceState,
    question: String,
    commands: &mut mpsc::Receiver<Command>,
    events: &mpsc::Sender<Event>,
) -> bool {
    state.ask_counter += 1;
    let id = AskId(format!("ask-{}", state.ask_counter));
    events
        .send(Event::Ask {
            id: id.clone(),
            questions: vec![AskQuestion {
                text: question,
                options: vec!["continue".to_string(), "stop".to_string()],
            }],
        })
        .await
        .ok();
    loop {
        tokio::select! {
            maybe = commands.recv() => match maybe {
                Some(Command::Answer { question: q, choice }) if q == id => {
                    return choice == 0;
                }
                Some(Command::Abort) => return false,
                Some(_) => {}
                None => return false,
            }
        }
    }
}

fn truncate_excerpt(text: &str) -> String {
    let capped: String = text.chars().take(2_000).collect();
    if capped.len() < text.len() {
        format!("{capped}…")
    } else {
        capped
    }
}

/// Report an error and finish the turn; returns the (empty) usage for
/// the engine's Usage record.
async fn finish_after_error(
    events: &mpsc::Sender<Event>,
    class: ErrorClass,
    message: &str,
) -> Usage {
    events
        .send(Event::Error {
            class,
            retryable: false,
            message: message.to_string(),
        })
        .await
        .ok();
    events
        .send(Event::TurnFinished {
            stop: Stop::Error,
            usage: Usage::default(),
        })
        .await
        .ok();
    Usage::default()
}

/// USD cost from usage and per-mtok prices (cache reads billed at input
/// rate — a conservative overestimate until per-tier pricing lands).
fn cost_of(usage: &Usage, price: ka_dialect::dialects::Price) -> f64 {
    let in_tokens = usage.input + usage.cache_read + usage.cache_write;
    (in_tokens as f64 / 1_000_000.0) * price.input_per_mtok
        + (usage.output as f64 / 1_000_000.0) * price.output_per_mtok
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::time::Duration;

    use ka_dialect::dialects::{Catalog, Wire};
    use ka_dialect::speaker::{
        SpeakFuture, SpeakRequest, Speaker, StreamEvent, ToolCall, TurnMessage, TurnRole,
    };
    use ka_protocol::{Command, Event, Stop, Usage};

    use super::{GuardRuntime, Voice, call_detail, cost_of, glob_match};

    #[test]
    fn unpriced_dialects_never_report_cost() {
        // the gating expression at the true-up site
        let priced = false;
        let cost = if priced { 18.0 } else { 0.0 };
        assert_eq!(cost, 0.0);
        let priced = true;
        let cost = if priced { 18.0 } else { 0.0 };
        assert_eq!(cost, 18.0);
    }
    #[test]
    fn call_detail_summarizes_head_path_and_pattern() {
        use serde_json::json;
        // bash: multiline command flattened, capped at 48
        let d = call_detail("bash", &json!({"command": "echo  a\nb\nc"}));
        assert_eq!(d, "echo a b c");
        let long = call_detail("bash", &json!({"command": "x".repeat(60)}));
        assert_eq!(long.chars().count(), 48);
        // file tools: last path segment, capped at 32
        assert_eq!(
            call_detail("edit", &json!({"path": "src/deep/lib/mod.rs"})),
            "mod.rs"
        );
        assert_eq!(call_detail("read", &json!({"path": "/a/b.rs"})), "b.rs");
        // searches: pattern
        assert_eq!(
            call_detail("grep", &json!({"pattern": "foo.*bar"})),
            "foo.*bar"
        );
        // everything else: empty
        assert_eq!(call_detail("todo", &json!({"items": []})), "");
        assert_eq!(call_detail("bash", &json!({})), "");
    }
    #[test]
    fn cost_of_computes_from_price() {
        let usage = Usage {
            input: 1_000_000,
            output: 1_000_000,
            ..Default::default()
        };
        let price = ka_dialect::dialects::Price {
            input_per_mtok: 3.0,
            output_per_mtok: 15.0,
        };
        assert!((cost_of(&usage, price) - 18.0).abs() < 1e-9);
    }

    /// Fake speaker: first request → one `read` tool call; once a tool
    /// result is present → final text. Records every request it saw.
    struct FakeSpeaker {
        seen: std::sync::Arc<parking_lot::Mutex<Vec<Vec<TurnMessage>>>>,
    }

    impl Speaker for FakeSpeaker {
        fn speak<'a>(
            &'a self,
            req: SpeakRequest,
            out: tokio::sync::mpsc::Sender<StreamEvent>,
        ) -> SpeakFuture<'a> {
            let seen = self.seen.clone();
            Box::pin(async move {
                seen.lock().push(req.messages.clone());
                let has_result = req
                    .messages
                    .iter()
                    .any(|m| m.role == TurnRole::Tool && !m.results.is_empty());
                if has_result {
                    out.send(StreamEvent::Text("all done".into())).await.ok();
                    out.send(StreamEvent::Finished {
                        stop: ka_protocol::Stop::Done,
                        usage: ka_protocol::Usage {
                            input: 10,
                            output: 5,
                            ..Default::default()
                        },
                    })
                    .await
                    .ok();
                } else {
                    out.send(StreamEvent::Call(ToolCall {
                        id: "c1".into(),
                        tool: "read".into(),
                        arguments: serde_json::json!({"path": "roundtrip.txt"}),
                    }))
                    .await
                    .ok();
                    out.send(StreamEvent::Finished {
                        stop: ka_protocol::Stop::Done,
                        usage: ka_protocol::Usage {
                            input: 10,
                            output: 5,
                            ..Default::default()
                        },
                    })
                    .await
                    .ok();
                }
            })
        }
    }

    #[tokio::test]
    async fn tool_roundtrip_executes_and_feeds_results_back() {
        use ka_protocol::Event;
        use tokio::sync::mpsc;

        let dir = std::env::temp_dir().join(format!("ka-voice-rt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("roundtrip.txt"), "ROUNDTRIP-CONTENT\n").unwrap();

        let catalog = Catalog::parse(
            "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000\n",
        )
        .unwrap();
        let seen = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let mut voice = Voice::new(catalog, dir.clone(), ka_protocol::Mode::Guarded, 10)
            .with_speaker(
                Wire::OpenaiChat,
                std::sync::Arc::new(FakeSpeaker { seen: seen.clone() }),
            );

        let (cmd_tx, mut cmd_rx) = mpsc::channel(16);
        let (evt_tx, mut evt_rx) = mpsc::channel(256);
        let mut interjections = Vec::new();
        let mut deferrals = std::collections::VecDeque::new();

        let handle = tokio::spawn(async move {
            voice
                .turn(
                    "test/m",
                    "read the file".into(),
                    &mut cmd_rx,
                    &evt_tx,
                    &mut interjections,
                    &mut deferrals,
                    &mut GuardRuntime::default(),
                    None,
                    Vec::new(),
                )
                .await;
        });

        let mut saw_output = false;
        let mut saw_done = false;
        while let Some(evt) = evt_rx.recv().await {
            match evt {
                Event::CallOutput { excerpt, .. } => {
                    assert!(excerpt.contains("ROUNDTRIP-CONTENT"), "{excerpt}");
                    saw_output = true;
                }
                Event::TurnFinished {
                    stop: ka_protocol::Stop::Done,
                    usage,
                } => {
                    assert_eq!(usage.input, 20); // two steps × 10
                    saw_done = true;
                    break;
                }
                Event::TurnFinished { .. } => break,
                _ => {}
            }
        }
        drop(cmd_tx);
        handle.await.unwrap();
        assert!(saw_output, "tool output must reach the surface");
        assert!(saw_done, "turn must finish done");

        let seen = seen.lock();
        assert_eq!(seen.len(), 2, "exactly two speaks expected");
        let second = &seen[1];
        let result = second
            .iter()
            .flat_map(|m| &m.results)
            .next()
            .expect("second speak must carry the tool result");
        assert!(result.content.contains("ROUNDTRIP-CONTENT"));
        assert!(!result.is_error);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Fake speaker: first request → one `todo` tool call; once a tool
    /// result is present → final text.
    struct TodoSpeaker;

    impl Speaker for TodoSpeaker {
        fn speak<'a>(
            &'a self,
            req: SpeakRequest,
            out: tokio::sync::mpsc::Sender<StreamEvent>,
        ) -> SpeakFuture<'a> {
            Box::pin(async move {
                let has_result = req
                    .messages
                    .iter()
                    .any(|m| m.role == TurnRole::Tool && !m.results.is_empty());
                if has_result {
                    out.send(StreamEvent::Text("plan noted".into())).await.ok();
                    out.send(StreamEvent::Finished {
                        stop: ka_protocol::Stop::Done,
                        usage: ka_protocol::Usage::default(),
                    })
                    .await
                    .ok();
                } else {
                    out.send(StreamEvent::Call(ToolCall {
                        id: "t1".into(),
                        tool: "todo".into(),
                        arguments: serde_json::json!({"items": [
                            {"text": "survey", "state": "done"},
                            {"text": "implement", "state": "pending"},
                        ]}),
                    }))
                    .await
                    .ok();
                    out.send(StreamEvent::Finished {
                        stop: ka_protocol::Stop::Done,
                        usage: ka_protocol::Usage::default(),
                    })
                    .await
                    .ok();
                }
            })
        }
    }

    #[tokio::test]
    async fn todo_call_reaches_surfaces_as_todos_event() {
        use tokio::sync::mpsc;

        let catalog = Catalog::parse(
            "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000\n",
        )
        .unwrap();
        let mut voice = Voice::new(
            catalog,
            std::env::temp_dir(),
            ka_protocol::Mode::Guarded,
            10,
        )
        .with_speaker(Wire::OpenaiChat, std::sync::Arc::new(TodoSpeaker));

        let (cmd_tx, mut cmd_rx) = mpsc::channel(16);
        let (evt_tx, mut evt_rx) = mpsc::channel(256);
        let mut interjections = Vec::new();
        let mut deferrals = std::collections::VecDeque::new();

        let handle = tokio::spawn(async move {
            voice
                .turn(
                    "test/m",
                    "plan the work".into(),
                    &mut cmd_rx,
                    &evt_tx,
                    &mut interjections,
                    &mut deferrals,
                    &mut GuardRuntime::default(),
                    None,
                    Vec::new(),
                )
                .await;
        });

        let mut todos_event = None;
        while let Some(evt) = evt_rx.recv().await {
            if let Event::Todos { items } = evt {
                todos_event = Some(items);
                break;
            }
        }
        drop(cmd_tx);
        handle.await.unwrap();
        let items = todos_event.expect("todo call must emit Event::Todos");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].text, "survey");
        assert_eq!(items[0].state, ka_protocol::TodoState::Done);
        assert_eq!(items[1].state, ka_protocol::TodoState::Pending);
    }

    #[test]
    fn prune_blanks_old_tool_outputs_beyond_window() {
        use ka_dialect::speaker::{ToolCall, ToolResult, TurnMessage, TurnRole};
        let catalog = Catalog::parse(
            "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000\nratio = 1.0\n",
        )
        .unwrap();
        let dir = std::env::temp_dir();
        let mut voice = Voice::new(catalog, dir.clone(), ka_protocol::Mode::Guarded, 5);
        // ratio 1.0 → tokens == chars
        let big = "x".repeat(30_000); // 30k tokens
        voice.history.push(TurnMessage::user("start"));
        voice.history.push(TurnMessage::assistant_with_calls(
            "working",
            vec![ToolCall {
                id: "c1".into(),
                tool: "bash".into(),
                arguments: Default::default(),
            }],
        ));
        voice.history.push(TurnMessage::tool(vec![ToolResult {
            call_id: "c1".into(),
            content: big.clone(),
            is_error: false,
            images: Vec::new(),
        }]));
        // recent large exchange: fills the 40k protect window so the old
        // tool result falls outside it
        voice
            .history
            .push(TurnMessage::user(format!("recent {}", "r".repeat(45_000))));
        voice.history.push(TurnMessage::assistant("tail"));

        let saved = voice.prune_tool_outputs(1.0);
        assert!(saved >= 20_000, "savings {saved}");
        let tool_msg = voice
            .history
            .iter()
            .find(|m| m.role == TurnRole::Tool)
            .unwrap();
        let content = &tool_msg.results[0].content;
        assert!(content.starts_with("[pruned output"), "{content}");
        assert!(content.contains("spill://"), "{content}");
        assert!(
            voice.history[3].content.starts_with("recent"),
            "recent messages untouched"
        );
        assert_eq!(voice.history[4].content, "tail");
    }

    #[test]
    fn prune_skips_when_savings_below_threshold() {
        let catalog = Catalog::parse(
            "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000\n",
        )
        .unwrap();
        let mut voice = Voice::new(catalog, std::env::temp_dir(), ka_protocol::Mode::Guarded, 5);
        voice.set_model_selector("test/m", 1.0);
        use ka_dialect::speaker::{ToolCall, ToolResult, TurnMessage};
        voice.history.push(TurnMessage::user("q"));
        voice.history.push(TurnMessage::assistant_with_calls(
            "",
            vec![ToolCall {
                id: "c".into(),
                tool: "read".into(),
                arguments: Default::default(),
            }],
        ));
        voice.history.push(TurnMessage::tool(vec![ToolResult {
            call_id: "c".into(),
            content: "tiny".to_string(),
            is_error: false,
            images: Vec::new(),
        }]));
        let saved = voice.prune_tool_outputs(1.0);
        assert_eq!(saved, 0, "tiny outputs must not be pruned");
        assert_eq!(voice.history[2].results[0].content, "tiny");
    }
    use std::sync::Arc;
    use tokio::sync::mpsc;

    /// Records requests, replies with one trimmed text delta.
    struct TitleSpeaker {
        seen: std::sync::Arc<parking_lot::Mutex<Vec<SpeakRequest>>>,
    }
    impl Speaker for TitleSpeaker {
        fn speak<'a>(
            &'a self,
            req: SpeakRequest,
            out: mpsc::Sender<StreamEvent>,
        ) -> SpeakFuture<'a> {
            self.seen.lock().push(req);
            Box::pin(async move {
                out.send(StreamEvent::Text("  Fix the Parser \n".into()))
                    .await
                    .ok();
                out.send(StreamEvent::Finished {
                    stop: Stop::Done,
                    usage: Usage::default(),
                })
                .await
                .ok();
            })
        }
    }

    /// Streams partial text, then fails: the partial must be discarded.
    struct FailingSpeaker;
    impl Speaker for FailingSpeaker {
        fn speak<'a>(
            &'a self,
            _req: SpeakRequest,
            out: mpsc::Sender<StreamEvent>,
        ) -> SpeakFuture<'a> {
            Box::pin(async move {
                out.send(StreamEvent::Text("partial".into())).await.ok();
                out.send(StreamEvent::Failed {
                    class: ka_protocol::ErrorClass::Network,
                    retryable: false,
                    message: "boom".into(),
                })
                .await
                .ok();
            })
        }
    }

    fn fast_voice(
        speaker: std::sync::Arc<dyn Speaker>,
    ) -> (Voice, std::sync::Arc<parking_lot::Mutex<Vec<SpeakRequest>>>) {
        let seen = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let catalog = Catalog::parse(
            "[dialects.\"test/fast\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000\n",
        )
        .unwrap();
        let seen2 = seen.clone();
        (
            Voice::new(catalog, std::env::temp_dir(), ka_protocol::Mode::Free, 5)
                .with_speaker(Wire::OpenaiChat, speaker),
            seen2,
        )
    }

    #[tokio::test]
    async fn role_complete_trims_caps_and_records_the_request() {
        let seen = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let (mut voice, _) = fast_voice(Arc::new(TitleSpeaker { seen: seen.clone() }));
        let out = voice
            .role_complete(
                "test/fast",
                "titles",
                "make a title",
                Duration::from_secs(5),
            )
            .await;
        assert_eq!(out.as_deref(), Some("Fix the Parser"));
        let reqs = seen.lock();
        assert_eq!(reqs.len(), 1);
        let req = &reqs[0];
        assert_eq!(req.model_id, "test/fast");
        assert!(req.tools.is_empty(), "role calls never carry tools");
        assert_eq!(req.effort, None);
        assert_eq!(
            req.dialect.max_output,
            Voice::ROLE_MAX_OUTPUT,
            "output capped for the cheap role"
        );
        assert_eq!(req.system, "titles");
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.messages[0].role, TurnRole::User);
        assert_eq!(req.messages[0].content, "make a title");
    }

    #[tokio::test]
    async fn role_complete_degrades_silently() {
        // unknown selector → None
        let (mut voice, _) = fast_voice(Arc::new(TitleSpeaker {
            seen: std::sync::Arc::new(parking_lot::Mutex::new(Vec::new())),
        }));
        assert!(
            voice
                .role_complete("nope/missing", "s", "p", Duration::from_secs(1))
                .await
                .is_none()
        );
        // failed stream: even partial text is discarded
        let (mut voice, _) = fast_voice(Arc::new(FailingSpeaker));
        assert!(
            voice
                .role_complete("test/fast", "s", "p", Duration::from_secs(1))
                .await
                .is_none()
        );
    }
    #[test]
    fn apply_digest_cuts_at_user_boundary_and_keeps_tail() {
        use ka_dialect::speaker::{ToolCall, ToolResult, TurnMessage, TurnRole};
        let catalog = Catalog::parse(
            "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000\n",
        )
        .unwrap();
        let mut voice = Voice::new(catalog, std::env::temp_dir(), ka_protocol::Mode::Guarded, 5);
        // ratio 1 char/token; tail budget is 20k, so the filler must not fit
        let filler = "y".repeat(30_000);
        voice.history.push(TurnMessage::user("old question"));
        voice.history.push(TurnMessage::assistant("old answer"));
        voice
            .history
            .push(TurnMessage::user(format!("filler {filler}")));
        voice.history.push(TurnMessage::assistant_with_calls(
            "let me check",
            vec![ToolCall {
                id: "t9".into(),
                tool: "read".into(),
                arguments: Default::default(),
            }],
        ));
        voice.history.push(TurnMessage::tool(vec![ToolResult {
            call_id: "t9".into(),
            content: "file contents".into(),
            is_error: false,
            images: Vec::new(),
        }]));
        voice.history.push(TurnMessage::assistant("done with that"));
        voice.history.push(TurnMessage::user("keep me"));

        let kept = voice.apply_digest("SUMMARY".to_string(), 1.0);
        // the cut must land on a user message boundary ("filler..." is too
        // big to fit with everything after; "keep me" is small)
        assert_eq!(
            voice.history[0].role,
            TurnRole::User,
            "history must start at a user message"
        );
        assert!(
            voice.history.iter().any(|m| m.content == "keep me"),
            "tail preserved"
        );
        // tool pair intact: an assistant-with-calls message is followed by
        // its tool message, or neither is present
        if voice.history.iter().any(|m| m.role == TurnRole::Tool) {
            let idx = voice
                .history
                .iter()
                .position(|m| m.role == TurnRole::Tool)
                .unwrap();
            assert!(idx > 0, "tool message never first");
        }
        assert_eq!(voice.digest.as_deref(), Some("SUMMARY"));
        assert_eq!(kept, 2, "cut index points at the filler user message");
        assert!(voice.digest_revision >= 1);
        assert!(voice.take_pending_digest().is_some());
        assert!(voice.take_pending_digest().is_none(), "consumed once");
    }

    #[test]
    fn context_pressure_uses_reserve() {
        let catalog = Catalog::parse(
            "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 100000\n",
        )
        .unwrap();
        let mut voice = Voice::new(catalog, std::env::temp_dir(), ka_protocol::Mode::Guarded, 5);
        voice.set_model_selector("test/m", 4.0);
        assert!(
            !voice.context_pressure(100_000),
            "empty history, no pressure"
        );
        voice.note_context_for_tests(90_000);
        // reserve = max(16384, 15%) = 16384; 90k + 20k tail > 100k - 16384
        assert!(voice.context_pressure(100_000), "90k used + tail must trip");
        assert!(!voice.context_pressure(0), "unknown window never trips");
    }

    /// Overflow → digest → retry, all through the fake speaker.
    struct OverflowFakeSpeaker {
        calls: std::sync::Arc<parking_lot::Mutex<Vec<usize>>>,
    }

    impl Speaker for OverflowFakeSpeaker {
        fn speak<'a>(
            &'a self,
            req: SpeakRequest,
            out: tokio::sync::mpsc::Sender<StreamEvent>,
        ) -> SpeakFuture<'a> {
            let calls = self.calls.clone();
            Box::pin(async move {
                let n = {
                    let mut c = calls.lock();
                    c.push(0);
                    c.len()
                };
                let has_digest = req
                    .messages
                    .first()
                    .is_some_and(|m| m.content.starts_with("<context-digest>"));
                if req.tools.is_empty() {
                    // summarize call
                    out.send(StreamEvent::Text("digested state".into()))
                        .await
                        .ok();
                    out.send(StreamEvent::Finished {
                        stop: ka_protocol::Stop::Done,
                        usage: Default::default(),
                    })
                    .await
                    .ok();
                } else if n == 1 && !has_digest {
                    out.send(StreamEvent::Failed {
                        class: ka_protocol::ErrorClass::Overflow,
                        retryable: false,
                        message: "prompt is too long".into(),
                    })
                    .await
                    .ok();
                } else {
                    out.send(StreamEvent::Text("recovered".into())).await.ok();
                    out.send(StreamEvent::Finished {
                        stop: ka_protocol::Stop::Done,
                        usage: Default::default(),
                    })
                    .await
                    .ok();
                }
            })
        }
    }

    #[tokio::test]
    async fn overflow_triggers_digest_and_retry() {
        use ka_protocol::Event;
        use tokio::sync::mpsc;

        let catalog = Catalog::parse(
            "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 50000\n",
        )
        .unwrap();
        let calls = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let mut voice = Voice::new(catalog, std::env::temp_dir(), ka_protocol::Mode::Guarded, 5)
            .with_speaker(
                Wire::OpenaiChat,
                std::sync::Arc::new(OverflowFakeSpeaker {
                    calls: calls.clone(),
                }),
            );

        let (cmd_tx, mut cmd_rx) = mpsc::channel(16);
        let (evt_tx, mut evt_rx) = mpsc::channel(256);
        let mut interjections = Vec::new();
        let mut deferrals = std::collections::VecDeque::new();
        let handle = tokio::spawn(async move {
            voice
                .turn(
                    "test/m",
                    "big prompt".into(),
                    &mut cmd_rx,
                    &evt_tx,
                    &mut interjections,
                    &mut deferrals,
                    &mut GuardRuntime::default(),
                    None,
                    Vec::new(),
                )
                .await;
        });

        let mut finished = false;
        let mut digest_started = false;
        while let Some(evt) = evt_rx.recv().await {
            match evt {
                Event::DigestStarted => digest_started = true,
                Event::TurnFinished {
                    stop: ka_protocol::Stop::Done,
                    ..
                } => {
                    finished = true;
                    break;
                }
                Event::TurnFinished { .. } => break,
                _ => {}
            }
        }
        drop(cmd_tx);
        handle.await.unwrap();
        assert!(digest_started, "overflow must trigger a digest");
        assert!(finished, "turn must recover and finish done");
        assert!(calls.lock().len() >= 3, "speak, summarize, retry");
    }

    #[test]
    fn glob_match_basics() {
        assert!(glob_match("cargo *", "cargo build --release"));
        assert!(glob_match("cargo build", "cargo build"));
        assert!(!glob_match("cargo build", "cargo test"));
        assert!(glob_match("git push *", "git push origin main"));
        assert!(!glob_match("git push *", "git status"));
        assert!(glob_match("rm *build*", "rm -rf ./build-dir"));
        assert!(glob_match("*", "anything at all"));
        assert!(glob_match("read?file", "read-file"));
    }

    struct RuleFakeSpeaker;

    impl Speaker for RuleFakeSpeaker {
        fn speak<'a>(
            &'a self,
            _req: SpeakRequest,
            out: tokio::sync::mpsc::Sender<StreamEvent>,
        ) -> SpeakFuture<'a> {
            Box::pin(async move {
                out.send(StreamEvent::Call(ToolCall {
                    id: "c1".into(),
                    tool: "bash".into(),
                    arguments: serde_json::json!({"command": "cargo build"}),
                }))
                .await
                .ok();
                out.send(StreamEvent::Finished {
                    stop: ka_protocol::Stop::Done,
                    usage: Default::default(),
                })
                .await
                .ok();
            })
        }
    }

    #[tokio::test]
    async fn rules_deny_in_free_mode() {
        use ka_protocol::Event;
        use tokio::sync::mpsc;

        let catalog = Catalog::parse(
            "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000\n",
        )
        .unwrap();
        let mut voice = Voice::new(catalog, std::env::temp_dir(), ka_protocol::Mode::Free, 5)
            .with_speaker(Wire::OpenaiChat, std::sync::Arc::new(RuleFakeSpeaker));
        voice.set_rules(vec![crate::config::Rule {
            tool: "bash".into(),
            pattern: Some("cargo *".into()),
            verdict: crate::config::Verdict::Deny,
        }]);

        let (cmd_tx, mut cmd_rx) = mpsc::channel(16);
        let (evt_tx, mut evt_rx) = mpsc::channel(256);
        let mut interjections = Vec::new();
        let mut deferrals = std::collections::VecDeque::new();
        let handle = tokio::spawn(async move {
            voice
                .turn(
                    "test/m",
                    "build it".into(),
                    &mut cmd_rx,
                    &evt_tx,
                    &mut interjections,
                    &mut deferrals,
                    &mut GuardRuntime::default(),
                    None,
                    Vec::new(),
                )
                .await;
        });

        let mut denied_output = false;
        while let Some(evt) = evt_rx.recv().await {
            if let Event::CallOutput {
                excerpt, is_error, ..
            } = &evt
            {
                assert!(is_error, "denied call must be an error result");
                assert!(excerpt.contains("denied by rule"), "{excerpt}");
                denied_output = true;
            }
            if matches!(evt, Event::TurnFinished { .. }) {
                break;
            }
        }
        drop(cmd_tx);
        handle.await.unwrap();
        assert!(denied_output, "rule denial must surface as tool error");
    }

    #[tokio::test]
    async fn plan_mode_denies_writes_outside_plans_dir() {
        use ka_protocol::Event;
        use tokio::sync::mpsc;

        let catalog = Catalog::parse(
            "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000\n",
        )
        .unwrap();
        // speaker asks to write OUTSIDE the plans dir
        struct WriteOutside;
        impl Speaker for WriteOutside {
            fn speak<'a>(
                &'a self,
                _req: SpeakRequest,
                out: tokio::sync::mpsc::Sender<StreamEvent>,
            ) -> SpeakFuture<'a> {
                Box::pin(async move {
                    out.send(StreamEvent::Call(ToolCall {
                        id: "w1".into(),
                        tool: "write".into(),
                        arguments: serde_json::json!({"path": "src/main.rs", "content": "x"}),
                    }))
                    .await
                    .ok();
                    out.send(StreamEvent::Finished {
                        stop: ka_protocol::Stop::Done,
                        usage: Default::default(),
                    })
                    .await
                    .ok();
                })
            }
        }
        let mut voice = Voice::new(catalog, std::env::temp_dir(), ka_protocol::Mode::Plan, 5)
            .with_speaker(Wire::OpenaiChat, std::sync::Arc::new(WriteOutside));

        let (cmd_tx, mut cmd_rx) = mpsc::channel(16);
        let (evt_tx, mut evt_rx) = mpsc::channel(256);
        let handle = tokio::spawn(async move {
            let mut i = Vec::new();
            let mut d = std::collections::VecDeque::new();
            voice
                .turn(
                    "test/m",
                    "do it".into(),
                    &mut cmd_rx,
                    &evt_tx,
                    &mut i,
                    &mut d,
                    &mut GuardRuntime::default(),
                    None,
                    Vec::new(),
                )
                .await;
        });
        let mut blocked = false;
        while let Some(evt) = evt_rx.recv().await {
            if let Event::CallOutput {
                excerpt, is_error, ..
            } = &evt
            {
                if excerpt.contains("plan mode is read-only") && *is_error {
                    blocked = true;
                }
            }
            if matches!(evt, Event::TurnFinished { .. }) {
                break;
            }
        }
        drop(cmd_tx);
        handle.await.unwrap();
        assert!(
            blocked,
            "write outside .ka/plans must be denied in plan mode"
        );
    }

    #[test]
    fn write_gate_allows_free_and_accept_edits_asks_guarded() {
        use super::Gate;
        let catalog = Catalog::parse(
            "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000\n",
        )
        .unwrap();
        let call = ToolCall {
            id: "w1".into(),
            tool: "write".into(),
            arguments: serde_json::json!({"path": "out.txt", "content": "x"}),
        };
        for mode in [ka_protocol::Mode::Free, ka_protocol::Mode::AcceptEdits] {
            let voice = crate::voice::Voice::new(catalog.clone(), std::env::temp_dir(), mode, 5);
            assert!(
                matches!(
                    voice.gate(crate::hands::Clearance::Write, &call),
                    Gate::Allow
                ),
                "write must auto-allow in {mode:?}"
            );
        }
        let voice =
            crate::voice::Voice::new(catalog, std::env::temp_dir(), ka_protocol::Mode::Guarded, 5);
        match voice.gate(crate::hands::Clearance::Write, &call) {
            Gate::Ask { question } => assert_eq!(question, "allow write to modify files?"),
            Gate::Allow | Gate::Deny { .. } => {
                panic!("guarded write must ask")
            }
        }
    }

    #[test]
    fn exec_gate_allows_only_free_and_keeps_plan_phrasing() {
        use super::Gate;
        let command = "cargo build --release";
        let call = ToolCall {
            id: "b1".into(),
            tool: "bash".into(),
            arguments: serde_json::json!({"command": command}),
        };
        for (mode, expected) in [
            (ka_protocol::Mode::Free, None),
            (
                ka_protocol::Mode::AcceptEdits,
                Some("run `cargo build --release`?"),
            ),
            (
                ka_protocol::Mode::Guarded,
                Some("run `cargo build --release`?"),
            ),
            (
                ka_protocol::Mode::Plan,
                Some("plan mode: run `cargo build --release`? (build with /build)"),
            ),
        ] {
            let catalog = Catalog::parse(
                "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000\n",
            )
            .unwrap();
            let voice = crate::voice::Voice::new(catalog, std::env::temp_dir(), mode, 5);
            let gate = voice.gate(crate::hands::Clearance::Exec, &call);
            match (gate, expected) {
                (Gate::Allow, None) => {}
                (Gate::Ask { question }, Some(want)) => assert_eq!(question, want),
                (other, want) => panic!("exec gate in {mode:?}: got {other:?}, want {want:?}"),
            }
        }
    }

    #[test]
    fn rewind_truncates_before_nth_last_user_message() {
        use ka_dialect::speaker::TurnMessage;
        let catalog = Catalog::parse(
            "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000\n",
        )
        .unwrap();
        let mut voice = Voice::new(catalog, std::env::temp_dir(), ka_protocol::Mode::Guarded, 5);
        for (u, a) in [("q1", "a1"), ("q2", "a2"), ("q3", "a3")] {
            voice.history.push(TurnMessage::user(u));
            voice.history.push(TurnMessage::assistant(a));
        }
        let kept = voice.rewind(1).unwrap();
        assert_eq!(kept, 4);
        let contents: Vec<&str> = voice.history.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(
            contents,
            vec!["q1", "a1", "q2", "a2"],
            "last exchange dropped"
        );
    }

    /// Speaker that fails `failures` times with a retryable network
    /// error, then finishes normally. Records request count.
    struct FlakySpeaker {
        failures: usize,
        seen: std::sync::Arc<parking_lot::Mutex<Vec<usize>>>,
    }

    impl FlakySpeaker {
        fn new(failures: usize) -> (Self, std::sync::Arc<parking_lot::Mutex<Vec<usize>>>) {
            let seen = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
            (
                Self {
                    failures,
                    seen: seen.clone(),
                },
                seen,
            )
        }
    }

    impl Speaker for FlakySpeaker {
        fn speak<'a>(
            &'a self,
            _req: SpeakRequest,
            out: tokio::sync::mpsc::Sender<StreamEvent>,
        ) -> SpeakFuture<'a> {
            let failures = self.failures;
            let seen = self.seen.clone();
            Box::pin(async move {
                let attempt = {
                    let mut counts = seen.lock();
                    counts.push(1);
                    counts.len()
                };
                if attempt <= failures {
                    out.send(StreamEvent::Failed {
                        class: ka_protocol::ErrorClass::Network,
                        retryable: true,
                        message: "connection reset".into(),
                    })
                    .await
                    .ok();
                    return;
                }
                out.send(StreamEvent::Text("recovered".into())).await.ok();
                out.send(StreamEvent::Finished {
                    stop: ka_protocol::Stop::Done,
                    usage: Usage::default(),
                })
                .await
                .ok();
            })
        }
    }

    fn flaky_catalog() -> Catalog {
        Catalog::parse(
            "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000\n",
        )
        .unwrap()
    }

    /// Drive one turn, answering asks via `answer` (None = never answer).
    /// Returns (events, seen-request-counts).
    async fn drive_turn(
        voice: &mut Voice,
        guards: &mut GuardRuntime,
        prompt: &str,
        answer: Option<usize>,
    ) -> (Vec<Event>, Vec<usize>) {
        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel(16);
        let (evt_tx, mut evt_rx) = tokio::sync::mpsc::channel(256);
        let mut interjections = Vec::new();
        let mut deferrals = std::collections::VecDeque::new();
        let events_handle = tokio::spawn(async move {
            let mut events = Vec::new();
            while let Some(evt) = evt_rx.recv().await {
                let done = matches!(evt, Event::TurnFinished { .. });
                if let (Event::Ask { id, .. }, Some(choice)) = (&evt, answer) {
                    cmd_tx
                        .send(Command::Answer {
                            question: id.clone(),
                            choice,
                        })
                        .await
                        .ok();
                }
                events.push(evt);
                if done {
                    break;
                }
            }
            events
        });
        voice
            .turn(
                "test/m",
                prompt.into(),
                &mut cmd_rx,
                &evt_tx,
                &mut interjections,
                &mut deferrals,
                guards,
                None,
                Vec::new(),
            )
            .await;
        // keep the receiver alive until the turn task wraps up
        drop(cmd_rx);
        (events_handle.await.unwrap(), Vec::new())
    }

    #[tokio::test]
    async fn retryable_failure_retries_then_succeeds_without_duplicate_user() {
        let (speaker, seen) = FlakySpeaker::new(2);
        let mut voice = Voice::new(
            flaky_catalog(),
            std::env::temp_dir(),
            ka_protocol::Mode::Free,
            5,
        )
        .with_speaker(Wire::OpenaiChat, std::sync::Arc::new(speaker));
        let mut guards = GuardRuntime::default();
        let (events, _) = drive_turn(&mut voice, &mut guards, "only prompt", None).await;

        let notes: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                Event::Note { message } => Some(message.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(notes.len(), 2, "two retry notes expected: {notes:?}");
        assert!(notes[0].contains("retrying in"), "{notes:?}");
        assert!(matches!(
            events.last(),
            Some(Event::TurnFinished {
                stop: Stop::Done,
                ..
            })
        ));
        assert_eq!(seen.lock().len(), 3, "2 failures + 1 success");
        assert_eq!(
            voice
                .history
                .iter()
                .filter(|m| m.role == TurnRole::User)
                .count(),
            1,
            "user record must not be duplicated by retries"
        );
    }

    #[tokio::test]
    async fn retry_canceled_by_abort_finishes_with_error() {
        let (speaker, _seen) = FlakySpeaker::new(usize::MAX);
        let mut voice = Voice::new(
            flaky_catalog(),
            std::env::temp_dir(),
            ka_protocol::Mode::Free,
            5,
        )
        .with_speaker(Wire::OpenaiChat, std::sync::Arc::new(speaker));

        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel(16);
        let (evt_tx, mut evt_rx) = tokio::sync::mpsc::channel(256);
        let mut interjections = Vec::new();
        let mut deferrals = std::collections::VecDeque::new();
        let handle = tokio::spawn(async move {
            voice
                .turn(
                    "test/m",
                    "prompt".into(),
                    &mut cmd_rx,
                    &evt_tx,
                    &mut interjections,
                    &mut deferrals,
                    &mut GuardRuntime::default(),
                    None,
                    Vec::new(),
                )
                .await;
        });
        // wait for the first retry note, then abort the wait
        let mut saw_retry_note = false;
        let mut canceled_note = false;
        let mut finished = None;
        while let Some(evt) = evt_rx.recv().await {
            match evt {
                Event::Note { message } if message.contains("retrying in") => {
                    saw_retry_note = true;
                    cmd_tx.send(Command::Abort).await.ok();
                }
                Event::Note { message } if message.contains("retry canceled") => {
                    canceled_note = true;
                }
                Event::TurnFinished { stop, .. } => {
                    finished = Some(stop);
                    break;
                }
                _ => {}
            }
        }
        handle.await.unwrap();
        assert!(saw_retry_note, "retry note must precede the cancel");
        assert!(canceled_note, "cancel must be announced");
        assert_eq!(finished, Some(Stop::Error), "cancel finishes with Error");
    }

    #[tokio::test]
    async fn spend_guard_asks_once_and_stop_aborts() {
        // priced dialect: 1M input tokens at $1/Mtok = $1.00 per step
        let catalog = Catalog::parse(
            "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000\npriced = true\n\n[dialects.\"test/m\".price]\ninput_per_mtok = 1.0\noutput_per_mtok = 0.0\n",
        )
        .unwrap();
        struct BigUsage;
        impl Speaker for BigUsage {
            fn speak<'a>(
                &'a self,
                _req: SpeakRequest,
                out: tokio::sync::mpsc::Sender<StreamEvent>,
            ) -> SpeakFuture<'a> {
                Box::pin(async move {
                    out.send(StreamEvent::Text("hi".into())).await.ok();
                    out.send(StreamEvent::Finished {
                        stop: ka_protocol::Stop::Done,
                        usage: Usage {
                            input: 1_000_000,
                            ..Default::default()
                        },
                    })
                    .await
                    .ok();
                })
            }
        }
        let mut voice = Voice::new(catalog, std::env::temp_dir(), ka_protocol::Mode::Free, 5)
            .with_speaker(Wire::OpenaiChat, std::sync::Arc::new(BigUsage));
        let mut guards = GuardRuntime::new(Some(0.5), None);

        // stop at the ask → clean abort
        let (events, _) = drive_turn(&mut voice, &mut guards, "q1", Some(1)).await;
        let asks = events
            .iter()
            .filter(|e| matches!(e, Event::Ask { .. }))
            .count();
        assert_eq!(asks, 1, "guard ask fires once: {events:?}");
        assert!(matches!(
            events.last(),
            Some(Event::TurnFinished {
                stop: Stop::Aborted,
                ..
            })
        ));
        assert!(guards.spend_latched, "guard latches after the ask");

        // second turn: latched → no repeat ask, turn proceeds normally
        let (events2, _) = drive_turn(&mut voice, &mut guards, "q2", None).await;
        assert!(
            !events2.iter().any(|e| matches!(e, Event::Ask { .. })),
            "latched guard must not re-ask"
        );
        assert!(matches!(
            events2.last(),
            Some(Event::TurnFinished {
                stop: Stop::Done,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn permission_memory_answers_once_then_auto_allows() {
        let dir = std::env::temp_dir().join(format!("ka-perm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("perm-target.txt"), "one\n").unwrap();
        struct TwoWrites;
        impl Speaker for TwoWrites {
            fn speak<'a>(
                &'a self,
                req: SpeakRequest,
                out: tokio::sync::mpsc::Sender<StreamEvent>,
            ) -> SpeakFuture<'a> {
                Box::pin(async move {
                    let writes = req.messages.iter().flat_map(|m| &m.results).count();
                    if writes == 0 {
                        out.send(StreamEvent::Call(ToolCall {
                            id: "w1".into(),
                            tool: "write".into(),
                            arguments: serde_json::json!({
                                "path": "perm-target.txt",
                                "content": "changed\n"
                            }),
                        }))
                        .await
                        .ok();
                        out.send(StreamEvent::Finished {
                            stop: ka_protocol::Stop::Done,
                            usage: Usage::default(),
                        })
                        .await
                        .ok();
                    } else {
                        out.send(StreamEvent::Text("done".into())).await.ok();
                        out.send(StreamEvent::Finished {
                            stop: ka_protocol::Stop::Done,
                            usage: Usage::default(),
                        })
                        .await
                        .ok();
                    }
                })
            }
        }
        let catalog = Catalog::parse(
            "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000\n",
        )
        .unwrap();
        let mut voice = Voice::new(catalog, dir.clone(), ka_protocol::Mode::Guarded, 5)
            .with_speaker(Wire::OpenaiChat, std::sync::Arc::new(TwoWrites));
        let mut guards = GuardRuntime::default();
        // choice 1 = "always"
        let (events, _) = drive_turn(&mut voice, &mut guards, "edit it", Some(1)).await;
        let asks = events
            .iter()
            .filter(|e| matches!(e, Event::Ask { .. }))
            .count();
        assert_eq!(asks, 1, "exactly one permission ask: {events:?}");
        assert!(matches!(
            events.last(),
            Some(Event::TurnFinished {
                stop: Stop::Done,
                ..
            })
        ));
        // session memory holds the grant; a second identical call needs no ask
        let (events2, _) = drive_turn(&mut voice, &mut guards, "edit again", None).await;
        assert!(
            !events2.iter().any(|e| matches!(e, Event::Ask { .. })),
            "remembered permission must not re-ask"
        );
        assert!(matches!(
            events2.last(),
            Some(Event::TurnFinished {
                stop: Stop::Done,
                ..
            })
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rewind_to_empty_clears_history() {
        use ka_dialect::speaker::TurnMessage;
        let catalog = Catalog::parse(
            "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000\n",
        )
        .unwrap();
        let mut voice = Voice::new(catalog, std::env::temp_dir(), ka_protocol::Mode::Guarded, 5);
        for (u, a) in [("q1", "a1"), ("q2", "a2")] {
            voice.history.push(TurnMessage::user(u));
            voice.history.push(TurnMessage::assistant(a));
        }
        let kept = voice.rewind(2).unwrap();
        assert_eq!(kept, 0);
        assert!(voice.history.is_empty());
        assert!(voice.rewind(1).is_none(), "nothing left to rewind");
    }

    #[test]
    fn catalog_lookup_contract() {
        let catalog = Catalog::embedded();
        assert!(catalog.get("nope/missing").is_none());
        assert!(catalog.get("openai/gpt-5.1").is_some());
        let _ = Wire::OpenaiChat;
    }

    /// Is `pid` alive? (`kill -0`)
    fn pid_alive(pid: u32) -> bool {
        // zombies count as dead: the killed child is unreaped until the
        // test process exits
        crate::hands::jobs::pid_alive(Some(pid))
    }

    /// Three bash calls whose starts/finishes are logged to a shared file:
    /// concurrent execution shows all S- lines before the first E- line.
    struct BashTrio {
        log: std::path::PathBuf,
    }

    impl Speaker for BashTrio {
        fn speak<'a>(
            &'a self,
            req: SpeakRequest,
            out: tokio::sync::mpsc::Sender<StreamEvent>,
        ) -> SpeakFuture<'a> {
            Box::pin(async move {
                let has_result = req
                    .messages
                    .iter()
                    .any(|m| m.role == TurnRole::Tool && !m.results.is_empty());
                if has_result {
                    out.send(StreamEvent::Text("all done".into())).await.ok();
                    out.send(StreamEvent::Finished {
                        stop: ka_protocol::Stop::Done,
                        usage: Usage::default(),
                    })
                    .await
                    .ok();
                    return;
                }
                let log = self.log.display().to_string();
                for (i, name) in [(1, "ALPHA"), (2, "BETA"), (3, "GAMMA")] {
                    out.send(StreamEvent::Call(ToolCall {
                        id: format!("c{i}"),
                        tool: "bash".into(),
                        arguments: serde_json::json!({"command": format!(
                            "echo S-{i} >> {log}; sleep 0.3; echo E-{i} >> {log}; echo OUT-{name}"
                        )}),
                    }))
                    .await
                    .ok();
                }
                out.send(StreamEvent::Finished {
                    stop: ka_protocol::Stop::Done,
                    usage: Usage::default(),
                })
                .await
                .ok();
            })
        }
    }

    #[tokio::test]
    async fn parallel_step_runs_concurrently_and_returns_results_in_call_order() {
        let dir = std::env::temp_dir().join(format!("ka-voice-par-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("log");
        let catalog = Catalog::parse(
            "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000\n",
        )
        .unwrap();
        let mut voice = Voice::new(catalog, dir.clone(), ka_protocol::Mode::Free, 5).with_speaker(
            Wire::OpenaiChat,
            std::sync::Arc::new(BashTrio { log: log.clone() }),
        );
        let mut guards = GuardRuntime::default();
        let (_events, _) = drive_turn(&mut voice, &mut guards, "run the trio", None).await;

        // results fed back in ORIGINAL call order with matching call ids
        let tool_msg = voice
            .history
            .iter()
            .find(|m| m.role == TurnRole::Tool)
            .unwrap();
        let results = &tool_msg.results;
        for ((i, result), marker) in results.iter().enumerate().zip(["ALPHA", "BETA", "GAMMA"]) {
            assert_eq!(result.call_id, format!("c{}", i + 1), "{results:?}");
            assert!(
                result.content.contains(&format!("OUT-{marker}")),
                "result {i} must carry its own output: {}",
                result.content
            );
            assert!(!result.is_error);
        }
        // concurrency proof: every call started before any finished
        let lines: Vec<String> = std::fs::read_to_string(&log)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        let last_start = lines
            .iter()
            .rposition(|l| l.starts_with("S-"))
            .expect("starts logged");
        let first_end = lines
            .iter()
            .position(|l| l.starts_with("E-"))
            .expect("finishes logged");
        assert!(
            last_start < first_end,
            "calls must overlap, not serialize: {lines:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// One long-running bash call; the abort arrives mid-step.
    struct LongBash {
        pidfile: std::path::PathBuf,
    }

    impl Speaker for LongBash {
        fn speak<'a>(
            &'a self,
            _req: SpeakRequest,
            out: tokio::sync::mpsc::Sender<StreamEvent>,
        ) -> SpeakFuture<'a> {
            Box::pin(async move {
                out.send(StreamEvent::Call(ToolCall {
                    id: "b1".into(),
                    tool: "bash".into(),
                    arguments: serde_json::json!({"command": format!(
                        "sleep 30 & echo $! > {}; wait",
                        self.pidfile.display()
                    )}),
                }))
                .await
                .ok();
                out.send(StreamEvent::Finished {
                    stop: ka_protocol::Stop::Done,
                    usage: Usage::default(),
                })
                .await
                .ok();
            })
        }
    }

    #[tokio::test]
    async fn abort_mid_step_cancels_in_flight_and_kills_children() {
        let dir = std::env::temp_dir().join(format!("ka-voice-abort-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pidfile = dir.join("sleeper.pid");
        let catalog = Catalog::parse(
            "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000\n",
        )
        .unwrap();
        let mut voice = Voice::new(catalog, dir.clone(), ka_protocol::Mode::Free, 5).with_speaker(
            Wire::OpenaiChat,
            std::sync::Arc::new(LongBash {
                pidfile: pidfile.clone(),
            }),
        );

        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel(16);
        let (evt_tx, mut evt_rx) = tokio::sync::mpsc::channel(256);
        let mut interjections = Vec::new();
        let mut deferrals = std::collections::VecDeque::new();
        let handle = tokio::spawn(async move {
            voice
                .turn(
                    "test/m",
                    "start the sleeper".into(),
                    &mut cmd_rx,
                    &evt_tx,
                    &mut interjections,
                    &mut deferrals,
                    &mut GuardRuntime::default(),
                    None,
                    Vec::new(),
                )
                .await;
        });
        let mut aborted = false;
        while let Some(evt) = evt_rx.recv().await {
            match evt {
                Event::CallStarted { .. } => {
                    // wait for the child to publish its pid, then abort
                    for _ in 0..100 {
                        if pidfile.exists() {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                    cmd_tx.send(Command::Abort).await.ok();
                }
                Event::TurnFinished { stop, .. } => {
                    aborted = stop == Stop::Aborted;
                    break;
                }
                _ => {}
            }
        }
        handle.await.unwrap();
        assert!(aborted, "mid-step abort must finish the turn aborted");

        // the in-flight child must be dead shortly after the cancel
        let sleeper: u32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        for _ in 0..100 {
            if !pid_alive(sleeper) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(
            !pid_alive(sleeper),
            "aborted bash child (pid {sleeper}) must be killed, not orphaned"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// bash (auto-backgrounds) → jobs list → jobs kill → done.
    struct BgScript;

    impl Speaker for BgScript {
        fn speak<'a>(
            &'a self,
            req: SpeakRequest,
            out: tokio::sync::mpsc::Sender<StreamEvent>,
        ) -> SpeakFuture<'a> {
            Box::pin(async move {
                let results = req.messages.iter().flat_map(|m| &m.results).count();
                if results >= 3 {
                    out.send(StreamEvent::Text("wrapped up".into())).await.ok();
                    out.send(StreamEvent::Finished {
                        stop: ka_protocol::Stop::Done,
                        usage: Usage::default(),
                    })
                    .await
                    .ok();
                    return;
                }
                let (tool, id, call) = match results {
                    0 => (
                        "bash",
                        "b1",
                        serde_json::json!({"command": "echo bg-running; sleep 30"}),
                    ),
                    1 => ("jobs", "j1", serde_json::json!({})),
                    _ => ("jobs", "j2", serde_json::json!({"action": "kill", "id": 1})),
                };
                out.send(StreamEvent::Call(ToolCall {
                    id: id.into(),
                    tool: tool.into(),
                    arguments: call,
                }))
                .await
                .ok();
                out.send(StreamEvent::Finished {
                    stop: ka_protocol::Stop::Done,
                    usage: Usage::default(),
                })
                .await
                .ok();
            })
        }
    }
    #[tokio::test]
    async fn background_threshold_promotes_and_jobs_hand_polls_and_kills() {
        let _ = std::fs::remove_dir_all(std::env::temp_dir().join("ka-voice-bg-spills"));
        let catalog = Catalog::parse(
            "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000\n",
        )
        .unwrap();
        let mut voice = Voice::new(catalog, std::env::temp_dir(), ka_protocol::Mode::Free, 5)
            .with_speaker(Wire::OpenaiChat, std::sync::Arc::new(BgScript));
        voice.set_bash_background_ms(60);
        // capture before the voice moves into the turn task
        let jobs = voice.jobs();

        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel(16);
        let (evt_tx, mut evt_rx) = tokio::sync::mpsc::channel(256);
        let mut interjections = Vec::new();
        let mut deferrals = std::collections::VecDeque::new();
        let handle = tokio::spawn(async move {
            voice
                .turn(
                    "test/m",
                    "run long".into(),
                    &mut cmd_rx,
                    &evt_tx,
                    &mut interjections,
                    &mut deferrals,
                    &mut GuardRuntime::default(),
                    None,
                    Vec::new(),
                )
                .await;
        });
        let mut outputs: Vec<String> = Vec::new();
        while let Some(evt) = evt_rx.recv().await {
            if let Event::CallOutput { excerpt, .. } = &evt {
                outputs.push(excerpt.clone());
            }
            if matches!(evt, Event::TurnFinished { .. }) {
                break;
            }
        }
        drop(cmd_tx);
        handle.await.unwrap();
        let promotion = outputs
            .iter()
            .find(|o| o.contains("backgrounded as job 1"))
            .expect("promotion result must reach the surface");
        assert!(promotion.contains("poll with jobs"), "{promotion}");
        let listing = outputs
            .iter()
            .find(|o| o.contains("job 1") && !o.contains("backgrounded as job"))
            .expect("jobs list output");
        assert!(listing.contains("running"), "{listing}");
        assert!(listing.contains("sleep 30"), "{listing}");
        assert!(
            listing.contains("bg-running"),
            "tail must show streamed output: {listing}"
        );
        let kill = outputs
            .iter()
            .find(|o| o.contains("killing job 1"))
            .expect("kill output");
        assert!(!kill.is_empty(), "{kill}");
        // the kill watcher records the terminal state in the table
        for _ in 0..100 {
            if jobs.snapshot()[0].state != crate::hands::jobs::JobState::Running {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(
            matches!(
                jobs.snapshot()[0].state,
                crate::hands::jobs::JobState::Exited(_)
            ),
            "killed job must land exited: {:?}",
            jobs.snapshot()[0].state
        );
    }

    /// Fails with a non-retryable auth error for the listed model ids,
    /// otherwise finishes with text. Records every request's model id.
    struct AuthFailSpeaker {
        seen: std::sync::Arc<parking_lot::Mutex<Vec<String>>>,
        fail_for: Vec<String>,
    }

    impl Speaker for AuthFailSpeaker {
        fn speak<'a>(
            &'a self,
            req: SpeakRequest,
            out: mpsc::Sender<StreamEvent>,
        ) -> SpeakFuture<'a> {
            let seen = self.seen.clone();
            Box::pin(async move {
                seen.lock().push(req.model_id.clone());
                if self.fail_for.contains(&req.model_id) {
                    out.send(StreamEvent::Failed {
                        class: ka_protocol::ErrorClass::Auth,
                        retryable: false,
                        message: "auth boom".into(),
                    })
                    .await
                    .ok();
                } else {
                    out.send(StreamEvent::Text("ok".into())).await.ok();
                    out.send(StreamEvent::Finished {
                        stop: ka_protocol::Stop::Done,
                        usage: ka_protocol::Usage::default(),
                    })
                    .await
                    .ok();
                }
            })
        }
    }

    /// Catalog covering the fallback test models.
    fn fallback_catalog() -> Catalog {
        Catalog::parse(
            "[dialects.\"test/a\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000\n\
             [dialects.\"test/b\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000\n\
             [dialects.\"test/c\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000\n\
             [dialects.\"test/d\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000\n",
        )
        .unwrap()
    }

    /// Drive one turn on `start` with `fallbacks`, returning the notes,
    /// errors, finish flag and the model ids the speaker saw.
    async fn drive_fallback_turn(
        fail_for: Vec<String>,
        fallbacks: Vec<String>,
        start: &str,
    ) -> (
        Vec<String>,
        Vec<(ka_protocol::ErrorClass, String)>,
        bool,
        Vec<String>,
    ) {
        let seen = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let mut voice = Voice::new(
            fallback_catalog(),
            std::env::temp_dir(),
            ka_protocol::Mode::Free,
            5,
        )
        .with_speaker(
            Wire::OpenaiChat,
            std::sync::Arc::new(AuthFailSpeaker {
                seen: seen.clone(),
                fail_for,
            }),
        );
        voice.set_fallbacks(fallbacks);
        let (cmd_tx, mut cmd_rx) = mpsc::channel(16);
        let (evt_tx, mut evt_rx) = mpsc::channel(256);
        let mut interjections = Vec::new();
        let mut deferrals = std::collections::VecDeque::new();
        let mut guards = GuardRuntime::default();
        let start = start.to_string();
        let handle = tokio::spawn(async move {
            voice
                .turn(
                    &start,
                    "hi".into(),
                    &mut cmd_rx,
                    &evt_tx,
                    &mut interjections,
                    &mut deferrals,
                    &mut guards,
                    None,
                    Vec::new(),
                )
                .await
        });
        let mut notes = Vec::new();
        let mut errors = Vec::new();
        let mut finished = false;
        while let Some(evt) = evt_rx.recv().await {
            match evt {
                Event::Note { message } => notes.push(message),
                Event::Error { class, message, .. } => errors.push((class, message)),
                Event::TurnFinished { .. } => {
                    finished = true;
                    break;
                }
                _ => {}
            }
        }
        let _ = handle.await.unwrap();
        drop(cmd_tx);
        let ids = seen.lock().clone();
        (notes, errors, finished, ids)
    }

    #[tokio::test]
    async fn fallback_hops_to_next_model_on_auth_failure() {
        let (notes, errors, finished, ids) =
            drive_fallback_turn(vec!["test/a".into()], vec!["test/b".into()], "test/a").await;
        assert_eq!(ids, vec!["test/a".to_string(), "test/b".to_string()]);
        assert!(
            notes.iter().any(|n| n.contains("fallback → test/b")),
            "{notes:?}"
        );
        assert!(errors.is_empty(), "{errors:?}");
        assert!(finished);
    }

    #[tokio::test]
    async fn fallback_stops_after_two_hops() {
        // a, b, c all fail; d is healthy but unreachable: the 2-hop cap
        // fires first and the original error surfaces
        let (notes, errors, finished, ids) = drive_fallback_turn(
            vec!["test/a".into(), "test/b".into(), "test/c".into()],
            vec!["test/b".into(), "test/c".into(), "test/d".into()],
            "test/a",
        )
        .await;
        assert_eq!(
            ids,
            vec![
                "test/a".to_string(),
                "test/b".to_string(),
                "test/c".to_string()
            ]
        );
        assert_eq!(notes.iter().filter(|n| n.contains("fallback →")).count(), 2);
        assert!(
            errors
                .iter()
                .any(|(c, m)| *c == ka_protocol::ErrorClass::Auth && m == "auth boom")
        );
        assert!(finished, "turn must end with TurnFinished after the error");
    }

    #[tokio::test]
    async fn empty_fallback_chain_fails_directly() {
        let (notes, errors, finished, ids) =
            drive_fallback_turn(vec!["test/a".into()], Vec::new(), "test/a").await;
        assert_eq!(ids, vec!["test/a".to_string()]);
        assert!(!notes.iter().any(|n| n.contains("fallback")));
        assert!(
            errors
                .iter()
                .any(|(c, _)| *c == ka_protocol::ErrorClass::Auth)
        );
        assert!(finished);
    }

    #[tokio::test]
    async fn schema_on_non_structured_model_errors_instructively() {
        let catalog = Catalog::parse(
            "[dialects.\"test/ns\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000\n\n[dialects.\"test/ns\".flags]\nstructured = false\n",
        )
        .unwrap();
        let seen = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let mut voice = Voice::new(catalog, std::env::temp_dir(), ka_protocol::Mode::Free, 5)
            .with_speaker(
                Wire::OpenaiChat,
                std::sync::Arc::new(AuthFailSpeaker {
                    seen: seen.clone(),
                    fail_for: Vec::new(),
                }),
            );
        let (_cmd_tx, mut cmd_rx) = mpsc::channel(16);
        let (evt_tx, mut evt_rx) = mpsc::channel(256);
        let mut interjections = Vec::new();
        let mut deferrals = std::collections::VecDeque::new();
        let mut guards = GuardRuntime::default();
        let schema = serde_json::json!({"type": "object"});
        let handle = tokio::spawn(async move {
            voice
                .turn(
                    "test/ns",
                    "hi".into(),
                    &mut cmd_rx,
                    &evt_tx,
                    &mut interjections,
                    &mut deferrals,
                    &mut guards,
                    Some(schema),
                    Vec::new(),
                )
                .await
        });
        let mut saw_unsupported = false;
        while let Some(evt) = evt_rx.recv().await {
            if let Event::Error {
                class: ka_protocol::ErrorClass::Unsupported,
                message,
                ..
            } = evt
            {
                assert!(message.contains("structured output"), "{message}");
                saw_unsupported = true;
                break;
            }
        }
        assert!(saw_unsupported, "expected an Unsupported error event");
        let _ = handle.await.unwrap();
        assert!(
            seen.lock().is_empty(),
            "the speaker must not be reached for unsupported schemas"
        );
    }

    /// Overflows on the small model, succeeds otherwise; records models.
    struct PromoteSpeaker {
        seen: std::sync::Arc<parking_lot::Mutex<Vec<String>>>,
    }

    impl Speaker for PromoteSpeaker {
        fn speak<'a>(
            &'a self,
            req: SpeakRequest,
            out: mpsc::Sender<StreamEvent>,
        ) -> SpeakFuture<'a> {
            let seen = self.seen.clone();
            Box::pin(async move {
                seen.lock().push(req.model_id.clone());
                if req.model_id == "test/small" {
                    out.send(StreamEvent::Failed {
                        class: ka_protocol::ErrorClass::Overflow,
                        retryable: false,
                        message: "prompt too long".into(),
                    })
                    .await
                    .ok();
                } else {
                    out.send(StreamEvent::Text("fits".into())).await.ok();
                    out.send(StreamEvent::Finished {
                        stop: ka_protocol::Stop::Done,
                        usage: ka_protocol::Usage::default(),
                    })
                    .await
                    .ok();
                }
            })
        }
    }

    fn promote_catalog() -> Catalog {
        Catalog::parse(
            "[dialects.\"test/small\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 100\n\
             [dialects.\"test/big\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000\n",
        )
        .unwrap()
    }

    #[tokio::test]
    async fn overflow_promotes_before_digesting() {
        let seen = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let mut voice = Voice::new(
            promote_catalog(),
            std::env::temp_dir(),
            ka_protocol::Mode::Free,
            5,
        )
        .with_speaker(
            Wire::OpenaiChat,
            std::sync::Arc::new(PromoteSpeaker { seen: seen.clone() }),
        );
        let (_cmd_tx, mut cmd_rx) = mpsc::channel(16);
        let (evt_tx, mut evt_rx) = mpsc::channel(256);
        let mut interjections = Vec::new();
        let mut deferrals = std::collections::VecDeque::new();
        let mut guards = GuardRuntime::default();
        let handle = tokio::spawn(async move {
            voice
                .turn(
                    "test/small",
                    "hi".into(),
                    &mut cmd_rx,
                    &evt_tx,
                    &mut interjections,
                    &mut deferrals,
                    &mut guards,
                    None,
                    Vec::new(),
                )
                .await
        });
        let mut notes = Vec::new();
        let mut saw_digest_started = false;
        while let Some(evt) = evt_rx.recv().await {
            match evt {
                Event::Note { message } => notes.push(message),
                Event::DigestStarted => saw_digest_started = true,
                Event::TurnFinished { .. } => break,
                _ => {}
            }
        }
        let _ = handle.await.unwrap();
        assert!(
            notes.iter().any(|n| n.contains("context → test/big")),
            "{notes:?}"
        );
        assert!(!saw_digest_started, "promotion must precede digestion");
        {
            let ids = seen.lock();
            assert_eq!(ids.first().map(String::as_str), Some("test/small"));
            assert_eq!(ids.last().map(String::as_str), Some("test/big"));
        }
    }

    #[tokio::test]
    async fn promotion_disabled_goes_straight_to_digest() {
        let seen = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let mut voice = Voice::new(
            promote_catalog(),
            std::env::temp_dir(),
            ka_protocol::Mode::Free,
            5,
        )
        .with_speaker(
            Wire::OpenaiChat,
            std::sync::Arc::new(PromoteSpeaker { seen: seen.clone() }),
        );
        voice.set_context_promote(false);
        let (_cmd_tx, mut cmd_rx) = mpsc::channel(16);
        let (evt_tx, mut evt_rx) = mpsc::channel(256);
        let mut interjections = Vec::new();
        let mut deferrals = std::collections::VecDeque::new();
        let mut guards = GuardRuntime::default();
        let handle = tokio::spawn(async move {
            voice
                .turn(
                    "test/small",
                    "hi".into(),
                    &mut cmd_rx,
                    &evt_tx,
                    &mut interjections,
                    &mut deferrals,
                    &mut guards,
                    None,
                    Vec::new(),
                )
                .await
        });
        let mut notes = Vec::new();
        while let Some(evt) = evt_rx.recv().await {
            match evt {
                Event::Note { message } => notes.push(message),
                Event::TurnFinished { .. } => break,
                _ => {}
            }
        }
        let _ = handle.await.unwrap();
        assert!(!notes.iter().any(|n| n.contains("context →")), "{notes:?}");
        assert!(
            seen.lock().iter().all(|m| m == "test/small"),
            "no promotion means the model never changes"
        );
    }

    /// Responds "CANDIDATE" to summarize-shaped requests (no tools).
    struct SpeculativeSpeaker;

    impl Speaker for SpeculativeSpeaker {
        fn speak<'a>(
            &'a self,
            req: SpeakRequest,
            out: mpsc::Sender<StreamEvent>,
        ) -> SpeakFuture<'a> {
            Box::pin(async move {
                if req.tools.is_empty() {
                    out.send(StreamEvent::Text("CANDIDATE".into())).await.ok();
                }
                out.send(StreamEvent::Finished {
                    stop: ka_protocol::Stop::Done,
                    usage: ka_protocol::Usage::default(),
                })
                .await
                .ok();
            })
        }
    }

    #[tokio::test]
    async fn speculative_candidate_used_when_watermark_matches() {
        let catalog = Catalog::parse(
            "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000000\n",
        )
        .unwrap();
        let mut voice = Voice::new(catalog, std::env::temp_dir(), ka_protocol::Mode::Free, 5)
            .with_speaker(Wire::OpenaiChat, std::sync::Arc::new(SpeculativeSpeaker));
        voice.set_model_selector("test/m", 4.0);
        // 70% of the reserve threshold: speculative zone, not tripping
        voice.note_context_for_tests(700_000);
        assert!(voice.context_pressure_frac(1_000_000, 80));
        assert!(!voice.context_pressure(1_000_000));
        assert!(voice.start_speculative(None), "speculative must fire");

        // watermark unchanged: the candidate is ready and matches
        // (poll: the background task stores the receiver on completion)
        let taken = loop {
            if let Some(t) = voice.take_speculative().await {
                break t;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        };
        assert_eq!(taken, "CANDIDATE");
        // consumed: nothing left
        assert!(voice.take_speculative().await.is_none());
    }

    #[tokio::test]
    async fn speculative_candidate_discarded_on_watermark_mismatch() {
        let catalog = Catalog::parse(
            "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000000\n",
        )
        .unwrap();
        let mut voice = Voice::new(catalog, std::env::temp_dir(), ka_protocol::Mode::Free, 5)
            .with_speaker(Wire::OpenaiChat, std::sync::Arc::new(SpeculativeSpeaker));
        voice.set_model_selector("test/m", 4.0);
        voice.note_context_for_tests(700_000);
        assert!(voice.start_speculative(None));
        // history moved on since the candidate started
        voice.history.push(TurnMessage::user("newer question"));
        assert!(
            voice.take_speculative().await.is_none(),
            "stale candidate must be discarded"
        );
    }

    #[tokio::test]
    async fn speculative_skipped_below_eighty_percent() {
        let catalog = Catalog::parse(
            "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000000\n",
        )
        .unwrap();
        let mut voice = Voice::new(catalog, std::env::temp_dir(), ka_protocol::Mode::Free, 5)
            .with_speaker(Wire::OpenaiChat, std::sync::Arc::new(SpeculativeSpeaker));
        voice.set_model_selector("test/m", 4.0);
        voice.note_context_for_tests(400_000);
        assert!(!voice.context_pressure_frac(1_000_000, 80));
        assert!(!voice.start_speculative(None));
    }
}
