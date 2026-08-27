//! The live voice: multi-step tool loop. Each step speaks with the offered
//! tools, executes returned calls through the Hands registry under
//! clearance gating (hardstops always prompt; headless surfaces deny),
//! feeds results back, and repeats until the model rests or the step cap
//! hits.

use std::collections::{HashMap, HashSet, VecDeque};

use ka_dialect::dialects::{Catalog, Wire};
use ka_dialect::speaker::{
    SpeakRequest, Speaker, StreamEvent, ToolCall, ToolResult, ToolSpec, TurnMessage, TurnRole,
};
use ka_protocol::{AskId, AskQuestion, Command, ErrorClass, Event, Stop, Usage};
use tokio::sync::mpsc;

use crate::hands::bashp::{all_readonly, analyze, hardstop};
use crate::hands::{
    Clearance, Hand, HandContext, Ledger, Spill, ToolOutput, registry_with_pathfinder,
};

/// Session-scoped mutable state the voice needs (owned by the engine).
#[derive(Default)]
pub struct VoiceState {
    /// Session always-allow rules (`tool:<name>`, `bash:<program>`).
    pub rules: HashSet<String>,
    /// Monotonic ask counter.
    pub ask_counter: u32,
    /// Loop-guard counts of (tool, args) signatures this prompt.
    pub loop_counts: HashMap<String, usize>,
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
pub struct Voice {
    catalog: Catalog,
    speakers: HashMap<Wire, std::sync::Arc<dyn Speaker>>,
    hands: Vec<Box<dyn Hand>>,
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
    /// Pathfinder bootstrap slot shared with the hand.
    pathfinder_slot:
        std::sync::Arc<parking_lot::RwLock<crate::hands::pathfinder::PathfinderSource>>,
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
        Self {
            catalog,
            speakers: Default::default(),
            hands: registry_with_pathfinder(slot.clone()),
            hand_ctx: HandContext {
                cwd: cwd.clone(),
                ledger: std::sync::Arc::new(parking_lot::Mutex::new(Ledger::default())),
                spill: std::sync::Arc::new(Spill::new()),
                snapshots: std::sync::Arc::new(parking_lot::Mutex::new(
                    crate::hands::snapshots::Snapshots::open(&cwd),
                )),
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
            rules_cfg: Vec::new(),
            hooks_cfg: Vec::new(),
            pathfinder_slot: slot,
        }
    }

    /// Register an extra tool (MCP hands arrive after async discovery).
    pub fn push_hand(&mut self, hand: Box<dyn Hand>) {
        self.hands.push(hand);
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
        let hands = crate::hands::registry_with_pathfinder(slot.clone());
        let hand_ctx = HandContext {
            cwd,
            ledger: std::sync::Arc::new(parking_lot::Mutex::new(Ledger::default())),
            spill: std::sync::Arc::new(Spill::new()),
            snapshots: std::sync::Arc::new(parking_lot::Mutex::new(
                crate::hands::snapshots::Snapshots::inert(),
            )),
        };
        Self {
            catalog,
            speakers: Default::default(),
            hands,
            hand_ctx,
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
            rules_cfg: Vec::new(),
            hooks_cfg: Vec::new(),
            pathfinder_slot: slot,
        }
    }

    /// Set configured permission rules + hooks (engine bootstrap).
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

    /// Run matching hooks for one event. Returns Err(reason) when a
    /// pre_tool_use hook blocked the call (exit 2, stderr as reason).
    async fn run_hooks(
        &self,
        event: crate::config::HookEvent,
        tool: &str,
        args: &serde_json::Value,
    ) -> Result<(), String> {
        use tokio::io::AsyncWriteExt;
        for hook in &self.hooks_cfg {
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
                .current_dir(&self.hand_ctx.cwd)
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

    /// Load resumed history + digest (engine bootstrap).
    pub fn load_history(&mut self, history: Vec<TurnMessage>, digest: Option<String>) {
        self.history = history;
        self.digest = digest;
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
        if window == 0 {
            return false; // unknown window: never auto-digest
        }
        // omp rule: reserve is the 16k floor when practical, but never
        // more than a quarter of small windows (proportional reserve)
        let proportional = window * RESERVE_PCT / 100;
        let reserve = proportional.max(RESERVE_FLOOR.min(window / 4));
        self.last_context + KEEP_TAIL_TOKENS.min(window / 4) > window.saturating_sub(reserve)
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
        let req = SpeakRequest {
            model_id,
            dialect: dialect.clone(),
            effort: None,
            system,
            messages,
            tools: Vec::new(),
            token,
            cache_key: None,
        };
        let speaker = self.speaker(dialect.wire);
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
        let _ = ratio;
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
    /// `TurnFinished`.
    #[allow(clippy::too_many_arguments)]
    pub async fn turn(
        &mut self,
        model_selector: &str,
        prompt: String,
        commands: &mut mpsc::Receiver<Command>,
        events: &mpsc::Sender<Event>,
        interjections: &mut Vec<String>,
        deferrals: &mut VecDeque<String>,
    ) {
        use ka_dialect::parse_selector;

        let parsed = match parse_selector(model_selector) {
            Ok(p) => p,
            Err(e) => {
                return finish_after_error(events, ErrorClass::Protocol, &e.to_string()).await;
            }
        };
        let model_id = parsed.model_id();
        let Some(dialect) = self.catalog.get(&model_id).cloned() else {
            return finish_after_error(
                events,
                ErrorClass::Protocol,
                &format!("unknown model {model_id:?} (not in catalog; add a dialect overlay)"),
            )
            .await;
        };
        let price = dialect.price;
        let ratio = if dialect.ratio > 0.0 {
            dialect.ratio
        } else {
            4.0
        };
        let token = dialect
            .api_key_env
            .as_deref()
            .and_then(ka_dialect::auth::resolve_token);
        let window = dialect.context as u64;

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
        self.history.push(TurnMessage::user(prompt));
        self.state.loop_counts.clear();
        let mut usage_total = Usage::default();
        let mut assistant_text = String::new();
        let mut final_stop = Stop::Done;
        let mut steps = 0u32;
        let mut overflow_retried = false;

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
            let mut step_failed: Option<(ErrorClass, String)> = None;
            let mut step_finished = false;

            while !step_finished {
                tokio::select! {
                    biased;
                    maybe_cmd = commands.recv() => {
                        match maybe_cmd {
                            None => return,
                            Some(Command::Abort) => {
                                events.send(Event::TurnFinished {
                                    stop: Stop::Aborted,
                                    usage: Usage::default(),
                                }).await.ok();
                                return;
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
                                final_stop = stop;
                                step_finished = true;
                            }
                            StreamEvent::Failed { class, message, .. } => {
                                step_failed = Some((class, message));
                                final_stop = Stop::Error;
                                step_finished = true;
                            }
                        }
                    }
                }
            }

            if let Some((class, message)) = step_failed {
                // Overflow → digest-and-retry once.
                if class == ErrorClass::Overflow && !overflow_retried {
                    overflow_retried = true;
                    events.send(Event::DigestStarted).await.ok();
                    if let Some(summary) = self
                        .summarize(None, std::time::Duration::from_secs(120))
                        .await
                    {
                        self.apply_digest(summary, self.ratio);
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

            // Execute this step's calls; ordered results.
            self.history.push(TurnMessage::assistant_with_calls(
                step_text.clone(),
                step_calls.clone(),
            ));
            let mut results: Vec<ToolResult> = Vec::new();
            for call in &step_calls {
                let sig = format!("{}|{}", call.tool, call.arguments);
                *self.state.loop_counts.entry(sig).or_insert(0) += 1;
                if self.state.loop_counts.values().any(|c| *c >= 4) {
                    results.push(ToolResult {
                        call_id: call.id.clone(),
                        content: "loop guard: this tool was called with identical arguments 4+ \
                                  times; stop repeating and reconsider"
                            .to_string(),
                        is_error: true,
                    });
                    events
                        .send(Event::CallFinished {
                            tool: call.tool.clone(),
                            id: call.id.clone(),
                            ok: false,
                        })
                        .await
                        .ok();
                    continue;
                }
                let output = self.gate_and_execute(call, commands, events).await;
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
                results.push(ToolResult {
                    call_id: call.id.clone(),
                    content: output.content,
                    is_error: output.is_error,
                });
            }
            self.history.push(TurnMessage::tool(results));
            steps += 1;
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
    }

    /// Clearance gate, then execution. Ask-and-wait for anything not
    /// auto-allowed; surface answers arrive as `Command::Answer`.
    async fn gate_and_execute(
        &mut self,
        call: &ToolCall,
        commands: &mut mpsc::Receiver<Command>,
        events: &mpsc::Sender<Event>,
    ) -> ToolOutput {
        let Some(hand) = self.hands.iter().find(|h| h.def().name == call.tool) else {
            return ToolOutput::err(format!("unknown tool {}", call.tool));
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
            return ToolOutput::err(format!("blocked by hook: {reason}"));
        }
        let def = hand.def();
        let verdict = self.gate(def.clearance, call);
        match verdict {
            Gate::Allow => {}
            Gate::Deny { reason } => return ToolOutput::err(reason),
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
                    return ToolOutput::err("permission ask failed: surface closed");
                }
                // wait for the answer (or abort)
                loop {
                    tokio::select! {
                        maybe = commands.recv() => {
                            match maybe {
                                Some(Command::Answer { question: q, choice }) if q == ask_id => {
                                    match choice {
                                        1 => {
                                            self.state.rules.insert(format!("tool:{}", call.tool));
                                            break;
                                        }
                                        2 => {
                                            return ToolOutput::err(format!(
                                                "permission denied by user for {}",
                                                call.tool
                                            ));
                                        }
                                        _ => break,
                                    }
                                }
                                Some(Command::Abort) => {
                                    return ToolOutput::err("aborted");
                                }
                                Some(_) => {}
                                None => return ToolOutput::err("surface closed during ask"),
                            }
                        }
                    }
                }
            }
        }
        let mut output = hand.execute(&call.arguments, &self.hand_ctx).await;
        // post_tool_use hooks: exit 2 flags the result as an error
        if let Err(reason) = self
            .run_hooks(
                crate::config::HookEvent::PostToolUse,
                &call.tool,
                &call.arguments,
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
                ka_protocol::Mode::Free => Gate::Allow,
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
                    ka_protocol::Mode::Guarded => Gate::Ask {
                        question: format!("run `{command}`?"),
                    },
                }
            }
        }
    }
}

enum Gate {
    Allow,
    Ask { question: String },
    Deny { reason: String },
}

fn truncate_excerpt(text: &str) -> String {
    let capped: String = text.chars().take(2_000).collect();
    if capped.len() < text.len() {
        format!("{capped}…")
    } else {
        capped
    }
}

async fn finish_after_error(events: &mpsc::Sender<Event>, class: ErrorClass, message: &str) {
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

    use ka_dialect::dialects::{Catalog, Wire};
    use ka_dialect::speaker::{
        SpeakFuture, SpeakRequest, Speaker, StreamEvent, ToolCall, TurnMessage, TurnRole,
    };
    use ka_protocol::Usage;

    use super::{Voice, cost_of, glob_match};

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
        }]));
        let saved = voice.prune_tool_outputs(1.0);
        assert_eq!(saved, 0, "tiny outputs must not be pruned");
        assert_eq!(voice.history[2].results[0].content, "tiny");
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
}
