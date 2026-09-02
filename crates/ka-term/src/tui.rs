//! The ka TUI: streaming transcript, input editor, footer meters, and ask
//! dialogs. Built on ratatui; talks to the engine exclusively through the
//! Command/Event queues.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use ka_protocol::{AskId, Command, Event};
use tokio::sync::mpsc;

/// How the app decided to exit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Exit {
    /// User quit.
    Quit,
    /// Engine ended.
    EngineEnded,
}

/// Editable single-line-ish input buffer with history (pure logic, tested).
#[derive(Debug, Default, Clone)]
pub struct InputBuffer {
    /// Current text.
    pub text: String,
    /// Cursor position (char index).
    pub cursor: usize,
    history: VecDeque<String>,
    /// Index while browsing history; None = live edit.
    browsing: Option<usize>,
}

impl InputBuffer {
    /// Insert a character at the cursor.
    pub fn insert(&mut self, c: char) {
        let byte = self.char_to_byte(self.cursor);
        self.text.insert(byte, c);
        self.cursor += 1;
        self.browsing = None;
    }

    /// Delete the character before the cursor.
    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let start = self.char_to_byte(self.cursor - 1);
        let end = self.char_to_byte(self.cursor);
        self.text.replace_range(start..end, "");
        self.cursor -= 1;
        self.browsing = None;
    }

    /// Move the cursor left one char.
    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    /// Move the cursor right one char.
    pub fn right(&mut self) {
        let len = self.text.chars().count();
        if self.cursor < len {
            self.cursor += 1;
        }
    }

    /// Move to the start.
    pub fn home(&mut self) {
        self.cursor = 0;
    }

    /// Move to the end.
    pub fn end(&mut self) {
        self.cursor = self.text.chars().count();
    }

    /// Insert a line break at the cursor.
    pub fn newline(&mut self) {
        let byte = self.char_to_byte(self.cursor);
        self.text.insert(byte, '\n');
        self.cursor += 1;
        self.browsing = None;
    }

    /// Insert a string at the cursor; CRLF/CR normalized to `\n`.
    pub fn insert_str(&mut self, s: &str) {
        let normalized = s.replace("\r\n", "\n").replace('\r', "\n");
        let byte = self.char_to_byte(self.cursor);
        self.text.insert_str(byte, &normalized);
        self.cursor += normalized.chars().count();
        self.browsing = None;
    }

    /// The input split into rows (one per source line).
    pub fn rows(&self) -> Vec<&str> {
        self.text.split('\n').collect()
    }

    /// Cursor position as `(row, column)` over `rows()`.
    pub fn cursor_row_col(&self) -> (usize, usize) {
        cursor_row_col(&self.text, self.cursor)
    }

    /// Move the cursor up one row (column clamped to that row's end).
    /// Returns `false` for single-line text — history-browse territory.
    pub fn move_up(&mut self) -> bool {
        if !self.text.contains('\n') {
            return false;
        }
        let (row, col) = self.cursor_row_col();
        self.place_cursor(row.saturating_sub(1), col);
        true
    }

    /// Move the cursor down one row (column clamped to that row's end).
    /// Returns `false` for single-line text — history-browse territory.
    pub fn move_down(&mut self) -> bool {
        if !self.text.contains('\n') {
            return false;
        }
        let (row, col) = self.cursor_row_col();
        self.place_cursor(row + 1, col);
        true
    }

    fn place_cursor(&mut self, row: usize, col: usize) {
        let mut char_idx = 0;
        for (i, r) in self.rows().iter().enumerate() {
            if i == row {
                char_idx += col.min(r.chars().count());
                self.cursor = char_idx;
                return;
            }
            char_idx += r.chars().count() + 1; // +1 for the newline
        }
        self.cursor = char_idx; // row past the end: clamp to text end
    }

    /// Take the current text (clearing the buffer).
    pub fn take(&mut self) -> String {
        let text = std::mem::take(&mut self.text);
        self.cursor = 0;
        if !text.trim().is_empty() {
            self.history.push_back(text.clone());
            if self.history.len() > 100 {
                self.history.pop_front();
            }
        }
        self.browsing = None;
        text
    }

    /// Browse history upward (older).
    pub fn history_prev(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let idx = match self.browsing {
            None => self.history.len() - 1,
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.browsing = Some(idx);
        self.load_history(idx);
    }

    /// Browse history downward (newer).
    pub fn history_next(&mut self) {
        let Some(idx) = self.browsing else {
            return;
        };
        if idx + 1 >= self.history.len() {
            self.browsing = None;
            self.text.clear();
            self.cursor = 0;
        } else {
            self.browsing = Some(idx + 1);
            self.load_history(idx + 1);
        }
    }

    fn load_history(&mut self, idx: usize) {
        if let Some(entry) = self.history.get(idx) {
            self.text = entry.clone();
            self.cursor = self.text.chars().count();
        }
    }

    fn char_to_byte(&self, char_idx: usize) -> usize {
        self.text
            .char_indices()
            .nth(char_idx)
            .map(|(b, _)| b)
            .unwrap_or(self.text.len())
    }
}

/// Transcript line kinds for rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Line {
    /// User input.
    User(String),
    /// Assistant text.
    Assistant(String),
    /// Reasoning (rendered dim).
    Thought(String),
    /// Tool activity, collapsed.
    Tool(String),
    /// System/status note.
    Note(String),
}

/// A provider row for the settings panel (built by the CLI from the
/// ka-dialect registry; ka-term stays dialect-free).
#[derive(Debug, Clone)]
pub struct ProviderInfo {
    /// Vendor prefix.
    pub name: String,
    /// Env var holding the API key (empty = keyless).
    pub env_var: String,
    /// Endpoint base URL.
    pub base_url: String,
    /// Whether the env var is set in this process.
    pub key_set: bool,
}

/// Transcript with a render cache. Entries are immutable once pushed, so
/// each is rendered (role blocks + markdown) exactly once per terminal
/// width; resizing rebuilds. Without the cache every frame re-parsed the
/// whole transcript — O(transcript) work per streaming delta.
#[derive(Debug, Default)]
pub struct Transcript {
    lines: Vec<Line>,
    rendered: Vec<Vec<ratatui::text::Line<'static>>>,
    width: u16,
    /// Render passes issued (cache-audit in tests).
    renders: usize,
}

impl Transcript {
    /// Append an entry; renders it at the current width.
    pub fn push(&mut self, line: Line) {
        let w = self.width;
        self.rendered.push(render_line(&line, w));
        self.lines.push(line);
        self.renders += 1;
    }

    /// Adopt a new terminal width, rebuilding the cache if it changed.
    pub fn set_width(&mut self, width: u16) {
        if width != self.width {
            self.width = width;
            self.rebuild();
        }
    }

    fn rebuild(&mut self) {
        let w = self.width;
        self.rendered = self.lines.iter().map(|l| render_line(l, w)).collect();
        self.renders += self.lines.len();
    }

    /// Drop everything (session switch).
    pub fn clear(&mut self) {
        self.lines.clear();
        self.rendered.clear();
    }

    /// The source entries, in order.
    pub fn entries(&self) -> &[Line] {
        &self.lines
    }

    /// Cached rendered row count.
    pub fn total_rows(&self) -> usize {
        self.rendered.iter().map(Vec::len).sum()
    }

    /// One rendered row by absolute index.
    pub fn row(&self, i: usize) -> Option<&ratatui::text::Line<'static>> {
        let mut i = i;
        for entry in &self.rendered {
            if i < entry.len() {
                return entry.get(i);
            }
            i -= entry.len();
        }
        None
    }

    /// Render passes issued so far (tests).
    pub fn render_passes(&self) -> usize {
        self.renders
    }
}

/// Render one transcript entry into styled rows at `width`.
fn render_line(line: &Line, width: u16) -> Vec<ratatui::text::Line<'static>> {
    use ratatui::text::Line as TuiLine;
    let mut out: Vec<TuiLine> = Vec::new();
    match line {
        Line::User(text) => push_block(&mut out, text, width),
        Line::Assistant(text) => {
            // assistant output on a subtle full-width surface: background
            // fill only, never border glyphs; the trailing gap row carries
            // the same fill so empty rows stay uniform
            let bg = crate::palette::BG_OUTPUT;
            let mut rows = crate::markdown::render(text, width);
            apply_output_surface(&mut rows, width, bg);
            out.extend(rows);
            out.push(surface_blank(width, bg));
        }
        Line::Thought(text) => push_gutter(&mut out, text, width, "⋯ ", crate::palette::THOUGHT),
        Line::Tool(text) => push_gutter(&mut out, text, width, "⚙ ", crate::palette::META),
        Line::Note(text) => push_gutter(
            &mut out,
            text,
            width,
            "! ",
            ratatui::style::Style::new().fg(crate::palette::ERR),
        ),
    }
    out
}

/// Viewport window over `total` rendered rows for `visible` rows given the
/// scroll anchor (None = pinned to tail). Returns `(start_row, pinned)`;
/// an anchor at or past the tail re-pins.
fn window_range(total: usize, visible: usize, scroll: Option<usize>) -> (usize, bool) {
    if visible == 0 || total <= visible {
        return (0, true);
    }
    let max_start = total - visible;
    match scroll {
        None => (max_start, true),
        Some(anchor) => {
            let anchor = anchor.min(max_start);
            (anchor, anchor >= max_start)
        }
    }
}

/// Visible transcript rows for a terminal height: input area + footer +
/// transcript top border are the three rows carved out of the viewport.
fn visible_rows(term_h: u16, input_h: u16) -> usize {
    term_h.saturating_sub(input_h + 2) as usize
}
/// Cursor `(row, col)` in `text` for a char-index cursor position.
fn cursor_row_col(text: &str, cursor_chars: usize) -> (usize, usize) {
    let before: String = text.chars().take(cursor_chars).collect();
    let row = before.matches('\n').count();
    let col = before.chars().rev().take_while(|&c| c != '\n').count();
    (row, col)
}

/// Input box height: borders + one row, growing one row per extra line
/// up to a five-row cap (longer drafts clip).
fn input_height(row_count: usize) -> u16 {
    3 + row_count.saturating_sub(1).min(5) as u16
}

/// Braille spinner frame for an elapsed-milliseconds clock.
fn spin_frame(elapsed_ms: u128) -> char {
    const F: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    F[(elapsed_ms / 120) as usize % F.len()]
}

/// Live-region markdown re-parse gate: at most ~12 re-renders/second.
fn live_stale(cached_at: Instant, now: Instant) -> bool {
    now.duration_since(cached_at).as_millis() >= 80
}

/// Scroll one page up from the current anchor (pinned counts from the
/// tail). Keeps a two-row overlap for reading continuity.
fn page_up(scroll: &mut Option<usize>, total: usize, visible: usize) {
    if visible == 0 || total <= visible {
        *scroll = None;
        return;
    }
    let max_anchor = total - visible;
    let step = visible.saturating_sub(2).max(1);
    let cur = scroll.unwrap_or(max_anchor);
    *scroll = Some(cur.saturating_sub(step).min(max_anchor));
}

/// Scroll one page down; reaching the tail re-pins (None).
fn page_down(scroll: &mut Option<usize>, total: usize, visible: usize) {
    let Some(anchor) = *scroll else { return };
    if visible == 0 {
        *scroll = None;
        return;
    }
    let max_anchor = total.saturating_sub(visible);
    let step = visible.saturating_sub(2).max(1);
    *scroll = if anchor + step >= max_anchor {
        None
    } else {
        Some(anchor + step)
    };
}

/// Footer state shown under the editor.
#[derive(Debug, Clone, Default)]
pub struct Meters {
    /// Active model selector.
    pub model: String,
    /// Permission mode.
    pub mode: String,
    /// Active session (strand) id.
    pub session: String,
    /// Reasoning effort.
    pub effort: String,
    /// Context usage / window.
    pub context: (u64, u64),
    /// Turn cost.
    pub cost: f64,
    /// Cache-hit rate (0-1) when known.
    pub cache_hit: Option<f32>,
}

impl Meters {
    /// One-line footer string.
    pub fn footer(&self) -> String {
        let (used, window) = self.context;
        let ctx = if window > 0 {
            format!("ctx {}%", (used as f64 / window as f64 * 100.0) as u64)
        } else {
            format!("~{used} tok")
        };
        let cost = if self.cost > 0.0 {
            format!(" ${:.4}", self.cost)
        } else {
            String::new()
        };
        let cache = self
            .cache_hit
            .map(|h| format!(" cache {:.0}%", h * 100.0))
            .unwrap_or_default();
        let tag = short_session(&self.session)
            .map(|t| format!(" · #{t}"))
            .unwrap_or_default();
        let effort = if self.effort.is_empty() {
            String::new()
        } else {
            format!(" · {}", self.effort)
        };
        format!(
            "{} · {}{} · {}{}{}{}",
            self.model, self.mode, effort, ctx, cost, cache, tag
        )
    }
}

/// A pending ask (permission dialog).
#[derive(Debug, Clone)]
pub struct PendingAsk {
    /// Ask id.
    pub id: AskId,
    /// Question text.
    pub question: String,
    /// Selectable options.
    pub options: Vec<String>,
    /// Selected option index.
    pub selected: usize,
}

/// Agent summaries injected by the CLI for `/agents` (ka-term stays
/// ka-agent-free).
pub static AGENTS: std::sync::OnceLock<Vec<(String, String)>> = std::sync::OnceLock::new();

/// Short human tag for a strand id: the first 8 chars of its random tail.
pub fn short_session(id: &str) -> Option<&str> {
    id.split_once('-')
        .map(|(_, tail)| &tail[..tail.len().min(8)])
}

/// Session picker (/session, /resume): newest strands + a fresh-session
/// row, filtered by the typed substring.
#[derive(Debug, Clone)]
pub struct SessionPicker {
    /// Newest-first sessions for the cwd.
    pub sessions: Vec<ka_strand::StrandSummary>,
    /// Selected row (0 = new session).
    pub selected: usize,
    /// Typed filter (matches title or id).
    pub filter: String,
    /// The active session id (tagged in the list).
    pub current: Option<String>,
}

impl SessionPicker {
    /// Rows after filtering: (label, detail). Row 0 is always new-session.
    pub fn rows(&self) -> Vec<(String, String)> {
        let mut rows = vec![("(new session)".to_string(), "start fresh".to_string())];
        let f = self.filter.to_lowercase();
        for s in &self.sessions {
            if !f.is_empty()
                && !s.title.to_lowercase().contains(&f)
                && !s.id.to_lowercase().contains(&f)
            {
                continue;
            }
            let tag = short_session(&s.id).unwrap_or("?");
            let here = if self.current.as_ref() == Some(&s.id) {
                " · current"
            } else {
                ""
            };
            rows.push((
                format!("#{tag}  {}{here}", s.title),
                format!("{} msgs · {}", s.messages, s.ts),
            ));
        }
        rows
    }

    /// The session id the selected row switches to (None = new session).
    pub fn pick(&self) -> Option<String> {
        let rows = self.rows();
        if self.selected == 0 {
            return None;
        }
        let visible: Vec<&ka_strand::StrandSummary> = self
            .sessions
            .iter()
            .filter(|s| {
                let f = self.filter.to_lowercase();
                f.is_empty()
                    || s.title.to_lowercase().contains(&f)
                    || s.id.to_lowercase().contains(&f)
            })
            .collect();
        // selected counts only session rows after row 0
        let idx = self.selected.saturating_sub(1);
        let _ = rows;
        visible.get(idx).map(|s| s.id.clone())
    }
}

/// Settings panel (/settings): live engine options + provider registry.
#[derive(Debug, Clone)]
pub struct SettingsPanel {
    /// Model selector (editable).
    pub model: String,
    /// Permission mode.
    pub mode: ka_protocol::Mode,
    /// Effort (None = provider default).
    pub effort: Option<ka_protocol::Effort>,
    /// Selected row index.
    pub selected: usize,
    /// Inline edit buffer while editing the model.
    pub edit: Option<String>,
    /// Providers injected by the CLI.
    pub providers: Vec<ProviderInfo>,
    /// User config path (informational).
    pub config_path: String,
}

impl SettingsPanel {
    /// The editable row count (model, mode, effort).
    pub const ROWS: usize = 3;

    /// Cycle mode guarded → free → plan → guarded.
    pub fn cycle_mode(&mut self) -> ka_protocol::Mode {
        self.mode = match self.mode {
            ka_protocol::Mode::Guarded => ka_protocol::Mode::Free,
            ka_protocol::Mode::Free => ka_protocol::Mode::Plan,
            ka_protocol::Mode::Plan => ka_protocol::Mode::Guarded,
        };
        self.mode
    }

    /// Cycle effort none → low → medium → high → none.
    pub fn cycle_effort(&mut self) -> ka_protocol::Effort {
        use ka_protocol::Effort;
        self.effort = match self.effort {
            None | Some(Effort::Off) => Some(Effort::Low),
            Some(Effort::Low) => Some(Effort::Medium),
            Some(Effort::Medium) => Some(Effort::High),
            Some(Effort::High) => Some(Effort::Max),
            Some(Effort::Max) => None,
        };
        self.effort.unwrap_or(Effort::Medium)
    }
}

/// A model row for the model picker (built by the CLI from the catalog;
/// ka-term stays catalog-free).
#[derive(Debug, Clone)]
pub struct ModelInfo {
    /// Full selector id (`vendor/model`).
    pub id: String,
    /// Wire label for display.
    pub wire: String,
    /// Context window (0 = unknown).
    pub context: u32,
    /// Env var holding the API key (empty = keyless).
    pub key_env: String,
    /// Whether the key is present in this process.
    pub key_set: bool,
    /// Vendor docs URL for the key prompt (empty = unknown).
    pub doc_url: String,
    /// USD per mtok input (0 = unknown).
    pub price_in: f64,
    /// USD per mtok output (0 = unknown).
    pub price_out: f64,
    /// Price is real published pricing (footer shows cost).
    pub priced: bool,
    /// Subscription plan (no per-token cost).
    pub plan: bool,
}
/// The `/model` picker state.
#[derive(Debug, Clone)]
pub struct ModelPicker {
    pub models: Vec<ModelInfo>,
    /// Selected row.
    pub selected: usize,
    /// Typed filter (substring over the id).
    pub filter: String,
}

/// One picker row for a model. Must stay on a single line under the
/// modal's inner width (68 - 2 borders = 66) or Paragraph wraps and the
/// tail models clip off the picker — the exact bug that hid installed
/// ollama models behind two-line rows.
fn model_row(marker: &str, m: &ModelInfo, ctx: &str, key: &str) -> String {
    let id: String = m.id.chars().take(34).collect();
    let wire = m
        .wire
        .trim_end_matches("_messages")
        .trim_end_matches("_chat");
    let price = if m.plan {
        "plan".to_string()
    } else if m.priced {
        let trim = |v: f64| {
            if (v - v.round()).abs() < f64::EPSILON {
                format!("{}", v.round() as u64)
            } else {
                format!("{v}")
            }
        };
        format!("${}/${}", trim(m.price_in), trim(m.price_out))
    } else {
        "-".to_string()
    };
    format!("{marker}{id:<34} {ctx:>5} {price:<10} {wire:<7}{key}")
}

impl ModelPicker {
    /// Rows after filtering.
    pub fn rows(&self) -> Vec<&ModelInfo> {
        let f = self.filter.to_lowercase();
        self.models
            .iter()
            .filter(|m| f.is_empty() || m.id.to_lowercase().contains(&f))
            .collect()
    }

    /// The selector Enter applies: the selected row's id, or — when the
    /// filter matched nothing — the raw filter (custom provider/model).
    pub fn pick(&self) -> Option<String> {
        let rows = self.rows();
        if rows.is_empty() {
            return (!self.filter.trim().is_empty()).then(|| self.filter.trim().to_string());
        }
        rows.get(self.selected).map(|m| m.id.clone())
    }
}

/// Which modal is open (drawn above everything).
#[derive(Debug, Clone)]
pub enum Modal {
    /// Session picker.
    Session(SessionPicker),
    /// Settings panel.
    Settings(SettingsPanel),
    /// Model picker.
    Model(ModelPicker),
    /// API key prompt for a provider.
    Key(KeyPrompt),
    /// Help overlay.
    Help,
}

/// API key entry for a provider's env var.
#[derive(Debug, Clone)]
pub struct KeyPrompt {
    /// Env var the engine reads (e.g. `ZHIPU_API_KEY`).
    pub env_var: String,
    /// Vendor prefix for display.
    pub provider: String,
    /// Where to get a key (vendor docs).
    pub doc_url: String,
    /// Entered (masked) key value.
    pub input: String,
}

/// Run the TUI over an engine handle. Blocks until exit.
pub async fn run(
    mut commands: mpsc::Sender<Command>,
    mut events: mpsc::Receiver<Event>,
    initial_model: &str,
    providers: Vec<ProviderInfo>,
    models: Vec<ModelInfo>,
    agents: Vec<(String, String)>,
) -> std::io::Result<Exit> {
    let _ = AGENTS.set(agents.clone());
    let mut terminal = ratatui::init();
    // Kitty keyboard protocol: Shift+Enter as a distinct key + bracketed
    // paste. Best effort — hosts without support degrade to plain Enter;
    // Ctrl+J always works as the newline fallback.
    let _ = crossterm::execute!(
        std::io::stdout(),
        crossterm::event::PushKeyboardEnhancementFlags(
            crossterm::event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | crossterm::event::KeyboardEnhancementFlags::REPORT_EVENT_TYPES
        ),
        crossterm::event::EnableBracketedPaste
    );
    let result = app(
        &mut terminal,
        &mut commands,
        &mut events,
        initial_model,
        providers,
        models,
        agents,
    )
    .await;
    let _ = crossterm::execute!(
        std::io::stdout(),
        crossterm::event::PopKeyboardEnhancementFlags,
        crossterm::event::DisableBracketedPaste
    );
    ratatui::restore();
    result
}

async fn app(
    terminal: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    commands: &mut mpsc::Sender<Command>,
    events: &mut mpsc::Receiver<Event>,
    initial_model: &str,
    providers: Vec<ProviderInfo>,
    models: Vec<ModelInfo>,
    agents: Vec<(String, String)>,
) -> std::io::Result<Exit> {
    use crossterm::event::{Event as TermEvent, KeyCode, KeyModifiers};
    let _ = agents.clone();

    let mut transcript = Transcript::default();
    let mut scroll: Option<usize> = None;
    let mut view_rows;
    let mut input = InputBuffer::default();
    let mut meters = Meters {
        model: initial_model.to_string(),
        mode: "guarded".to_string(),
        ..Default::default()
    };
    let mut busy = false;
    let mut busy_since: Option<Instant> = None;
    let mut live_cache: Option<(String, u16, Vec<ratatui::text::Line<'static>>, Instant)> = None;
    let mut turn_ended = false;
    let mut pending: Option<PendingAsk> = None;
    let mut pending_turn_cost = 0.0f64;
    let mut turn_usage: Option<(u64, u64, u64)> = None; // (input+cache_read, total_in_seen, output)
    let mut current_assistant = String::new();
    let mut current_thought = String::new();
    let mut current_tool = String::new();
    let mut exit = None;
    let mut slash_popup: Option<SlashPopup> = None;
    let mut modal: Option<Modal> = None;
    let providers_ref = providers;
    let models_ref = models;
    let mut term_events = crossterm::event::EventStream::new();
    let mut spin = tokio::time::interval(Duration::from_millis(120));

    while exit.is_none() {
        let footer = meters.footer();
        let busy_now = busy;
        let ask = pending.clone();
        let input_snapshot = input.text.clone();
        let (term_w, term_h) = match terminal.size() {
            Ok(s) => (s.width, s.height),
            Err(_) => (80, 24),
        };
        let md_width = term_w.saturating_sub(2);
        transcript.set_width(md_width);
        let input_h = input_height(input.text.split('\n').count());
        view_rows = visible_rows(term_h, input_h);
        let live = if busy_now {
            let now = Instant::now();
            let stale = live_cache
                .as_ref()
                .map(|(_, _, _, at)| live_stale(*at, now))
                .unwrap_or(true);
            let changed = live_cache
                .as_ref()
                .is_none_or(|(t, w, _, _)| t != &current_assistant || *w != md_width);
            if changed && (stale || turn_ended) {
                live_cache = Some((
                    current_assistant.clone(),
                    md_width,
                    crate::markdown::render(&current_assistant, md_width),
                    now,
                ));
            }
            live_cache
                .as_ref()
                .map(|(_, _, rows, _)| (current_thought.clone(), rows.clone()))
        } else {
            live_cache = None;
            None
        };
        turn_ended = false;
        let cursor = input.cursor;
        terminal.draw(|frame| {
            render(
                frame,
                &transcript,
                scroll,
                &input_snapshot,
                cursor,
                &footer,
                busy_now,
                busy_since,
                Instant::now(),
                ask.as_ref(),
                live.as_ref(),
                slash_popup.as_ref(),
                modal.as_ref(),
            );
        })?;

        tokio::select! {
            biased;
            maybe_term = term_events.next() => {
                if let Some(Ok(TermEvent::Key(key))) = maybe_term {
                    if key.kind == crossterm::event::KeyEventKind::Release {
                        continue;
                    }
                    // Ask dialog captures input first
                    if let Some(ask) = pending.as_mut() {
                        match key.code {
                            KeyCode::Up | KeyCode::Left => {
                                ask.selected = ask.selected.saturating_sub(1);
                            }
                            KeyCode::Down | KeyCode::Right => {
                                if ask.selected + 1 < ask.options.len() {
                                    ask.selected += 1;
                                }
                            }
                            KeyCode::Enter | KeyCode::Char(' ') => {
                                let (id, choice) = (ask.id.clone(), ask.selected);
                                pending = None;
                                let _ = commands.send(Command::Answer { question: id, choice }).await;
                            }
                            KeyCode::Esc => {
                                let id = ask.id.clone();
                                let deny = ask.options.len().saturating_sub(1);
                                pending = None;
                                let _ = commands.send(Command::Answer { question: id, choice: deny }).await;
                            }
                            _ => {}
                        }
                        continue;
                    }
                    // Modal (session picker / settings) captures input next
                    if let Some(open) = modal.as_mut() {
                        match open {
                            Modal::Session(picker) => match key.code {
                                KeyCode::Esc => modal = None,
                                KeyCode::Up => picker.selected = picker.selected.saturating_sub(1),
                                KeyCode::Down => {
                                    let n = picker.rows().len();
                                    if picker.selected + 1 < n {
                                        picker.selected += 1;
                                    }
                                }
                                KeyCode::Backspace => {
                                    picker.filter.pop();
                                    picker.selected = 0;
                                }
                                KeyCode::Enter => {
                                    let id = picker.pick();
                                    let target = id.unwrap_or_else(|| "new".to_string());
                                    let _ = commands
                                        .send(Command::SwitchStrand { id: target })
                                        .await;
                                    modal = None;
                                    busy = true;
                                }
                                KeyCode::Char(c) => picker.filter.push(c),
                                _ => {}
                            },
                            Modal::Help => {
                                if key.code == KeyCode::Esc || key.code == KeyCode::Enter {
                                    modal = None;
                                }
                            }
                            Modal::Key(prompt) => match key.code {
                                KeyCode::Esc => modal = None,
                                KeyCode::Backspace => {
                                    prompt.input.pop();
                                }
                                KeyCode::Enter => {
                                    let value = prompt.input.trim().to_string();
                                    if !value.is_empty() {
                                        let _ = commands
                                            .send(Command::SaveApiKey {
                                                env_var: prompt.env_var.clone(),
                                                value,
                                            })
                                            .await;
                                    }
                                    modal = None;
                                }
                                KeyCode::Char(c) => prompt.input.push(c),
                                _ => {}
                            },
                            Modal::Model(picker) => match key.code {
                                KeyCode::Esc => modal = None,
                                KeyCode::Up => picker.selected = picker.selected.saturating_sub(1),
                                KeyCode::Down => {
                                    let n = picker.rows().len();
                                    if picker.selected + 1 < n {
                                        picker.selected += 1;
                                    }
                                }
                                KeyCode::Backspace => {
                                    picker.filter.pop();
                                    picker.selected = 0;
                                }
                                KeyCode::Enter => {
                                    if let Some(selector) = picker.pick() {
                                        let _ = commands
                                            .send(Command::SetModel {
                                                selector: selector.clone(),
                                            })
                                            .await;
                                        // the pick also becomes the default for
                                        // future conversations
                                        let _ = commands
                                            .send(Command::SaveSettings {
                                                model: Some(selector.clone()),
                                                effort: None,
                                                mode: None,
                                            })
                                            .await;
                                        // a keyed model without a key asks for one
                                        if let Some(m) =
                                            picker.models.iter().find(|m| m.id == selector)
                                        {
                                            if !m.key_set && !m.key_env.is_empty() {
                                                modal = Some(Modal::Key(KeyPrompt {
                                                    env_var: m.key_env.clone(),
                                                    provider: selector
                                                        .split('/')
                                                        .next()
                                                        .unwrap_or("")
                                                        .to_string(),
                                                    doc_url: m.doc_url.clone(),
                                                    input: String::new(),
                                                }));
                                                continue;
                                            }
                                        }
                                    }
                                    modal = None;
                                }
                                KeyCode::Char(c) => {
                                    picker.filter.push(c);
                                    picker.selected = 0;
                                }
                                _ => {}
                            },
            Modal::Settings(panel) => {
                                let editing = panel.edit.is_some();
                                match key.code {
                                    KeyCode::Esc => {
                                        if editing {
                                            panel.edit = None;
                                        } else {
                                            modal = None;
                                        }
                                    }
                                    KeyCode::Up if !editing => {
                                        panel.selected = panel.selected.saturating_sub(1)
                                    }
                                    KeyCode::Down if !editing => {
                                        if panel.selected + 1 < SettingsPanel::ROWS {
                                            panel.selected += 1;
                                        }
                                    }
                                    KeyCode::Backspace if editing => {
                                        if let Some(edit) = panel.edit.as_mut() {
                                            edit.pop();
                                        }
                                    }
                                    KeyCode::Char(c) if editing => {
                                        if let Some(edit) = panel.edit.as_mut() {
                                            edit.push(c);
                                        }
                                    }
                                    KeyCode::Enter => match panel.selected {
                                        0 => {
                                            if editing {
                                                let value =
                                                    panel.edit.clone().unwrap_or_default();
                                                if !value.trim().is_empty() {
                                                    panel.model = value.trim().to_string();
                                                    let _ = commands
                                                        .send(Command::SetModel {
                                                            selector: panel.model.clone(),
                                                        })
                                                        .await;
                                                }
                                                panel.edit = None;
                                            } else {
                                                panel.edit = Some(panel.model.clone());
                                            }
                                        }
                                        1 => {
                                            let mode = panel.cycle_mode();
                                            let _ =
                                                commands.send(Command::SetMode { mode }).await;
                                        }
                                        _ => {
                                            let level = panel.cycle_effort();
                                            let _ = commands
                                                .send(Command::SetEffort { level })
                                                .await;
                                        }
                                    },
                                    KeyCode::Char('s') if !editing => {
                                        // "(canned)" is the no-model placeholder,
                                        // not a selector — persisting it would
                                        // poison every later launch
                                        let model = (panel.model != "(canned)")
                                            .then(|| panel.model.clone());
                                        let _ = commands
                                            .send(Command::SaveSettings {
                                                model,
                                                effort: panel.effort,
                                                mode: Some(panel.mode),
                                            })
                                            .await;
                                    }
                                    _ => {}
                                }
                            }
                        }
                        continue;
                    }
                    match (key.code, key.modifiers) {
                        (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                            if busy {
                                let _ = commands.send(Command::Abort).await;
                            } else {
                                exit = Some(Exit::Quit);
                            }
                        }
                        (KeyCode::Esc, _) if busy => {
                            let _ = commands.send(Command::Abort).await;
                        }
                        (KeyCode::Esc, _) if scroll.is_some() => scroll = None,
                        (KeyCode::Enter, KeyModifiers::SHIFT) => input.newline(),
                        (KeyCode::Char('j'), KeyModifiers::CONTROL) => input.newline(),
                        (KeyCode::Enter, _) => {
                            let text = input.take();
                            if text.trim().is_empty() {
                                continue;
                            }
                            if let Some(cmd) = slash_command(&text) {
                                transcript.push(Line::User(text));
                                if let Some(note) = cmd.note {
                                    transcript.push(Line::Note(note));
                                }
                                        if let Some(kind) = cmd.modal {
            if kind == ModalKind::Key {
                // /key: open the prompt for the current model's provider
                let current = meters
                    .model
                    .trim_start_matches('+')
                    .split('@')
                    .next()
                    .unwrap_or("")
                    .to_string();
                let vendor = current.split('/').next().unwrap_or("").to_string();
                let info = models_ref
                    .iter()
                    .find(|m| m.id == current)
                    .or_else(|| models_ref.iter().find(|m| current.starts_with(&m.id)));
                let prompt = info.and_then(|m| {
                    (!m.key_env.is_empty()).then(|| KeyPrompt {
                        env_var: m.key_env.clone(),
                        provider: vendor.clone(),
                        doc_url: m.doc_url.clone(),
                        input: String::new(),
                    })
                });
                match prompt {
                    Some(p) => modal = Some(Modal::Key(p)),
                    None => transcript
                        .push(Line::Note("no api key variable is known for this model".into())),
                }
            } else {
                modal = Some(match kind {
                    ModalKind::Key | ModalKind::Session => {
                                            let sessions = std::env::current_dir()
                                                .ok()
                                                .and_then(|cwd| ka_strand::list(&cwd).ok())
                                                .unwrap_or_default();
                                            Modal::Session(SessionPicker {
                                                sessions,
                                                selected: 0,
                                                filter: String::new(),
                                                current: (!meters.session.is_empty())
                                                    .then(|| meters.session.clone()),
                                            })
                                        }
                                        ModalKind::Help => Modal::Help,
                                        ModalKind::Model => Modal::Model(ModelPicker {
                                            models: models_ref.clone(),
                                            selected: 0,
                                            filter: String::new(),
                                        }),
                                        ModalKind::Settings => Modal::Settings(SettingsPanel {
                                            model: meters.model.clone(),
                                            mode: match meters.mode.as_str() {
                                                "free" => ka_protocol::Mode::Free,
                                                "plan" => ka_protocol::Mode::Plan,
                                                _ => ka_protocol::Mode::Guarded,
                                            },
                                            effort: None,
                                            selected: 0,
                                            edit: None,
                                            providers: providers_ref.clone(),
                                            config_path: ka_config_path(),
                                        }),
                                    });
                            }
                        }
                                if let Some(evt) = cmd.event {
                                    let is_switch =
                                        matches!(evt, Command::SwitchStrand { .. });
                                    let _ = commands.send(evt).await;
                                    if is_switch {
                                        busy = true;
                                    }
                                }
                                if let Some(follow) = cmd.followup {
                                    transcript.push(Line::Note("(mode set; starting)".into()));
                                    busy = true;
                                    let _ = commands
                                        .send(Command::Prompt { text: follow })
                                        .await;
                                }
                                if cmd.quit {
                                    exit = Some(Exit::Quit);
                                }
                                continue;
                            }
                            transcript.push(Line::User(text.clone()));
                            let cmd = if busy {
                                // '+'-prefix defers; Enter interjects
                                if let Some(deferred) = text.strip_prefix('+') {
                                    transcript.push(Line::Note(
                                        "⏳ deferred until this turn ends".into(),
                                    ));
                                    Command::Defer { text: deferred.trim_start().to_string() }
                                } else {
                                    transcript.push(Line::Note(
                                        "⚡ steering this turn".into(),
                                    ));
                                    Command::Interject { text }
                                }
                            } else {
                                Command::Prompt { text }
                            };
                            busy = true;
                            let _ = commands.send(cmd).await;
                        }
                        (KeyCode::PageUp, _) => {
                            page_up(&mut scroll, transcript.total_rows(), view_rows);
                        }
                        (KeyCode::PageDown, _) => {
                            page_down(&mut scroll, transcript.total_rows(), view_rows);
                        }
                        (KeyCode::Tab, _) => {
                            if let Some(popup) = slash_popup.as_mut() {
                                if let Some((name, _)) = popup.items.get(popup.selected).cloned() {
                                    input.text = format!("{name} ");
                                    input.cursor = input.text.chars().count();
                                    slash_popup = update_suggestions(&input.text);
                                }
                            }
                        }
                        (KeyCode::Up, _) if slash_popup.is_some() => {
                            if let Some(popup) = slash_popup.as_mut() {
                                popup.selected = popup.selected.saturating_sub(1);
                            }
                        }
                        (KeyCode::Down, _) if slash_popup.is_some() => {
                            if let Some(popup) = slash_popup.as_mut() {
                                if popup.selected + 1 < popup.items.len() {
                                    popup.selected += 1;
                                }
                            }
                        }
                        (KeyCode::Up, _)
                            if !busy && slash_popup.is_none() && input.text.contains('\n') =>
                        {
                            input.move_up();
                        }
                        (KeyCode::Up, _) if !busy => input.history_prev(),
                        (KeyCode::Down, _)
                            if !busy && slash_popup.is_none() && input.text.contains('\n') =>
                        {
                            input.move_down();
                        }
                        (KeyCode::Down, _) if !busy => input.history_next(),
                        (KeyCode::Left, _) => input.left(),
                        (KeyCode::Right, _) => input.right(),
                        (KeyCode::Home, _) => input.home(),
                        (KeyCode::End, _) => input.end(),
                        (KeyCode::Backspace, _) => {
                            input.backspace();
                            slash_popup = update_suggestions(&input.text);
                        }
                        (KeyCode::Char(c), _) => {
                            input.insert(c);
                            slash_popup = update_suggestions(&input.text);
                        }
                        _ => {}
                    }
                } else if let Some(Ok(TermEvent::Paste(text))) = maybe_term {
                    input.insert_str(&text);
                    slash_popup = update_suggestions(&input.text);
                }
            }
            maybe_evt = events.recv() => {
                match maybe_evt {
                    None => { exit = Some(Exit::EngineEnded); }
                    Some(evt) => {
                        let replayed = matches!(evt, Event::Replay { .. });
                        apply_event(
                            &evt,
                            &mut transcript,
                            &mut busy,
                            &mut busy_since,
                            &mut meters,
                            &mut pending,
                            &mut pending_turn_cost,
                            &mut turn_usage,
                            &mut current_assistant,
                            &mut current_thought,
                            &mut current_tool,
                        );
                        if matches!(evt, Event::TurnFinished { .. }) {
                            turn_ended = true;
                        }
                        if replayed {
                            scroll = None;
                        }
                                    }
                }
            }
            _ = spin.tick(), if busy => {}
        }
    }
    Ok(exit.unwrap_or(Exit::Quit))
}

#[allow(clippy::too_many_arguments)]
fn apply_event(
    evt: &Event,
    transcript: &mut Transcript,
    busy: &mut bool,
    busy_since: &mut Option<Instant>,
    meters: &mut Meters,
    pending: &mut Option<PendingAsk>,
    pending_turn_cost: &mut f64,
    turn_usage: &mut Option<(u64, u64, u64)>,
    current_assistant: &mut String,
    current_thought: &mut String,
    current_tool: &mut String,
) {
    match evt {
        Event::TurnStarted { .. } => {
            *busy = true;
            *busy_since = Some(Instant::now());
            *current_assistant = String::new();
            *current_thought = String::new();
            *current_tool = String::new();
            *pending_turn_cost = 0.0;
            *turn_usage = None;
        }
        Event::Delta { kind } => match kind {
            ka_protocol::DeltaKind::Text(t) => current_assistant.push_str(t),
            ka_protocol::DeltaKind::Thought(t) => current_thought.push_str(t),
            ka_protocol::DeltaKind::Call { tool, .. } => {
                if !current_tool.is_empty() {
                    transcript.push(Line::Tool(std::mem::take(current_tool)));
                }
                *current_tool = format!("→ {tool}");
            }
        },
        Event::CallStarted { tool, .. } => {
            if !current_tool.is_empty() {
                transcript.push(Line::Tool(std::mem::take(current_tool)));
            }
            *current_tool = format!("→ {tool}");
        }
        Event::CallOutput {
            excerpt, is_error, ..
        } => {
            let first = excerpt.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
            let note: String = first.chars().take(120).collect();
            if *is_error {
                current_tool.push_str(&format!(" ✗ {note}"));
            } else {
                current_tool.push_str(&format!(" ✓ {note}"));
            }
        }
        Event::CallFinished { .. } => {
            if !current_tool.is_empty() {
                transcript.push(Line::Tool(std::mem::take(current_tool)));
            }
        }
        Event::Ask { id, questions } => {
            if let Some(q) = questions.first() {
                *pending = Some(PendingAsk {
                    id: id.clone(),
                    question: q.text.clone(),
                    options: q.options.clone(),
                    selected: 0,
                });
            }
        }
        Event::TurnFinished { stop: _, usage } => {
            if !current_thought.trim().is_empty() {
                transcript.push(Line::Thought(std::mem::take(current_thought)));
            }
            if !current_assistant.trim().is_empty() {
                transcript.push(Line::Assistant(std::mem::take(current_assistant)));
            }
            if !current_tool.is_empty() {
                transcript.push(Line::Tool(std::mem::take(current_tool)));
            }
            *busy = false;
            *busy_since = None;
            meters.cost += usage.cost;
            let in_seen = usage.input + usage.cache_read + usage.cache_write;
            let cache_hit = if in_seen > 0 {
                Some(usage.cache_read as f32 / in_seen as f32)
            } else {
                None
            };
            meters.cache_hit = cache_hit;
            *turn_usage = Some((in_seen, usage.input, usage.output));
            let _ = pending_turn_cost;
        }
        Event::ModelChanged { selector } => meters.model = selector.clone(),
        Event::ContextMeter { used, window } => {
            meters.context = (*used, *window);
        }
        Event::EffortChanged { level } => {
            meters.effort = format!("{level:?}").to_lowercase();
        }
        Event::SessionInfo { id } => meters.session = id.clone(),
        Event::ModeChanged { mode } => {
            meters.mode = match mode {
                ka_protocol::Mode::Guarded => "guarded".to_string(),
                ka_protocol::Mode::Free => "free".to_string(),
                ka_protocol::Mode::Plan => "plan".to_string(),
            };
        }
        Event::Error { message, .. } => {
            transcript.push(Line::Note(format!("! {message}")));
        }
        Event::Replay { messages } => {
            // a replay is the full transcript of the active session:
            // rebuild from scratch (startup on a fresh Transcript, session
            // switch replaces the previous conversation)
            transcript.clear();
            meters.cost = 0.0;
            meters.context = (0, 0);
            current_assistant.clear();
            current_thought.clear();
            current_tool.clear();
            for m in messages {
                if m.role == "user" {
                    transcript.push(Line::User(m.content.clone()));
                } else {
                    transcript.push(Line::Assistant(m.content.clone()));
                }
            }
        }
        Event::Note { message } => transcript.push(Line::Note(message.clone())),
        Event::Idle => {}
        Event::DigestStarted => transcript.push(Line::Note("⋯ digesting context…".to_string())),
        Event::DigestFinished { .. } => {}
    }
}

/// Slash-command autocomplete state.
#[derive(Debug, Clone)]
pub struct SlashPopup {
    /// (command, description) items filtered by the current prefix.
    pub items: Vec<(String, String)>,
    /// Selected item index.
    pub selected: usize,
}

/// All available slash commands: builtins + custom files.
pub fn available_slash_commands() -> Vec<(String, String)> {
    let mut cmds = vec![
        (
            "/model".to_string(),
            "switch model: picker, or vendor/model@effort".to_string(),
        ),
        (
            "/mode".to_string(),
            "set mode: guarded | free | plan".to_string(),
        ),
        (
            "/plan".to_string(),
            "research the task, write .ka/plans/plan.md".to_string(),
        ),
        ("/build".to_string(), "implement the plan file".to_string()),
        (
            "/rewind".to_string(),
            "drop the last N exchanges".to_string(),
        ),
        ("/compact".to_string(), "digest the context now".to_string()),
        (
            "/session".to_string(),
            "pick a session to resume".to_string(),
        ),
        ("/resume".to_string(), "alias for /session".to_string()),
        ("/new".to_string(), "start a fresh session".to_string()),
        (
            "/undo".to_string(),
            "restore the last edited/written file".to_string(),
        ),
        ("/help".to_string(), "commands and key bindings".to_string()),
        (
            "/settings".to_string(),
            "settings & provider status".to_string(),
        ),
        (
            "/key".to_string(),
            "set an api key for the current model".to_string(),
        ),
        ("/quit".to_string(), "exit".to_string()),
    ];
    if let Ok(cwd) = std::env::current_dir() {
        let home = std::env::var("HOME").ok();
        let mut roots = vec![
            cwd.join(".ka/commands"),
            cwd.join(".agents/commands"),
            cwd.join(".claude/commands"),
        ];
        if let Some(h) = home {
            roots.push(std::path::PathBuf::from(h).join(".config/ka/commands"));
        }
        for root in roots {
            let Ok(entries) = std::fs::read_dir(&root) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_none_or(|e| e != "md") {
                    continue;
                }
                let name = format!(
                    "/{}",
                    path.file_stem().unwrap_or_default().to_string_lossy()
                );
                if !cmds.iter().any(|(n, _)| *n == name) {
                    let desc = std::fs::read_to_string(&path)
                        .ok()
                        .and_then(|body| {
                            body.lines().find(|l| !l.trim().is_empty()).map(|l| {
                                l.trim()
                                    .trim_start_matches(['#', '>'])
                                    .trim()
                                    .chars()
                                    .take(40)
                                    .collect::<String>()
                            })
                        })
                        .filter(|d| !d.is_empty())
                        .unwrap_or_else(|| "(custom)".to_string());
                    cmds.push((name, desc));
                }
            }
        }
    }
    cmds
}

/// Recompute the suggestion popup from the raw input.
pub fn update_suggestions(input: &str) -> Option<SlashPopup> {
    if !input.starts_with('/') || input.contains(' ') {
        return None;
    }
    let items: Vec<(String, String)> = available_slash_commands()
        .into_iter()
        .filter(|(name, _)| name.starts_with(input))
        .collect();
    if items.is_empty() {
        None
    } else {
        Some(SlashPopup { items, selected: 0 })
    }
}

/// The user config path shown in the settings panel (mirrors
/// ka-agent's `config::user_config_path`).
fn ka_config_path() -> String {
    std::env::var("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
        .join("ka/ka.toml")
        .display()
        .to_string()
}

/// A parsed slash command.
struct Slash {
    event: Option<Command>,
    quit: bool,
    followup: Option<String>,
    /// Modal to open instead of sending an event.
    modal: Option<ModalKind>,
    /// Local transcript note (no engine roundtrip).
    note: Option<String>,
}

/// Modal a slash command opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModalKind {
    /// Session picker.
    Session,
    /// Settings panel.
    Settings,
    /// Model picker.
    Model,
    /// API key prompt.
    Key,
    /// Help overlay.
    Help,
}

/// Load a custom command body from `.ka/commands/<name>.md` (project) or
/// the user dir; `$ARGUMENTS` substituted with the rest of the line.
fn custom_command(head: &str, rest: Option<&str>) -> Option<String> {
    let name = head.strip_prefix('/')?;
    let cwd = std::env::current_dir().ok()?;
    let mut candidates = vec![
        cwd.join(format!(".ka/commands/{name}.md")),
        cwd.join(format!(".agents/commands/{name}.md")),
        cwd.join(format!(".claude/commands/{name}.md")),
    ];
    if let Ok(home) = std::env::var("HOME") {
        candidates
            .push(std::path::PathBuf::from(home).join(format!(".config/ka/commands/{name}.md")));
    }
    for path in candidates {
        if let Ok(body) = std::fs::read_to_string(&path) {
            let args = rest.unwrap_or("");
            return Some(body.replace("$ARGUMENTS", args));
        }
    }
    None
}

fn slash_command(text: &str) -> Option<Slash> {
    let mut parts = text.splitn(2, ' ');
    let head = parts.next()?.trim();
    let rest = parts.next().map(str::trim).filter(|s| !s.is_empty());
    // custom commands load from files; builtins win over files
    if !matches!(
        head,
        "/quit"
            | "/exit"
            | "/model"
            | "/mode"
            | "/compact"
            | "/session"
            | "/resume"
            | "/new"
            | "/settings"
            | "/key"
    ) {
        if let Some(body) = custom_command(head, rest) {
            return Some(Slash {
                note: None,
                event: Some(Command::Prompt { text: body }),
                quit: false,
                followup: None,
                modal: None,
            });
        }
    }
    match head {
        "/quit" | "/exit" => Some(Slash {
            note: None,
            event: None,
            quit: true,
            followup: None,
            modal: None,
        }),
        "/model" => match rest {
            Some(selector) => Some(Slash {
                note: None,
                event: Some(Command::SetModel {
                    selector: selector.to_string(),
                }),
                quit: false,
                followup: None,
                modal: None,
            }),
            None => Some(Slash {
                note: None,
                event: None,
                quit: false,
                followup: None,
                modal: Some(ModalKind::Model),
            }),
        },
        "/plan" => {
            let task = rest.map(str::to_string).unwrap_or_default();
            Some(Slash {
                note: None,
                event: Some(Command::SetMode {
                    mode: ka_protocol::Mode::Plan,
                }),
                quit: false,
                followup: None,
                modal: None,
            })
            .map(|mut sl| {
                sl.followup = Some(format!(
                    "Plan this task. Research the codebase with read/glob/grep/pathfinder, \\
then write a concrete numbered implementation plan to .ka/plans/plan.md. Task: {task}"
                ));
                sl
            })
        }
        "/build" => Some(Slash {
            note: None,
            event: Some(Command::SetMode {
                mode: ka_protocol::Mode::Guarded,
            }),
            quit: false,
            modal: None,
            followup: Some(
                "Switching to build mode. Read .ka/plans/plan.md and implement it step by \\
step now; verify each step."
                    .to_string(),
            ),
        }),
        "/rewind" => {
            let turns: u32 = rest.and_then(|r| r.trim().parse().ok()).unwrap_or(1);
            Some(Slash {
                note: None,
                event: Some(Command::Rewind { turns }),
                quit: false,
                followup: None,
                modal: None,
            })
        }
        "/compact" => {
            let focus = rest.map(str::to_string);
            Some(Slash {
                note: None,
                event: Some(Command::Compact { focus }),
                quit: false,
                followup: None,
                modal: None,
            })
        }
        "/session" | "/resume" => Some(Slash {
            note: None,
            event: None,
            quit: false,
            followup: None,
            modal: Some(ModalKind::Session),
        }),
        "/new" => Some(Slash {
            note: None,
            event: Some(Command::SwitchStrand {
                id: "new".to_string(),
            }),
            quit: false,
            followup: None,
            modal: None,
        }),
        "/undo" => Some(Slash {
            note: None,
            event: Some(Command::UndoFile),
            quit: false,
            followup: None,
            modal: None,
        }),
        "/agents" => Some(Slash {
            event: None,
            quit: false,
            followup: None,
            modal: None,
            note: Some(format!(
                "agents:{}",
                AGENTS
                    .get()
                    .map(|a| {
                        a.iter()
                            .map(|x| format!("\n• {} — {}", x.0, x.1))
                            .collect::<String>()
                    })
                    .unwrap_or_default()
            )),
        }),
        "/help" => Some(Slash {
            note: None,
            event: None,
            quit: false,
            followup: None,
            modal: Some(ModalKind::Help),
        }),
        "/settings" => Some(Slash {
            note: None,
            event: None,
            quit: false,
            followup: None,
            modal: Some(ModalKind::Settings),
        }),
        "/key" => Some(Slash {
            note: None,
            event: None,
            quit: false,
            followup: None,
            modal: Some(ModalKind::Key),
        }),
        "/mode" => {
            let mode = match rest? {
                "free" => ka_protocol::Mode::Free,
                "plan" => ka_protocol::Mode::Plan,
                _ => ka_protocol::Mode::Guarded,
            };
            Some(Slash {
                note: None,
                event: Some(Command::SetMode { mode }),
                quit: false,
                followup: None,
                modal: None,
            })
        }
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
fn render(
    frame: &mut ratatui::Frame,
    transcript: &Transcript,
    scroll: Option<usize>,
    input: &str,
    cursor: usize,
    footer: &str,
    busy: bool,
    busy_since: Option<Instant>,
    now: Instant,
    ask: Option<&PendingAsk>,
    live: Option<&(String, Vec<ratatui::text::Line<'static>>)>,
    popup: Option<&SlashPopup>,
    modal: Option<&Modal>,
) {
    use ratatui::layout::Constraint::{Length, Min};
    use ratatui::style::{Modifier, Style};
    use ratatui::text::{Line as TuiLine, Span};
    use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

    let chunks = ratatui::layout::Layout::vertical([
        Min(3),
        Length(input_height(input.split('\n').count())),
        Length(1),
    ])
    .split(frame.area());

    // ── transcript: cached rows + live region under a scroll window ──
    let mut live_rows: Vec<TuiLine> = Vec::new();
    if let Some((thought, md_rows)) = live {
        if !thought.trim().is_empty() {
            let tail: Vec<&str> = thought
                .lines()
                .rev()
                .take(5)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            for l in tail {
                live_rows.push(TuiLine::styled(format!("⋯ {l}"), crate::palette::THOUGHT));
            }
        }
        // cached markdown rows (clone-on-append; the cache stays cursor-free)
        // live markdown rows get the same surface as final output; the
        // cursor is appended first so the surface pads around it
        if !md_rows.is_empty() {
            let mut md_live: Vec<TuiLine> = md_rows.to_vec();
            if let Some(last) = md_live.last_mut() {
                last.spans
                    .push(Span::styled("▌", crate::palette::ACCENT_STYLE));
            }
            apply_output_surface(
                &mut md_live,
                frame.area().width.saturating_sub(2),
                crate::palette::BG_OUTPUT,
            );
            live_rows.extend(md_live);
        }
    }

    let cached = transcript.total_rows();
    let total = cached + live_rows.len();
    let visible = chunks[0].height.saturating_sub(1) as usize;
    debug_assert_eq!(
        visible,
        visible_rows(frame.area().height, chunks[1].height),
        "viewport arithmetic must agree with the layout"
    );
    let (start, pinned) = window_range(total, visible, scroll);
    let end = (start + visible).min(total);
    let mut window: Vec<TuiLine> = Vec::with_capacity(end - start);
    for i in start..end {
        if i < cached {
            if let Some(row) = transcript.row(i) {
                window.push(row.clone());
            }
        } else if let Some(row) = live_rows.get(i - cached) {
            window.push(row.clone());
        }
    }
    let title = if pinned {
        "ka".to_string()
    } else {
        format!("ka · ↑{} above (pgdn/esc)", start)
    };
    let widget = Paragraph::new(window).block(
        Block::default()
            .borders(Borders::TOP)
            .title(ratatui::text::Line::from(title).style(crate::palette::META))
            .border_style(crate::palette::BORDER)
            .padding(ratatui::widgets::Padding::horizontal(1)),
    );
    frame.render_widget(widget, chunks[0]);

    // ── input ─────────────────────────────────────────────────────
    let title = if busy {
        "input (enter=interject, +=defer, esc=abort)"
    } else {
        "input · ⏎ send · ⇧⏎/ctrl+j newline"
    };
    let input_border = if modal.is_some() || popup.is_some() {
        crate::palette::META
    } else if busy {
        ratatui::style::Style::new().fg(crate::palette::WARN)
    } else {
        crate::palette::BORDER
    };
    // shared horizontal window: all rows shift together so the cursor
    // row can always show the cursor
    let (cur_row, cur_col) = cursor_row_col(input, cursor);
    let inner_w = chunks[1].width.saturating_sub(2) as usize;
    let scroll_col = cur_col.saturating_sub(inner_w.saturating_sub(1).max(1));
    let body: Vec<TuiLine> = if input.is_empty() && !busy && popup.is_none() && modal.is_none() {
        vec![TuiLine::styled(
            "ask ka · / for commands · ⇧⏎ newline",
            crate::palette::PLACEHOLDER,
        )]
    } else {
        input
            .split('\n')
            .map(|r| TuiLine::from(r.chars().skip(scroll_col).collect::<String>()))
            .collect()
    };
    let input_widget = Paragraph::new(body).block(
        Block::default()
            .borders(Borders::ALL)
            .title(ratatui::text::Line::from(title).style(crate::palette::META))
            .border_style(input_border),
    );
    frame.render_widget(input_widget, chunks[1]);
    let area = chunks[1];
    frame.set_cursor_position((
        area.x + 1 + (cur_col - scroll_col) as u16,
        area.y + 1 + cur_row.min(area.height.saturating_sub(2) as usize) as u16,
    ));

    // ── footer ────────────────────────────────────────────────────
    let status = match busy_since {
        Some(t0) if busy => format!(
            " {} working",
            spin_frame(now.duration_since(t0).as_millis())
        ),
        _ => String::new(),
    };
    let footer_line = TuiLine::from(vec![
        Span::styled(footer.to_string(), crate::palette::META),
        Span::styled(status, crate::palette::ACCENT_BOLD),
    ]);
    frame.render_widget(Paragraph::new(footer_line), chunks[2]);

    // ── slash autocomplete popup (above input) ────────────────────
    if let Some(popup) = popup {
        // border(2) + up to 7 items + 1 hint row
        let rows = popup.items.len().min(7) as u16 + 3;
        let rect = ratatui::layout::Rect {
            x: chunks[1].x,
            y: chunks[1].y.saturating_sub(rows),
            width: (chunks[1].width).min(56),
            height: rows,
        };
        frame.render_widget(Clear, rect);
        let mut text = Vec::new();
        for (i, (name, desc)) in popup.items.iter().take(7).enumerate() {
            let marker = if i == popup.selected { "▶ " } else { "  " };
            let style = if i == popup.selected {
                crate::palette::ACCENT_BOLD
            } else {
                ratatui::style::Style::default()
            };
            let desc_trim: String = desc.chars().take(32).collect();
            text.push(TuiLine::styled(
                format!("{marker}{name:<12} {desc_trim}"),
                style,
            ));
        }
        text.push(TuiLine::styled(
            "tab complete · ↑↓ select",
            crate::palette::META,
        ));
        let widget = Paragraph::new(text)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(ratatui::text::Line::from("commands").style(crate::palette::META))
                    .border_style(crate::palette::BORDER),
            )
            .wrap(Wrap { trim: false });
        frame.render_widget(widget, rect);
    }

    // ── ask modal ─────────────────────────────────────────────────
    if let Some(ask) = ask {
        let rect = centered(40, 8, frame.area());
        frame.render_widget(Clear, rect);
        let mut text = vec![TuiLine::styled(
            ask.question.clone(),
            ratatui::style::Style::new()
                .fg(crate::palette::WARN)
                .add_modifier(Modifier::BOLD),
        )];
        for (i, opt) in ask.options.iter().enumerate() {
            let marker = if i == ask.selected { "▶ " } else { "  " };
            let style = if i == ask.selected {
                crate::palette::ACCENT_BOLD
            } else {
                ratatui::style::Style::default()
            };
            text.push(TuiLine::styled(format!("{marker}{opt}"), style));
        }
        text.push(TuiLine::styled(
            "↑↓ select · enter confirm · esc deny",
            crate::palette::META,
        ));
        let widget = Paragraph::new(text)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(ratatui::text::Line::from("permission").style(crate::palette::META))
                    .border_style(crate::palette::BORDER),
            )
            .wrap(Wrap { trim: true });
        frame.render_widget(widget, rect);
    }

    // ── modals (session picker / settings) ────────────────────────
    if let Some(open) = modal {
        match open {
            Modal::Session(picker) => {
                let rows = picker.rows();
                let height = (rows.len() as u16 + 4).min(20);
                let width = 68.min(frame.area().width);
                let rect = centered(width, height, frame.area());
                frame.render_widget(Clear, rect);
                let mut text = vec![TuiLine::from(vec![
                    Span::styled("filter: ", ratatui::style::Style::default()),
                    Span::styled(picker.filter.clone(), crate::palette::ACCENT_STYLE),
                ])];
                for (i, (label, detail)) in rows.iter().take((height as usize) - 4).enumerate() {
                    let marker = if i == picker.selected { "▶ " } else { "  " };
                    let style = if i == picker.selected {
                        crate::palette::ACCENT_BOLD
                    } else {
                        ratatui::style::Style::default()
                    };
                    text.push(TuiLine::styled(
                        format!("{marker}{label}  —  {detail}"),
                        style,
                    ));
                }
                text.push(TuiLine::styled(
                    "type to filter · enter switch · esc close",
                    crate::palette::META,
                ));
                let widget = Paragraph::new(text)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(
                                ratatui::text::Line::from("sessions").style(crate::palette::META),
                            )
                            .border_style(crate::palette::BORDER),
                    )
                    .wrap(Wrap { trim: false });
                frame.render_widget(widget, rect);
            }
            Modal::Key(prompt) => {
                // full-frame wipe: switching here from the model picker must
                // leave no stale pixels outside the prompt rect
                frame.render_widget(Clear, frame.area());
                let height = 9u16.min(frame.area().height.saturating_sub(2));
                let width = 64.min(frame.area().width);
                let rect = centered(width, height, frame.area());
                frame.render_widget(Clear, rect);
                let mut text = vec![TuiLine::styled("api key", crate::palette::ACCENT_BOLD)];
                text.push(TuiLine::from(vec![
                    Span::styled("provider  ", crate::palette::META),
                    Span::styled(prompt.provider.clone(), Style::default()),
                ]));
                text.push(TuiLine::from(vec![
                    Span::styled("key var   ", crate::palette::META),
                    Span::styled(prompt.env_var.clone(), Style::default()),
                ]));
                if !prompt.doc_url.is_empty() {
                    let doc: String = prompt.doc_url.chars().take(width as usize - 4).collect();
                    text.push(TuiLine::from(vec![
                        Span::styled("get one   ", crate::palette::META),
                        Span::styled(doc, crate::palette::CYAN),
                    ]));
                }
                text.push(TuiLine::default());
                let masked = "•".repeat(prompt.input.chars().count());
                text.push(TuiLine::from(vec![
                    Span::styled("value     ", crate::palette::META),
                    Span::styled(masked, Style::default()),
                    Span::styled("▌", crate::palette::ACCENT_STYLE),
                ]));
                text.push(TuiLine::default());
                text.push(TuiLine::styled(
                    "enter save · esc cancel",
                    crate::palette::META,
                ));
                let widget = Paragraph::new(text)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(ratatui::text::Line::from("api key").style(crate::palette::META))
                            .border_style(crate::palette::BORDER),
                    )
                    .wrap(Wrap { trim: false });
                frame.render_widget(widget, rect);
            }
            Modal::Help => {
                let height = 22u16.min(frame.area().height.saturating_sub(2));
                let width = 66.min(frame.area().width);
                let rect = centered(width, height, frame.area());
                frame.render_widget(Clear, rect);
                let keys = vec![
                    ("Enter", "send · interject mid-turn"),
                    ("+text", "defer until this turn ends"),
                    ("Esc / Ctrl-C", "abort turn · close overlays · unpin scroll"),
                    ("PgUp / PgDn", "scroll the transcript"),
                    ("↑ ↓", "history · navigate pickers"),
                    ("Tab", "complete slash command"),
                ];
                let mut text = Vec::new();
                for (k, v) in keys {
                    text.push(TuiLine::from(vec![
                        Span::styled(format!("{k:<12} "), crate::palette::ACCENT_STYLE),
                        Span::styled(v.to_string(), ratatui::style::Style::default()),
                    ]));
                }
                text.push(TuiLine::default());
                text.push(TuiLine::styled(
                    "commands:",
                    ratatui::style::Style::default(),
                ));
                for (name, desc) in available_slash_commands() {
                    text.push(TuiLine::from(vec![
                        Span::styled(format!("{name:<12} "), crate::palette::ACCENT_STYLE),
                        Span::styled(desc, ratatui::style::Style::default()),
                    ]));
                }
                let widget = Paragraph::new(text)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(ratatui::text::Line::from("help").style(crate::palette::META))
                            .border_style(crate::palette::BORDER),
                    )
                    .wrap(Wrap { trim: false });
                frame.render_widget(widget, rect);
            }
            Modal::Model(picker) => {
                let rows = picker.rows();
                let height = (rows.len() as u16 + 4).min(18);
                let width = 68.min(frame.area().width);
                let rect = centered(width, height, frame.area());
                frame.render_widget(Clear, rect);
                let mut text = vec![TuiLine::from(vec![
                    Span::styled("filter: ", ratatui::style::Style::default()),
                    Span::styled(picker.filter.clone(), crate::palette::ACCENT_STYLE),
                ])];
                let cap = (height as usize).saturating_sub(4);
                for (i, m) in rows.iter().take(cap).enumerate() {
                    let marker = if i == picker.selected { "▶ " } else { "  " };
                    let style = if i == picker.selected {
                        crate::palette::ACCENT_BOLD
                    } else {
                        ratatui::style::Style::default()
                    };
                    let ctx = if m.context > 0 {
                        format!("{}k", m.context / 1000)
                    } else {
                        "?".to_string()
                    };
                    let key = if m.key_env.is_empty() {
                        String::new()
                    } else if m.key_set {
                        " ✓".to_string()
                    } else {
                        " ✗".to_string()
                    };
                    text.push(TuiLine::styled(model_row(marker, m, &ctx, &key), style));
                }
                if rows.is_empty() {
                    if picker.filter.trim().is_empty() {
                        text.push(TuiLine::styled(
                            "(no models; type a vendor/model selector)",
                            crate::palette::META,
                        ));
                    } else {
                        text.push(TuiLine::styled(
                            format!("enter sets '{}' as a custom selector", picker.filter),
                            crate::palette::WARN,
                        ));
                    }
                }
                text.push(TuiLine::styled(
                    if rows.len() > cap {
                        "more rows — type to filter · enter switch · esc close"
                    } else {
                        "type to filter · enter switch · esc close"
                    },
                    crate::palette::META,
                ));
                let widget = Paragraph::new(text)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(ratatui::text::Line::from("model").style(crate::palette::META))
                            .border_style(crate::palette::BORDER),
                    )
                    .wrap(Wrap { trim: false });
                frame.render_widget(widget, rect);
            }
            Modal::Settings(panel) => {
                let height = (SettingsPanel::ROWS + panel.providers.len() + 6) as u16;
                let height = height.min(frame.area().height.saturating_sub(2));
                let width = 72.min(frame.area().width);
                let rect = centered(width, height, frame.area());
                frame.render_widget(Clear, rect);
                let sel = crate::palette::ACCENT_BOLD;
                let dim = ratatui::style::Style::new().fg(crate::palette::MUTED);
                let marker =
                    |i: usize| -> &'static str { if i == panel.selected { "▶ " } else { "  " } };
                let mode_str = match panel.mode {
                    ka_protocol::Mode::Guarded => "guarded",
                    ka_protocol::Mode::Free => "free",
                    ka_protocol::Mode::Plan => "plan",
                };
                let effort_str = panel
                    .effort
                    .as_ref()
                    .map(|e| format!("{e:?}").to_lowercase())
                    .unwrap_or_else(|| "(default)".to_string());
                let editing = panel.edit.is_some();
                // while editing, the row shows a cursor block so the mode
                // change is unmistakable (nothing else on screen moves)
                let model_row = match &panel.edit {
                    Some(buf) => format!("{}model ✎  {}▌", marker(0), buf),
                    None => format!("{}model    {}", marker(0), panel.model),
                };
                let model_style = if editing {
                    ratatui::style::Style::new()
                        .fg(crate::palette::WARN)
                        .add_modifier(Modifier::BOLD)
                } else if panel.selected == 0 {
                    sel
                } else {
                    dim
                };
                let mut text = vec![
                    TuiLine::styled(model_row, model_style),
                    TuiLine::styled(
                        format!("{}mode     {}", marker(1), mode_str),
                        if panel.selected == 1 { sel } else { dim },
                    ),
                    TuiLine::styled(
                        format!("{}effort   {}", marker(2), effort_str),
                        if panel.selected == 2 { sel } else { dim },
                    ),
                    TuiLine::styled(
                        format!("config: {}", panel.config_path),
                        crate::palette::META,
                    ),
                ];
                // hint sits above the providers so it survives clipping
                // on short terminals
                let hint = if editing {
                    "typing edits the selector · enter apply · esc cancel"
                } else {
                    "enter edit/cycle · s save to config · esc close"
                };
                text.push(TuiLine::styled(hint, crate::palette::META));
                text.push(TuiLine::styled(
                    "providers:",
                    ratatui::style::Style::default(),
                ));
                let url_room = (width.saturating_sub(38)) as usize;
                // keyed providers first; cap the list so the panel stays
                // readable with a large catalog behind it
                let mut providers: Vec<&ProviderInfo> = panel.providers.iter().collect();
                providers.sort_by_key(|p| !p.key_set && !p.env_var.is_empty());
                const CAP: usize = 18;
                let (shown, rest) = providers.split_at(CAP.min(providers.len()));
                for p in shown {
                    let key = if p.env_var.is_empty() {
                        "(keyless)".to_string()
                    } else if p.key_set {
                        format!("{} ✓", p.env_var)
                    } else {
                        format!("{} ✗", p.env_var)
                    };
                    let url: String = p.base_url.chars().take(url_room).collect();
                    text.push(TuiLine::from(vec![
                        Span::styled(format!("  {:<12}", p.name), dim),
                        Span::styled(
                            key,
                            if p.key_set || p.env_var.is_empty() {
                                crate::palette::OK
                            } else {
                                crate::palette::ERR
                            },
                        ),
                        Span::styled(format!("  {url}"), crate::palette::META),
                    ]));
                }
                if !rest.is_empty() {
                    text.push(TuiLine::styled(
                        format!("  +{} more (see `ka providers`)", rest.len()),
                        crate::palette::META,
                    ));
                }
                let widget = Paragraph::new(text)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(
                                ratatui::text::Line::from("settings").style(crate::palette::META),
                            )
                            .border_style(crate::palette::BORDER),
                    )
                    .wrap(Wrap { trim: false });
                frame.render_widget(widget, rect);
            }
        }
    }
}

/// Merge a background into every span and pad each row to the full
/// transcript width — the assistant "slab", a fill with no border glyphs.
fn apply_output_surface(
    rows: &mut [ratatui::text::Line<'static>],
    width: u16,
    bg: ratatui::style::Color,
) {
    use unicode_width::UnicodeWidthStr;
    for row in rows.iter_mut() {
        for s in row.spans.iter_mut() {
            s.style = s.style.bg(bg);
        }
        let used: usize = row.spans.iter().map(|s| s.content.width()).sum();
        let w = width as usize;
        if w > used {
            row.spans.push(ratatui::text::Span::styled(
                " ".repeat(w - used),
                ratatui::style::Style::new().bg(bg),
            ));
        }
    }
}

/// An empty row carrying only the surface background. The bg must live on
/// a SPAN: Paragraph ignores line-level styles when painting cells.
fn surface_blank(width: u16, bg: ratatui::style::Color) -> ratatui::text::Line<'static> {
    ratatui::text::Line::from(vec![ratatui::text::Span::styled(
        " ".repeat(width as usize),
        ratatui::style::Style::new().bg(bg),
    )])
}

/// Full-width band for user messages — the only edge-to-edge role
/// (OMP userMsgBg: warm dark, amber prompt glyph, default text).
fn push_block(out: &mut Vec<ratatui::text::Line<'static>>, text: &str, width: u16) {
    use ratatui::text::Line as TuiLine;
    use ratatui::text::Span;

    let lead_style = ratatui::style::Style::new()
        .fg(crate::palette::ACCENT)
        .bg(crate::palette::BG_USER);
    let body_style = ratatui::style::Style::new().bg(crate::palette::BG_USER);
    // the band spans the full transcript width, edge to edge
    let usable = width as usize;
    for (li, raw) in text.lines().enumerate() {
        // wrap long lines at the band width (char boundary)
        let mut start = 0;
        let chars: Vec<char> = raw.chars().collect();
        loop {
            // the prefix goes on the very first segment of the message;
            // every later segment (wrapped or a new source line) indents
            let first = li == 0 && start == 0;
            let lead = if first { "❯ " } else { "  " };
            // max(1) guarantees forward progress even at degenerate
            // widths (0/1 columns) where the lead alone overflows
            let room = usable.saturating_sub(2).max(1);
            let end = (start + room).min(chars.len());
            let segment: String = chars[start..end].iter().collect();
            let pad = usable.saturating_sub(segment.chars().count() + 2);
            let mut spans = vec![
                Span::styled(lead.to_string(), lead_style),
                Span::styled(segment, body_style),
            ];
            if pad > 0 {
                spans.push(Span::styled(" ".repeat(pad), body_style));
            }
            out.push(TuiLine::from(spans));
            if end >= chars.len() {
                break;
            }
            start = end;
        }
    }
    out.push(surface_blank(width, crate::palette::BG_USER)); // spacing after each block
}

/// Gutter-prefixed rows for ambient roles (thought/tool/note): no
/// background, no full-width padding.
fn push_gutter(
    out: &mut Vec<ratatui::text::Line<'static>>,
    text: &str,
    width: u16,
    prefix: &str,
    style: ratatui::style::Style,
) {
    use ratatui::text::Line as TuiLine;

    let usable = width as usize;
    for (li, raw) in text.lines().enumerate() {
        // wrap long lines at the transcript width (char boundary)
        let mut start = 0;
        let chars: Vec<char> = raw.chars().collect();
        loop {
            let first = li == 0 && start == 0;
            let lead = if first { prefix } else { "  " };
            // max(1) guarantees forward progress even at degenerate
            // widths (0/1 columns) where the lead alone overflows
            let room = usable.saturating_sub(lead.chars().count()).max(1);
            let end = (start + room).min(chars.len());
            let segment: String = chars[start..end].iter().collect();
            out.push(TuiLine::styled(format!("{lead}{segment}"), style));
            if end >= chars.len() {
                break;
            }
            start = end;
        }
    }
    out.push(TuiLine::default()); // spacing after each entry
}

fn centered(width: u16, height: u16, area: ratatui::layout::Rect) -> ratatui::layout::Rect {
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    ratatui::layout::Rect {
        x,
        y,
        width: width.min(area.width),
        height: height.min(area.height),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn model(id: &str, context: u32) -> ModelInfo {
        ModelInfo {
            id: id.to_string(),
            wire: "openai_chat".to_string(),
            context,
            key_env: "X_API_KEY".to_string(),
            key_set: false,
            doc_url: String::new(),
            price_in: 0.0,
            price_out: 0.0,
            priced: false,
            plan: false,
        }
    }

    #[test]
    fn model_picker_filters_picks_and_falls_back_to_custom() {
        let picker = ModelPicker {
            models: vec![
                model("ollama/qwen3-32b", 131_072),
                model("openai/gpt-5.1", 400_000),
                model("anthropic/claude-sonnet-5", 200_000),
            ],
            selected: 0,
            filter: String::new(),
        };
        assert_eq!(picker.rows().len(), 3);
        assert_eq!(picker.pick().as_deref(), Some("ollama/qwen3-32b"));

        let mut filtered = picker.clone();
        filtered.filter = "gpt".to_string();
        assert_eq!(filtered.rows().len(), 1);
        assert_eq!(filtered.pick().as_deref(), Some("openai/gpt-5.1"));

        // no match: Enter sets the filter itself as a custom selector
        let mut custom = picker.clone();
        custom.filter = "groq/llama-3.3-70b".to_string();
        assert!(custom.rows().is_empty());
        assert_eq!(
            custom.pick().as_deref(),
            Some("groq/llama-3.3-70b"),
            "unmatched filter becomes the selector"
        );

        // no match and no filter: nothing to apply
        let mut empty = picker.clone();
        empty.filter = "   ".to_string();
        assert!(empty.rows().is_empty());
        assert_eq!(empty.pick(), None);

        // selection follows the filtered view
        let mut second = picker.clone();
        second.filter = "claude".to_string();
        second.selected = 0;
        assert_eq!(second.pick().as_deref(), Some("anthropic/claude-sonnet-5"));
    }

    #[test]
    fn model_rows_stay_single_line() {
        let long = model(
            "ollama/some-extremely-long-model-name:with-tag-and-more",
            1_000_000,
        );
        let row = model_row("▶ ", &long, "1000k", " ✗");
        assert!(
            row.chars().count() <= 66,
            "row too wide: {} {}",
            row.chars().count(),
            row
        );
        let short = model("ollama/qwen3.5:9b", 262_144);
        let row = model_row("  ", &short, "262k", "");
        assert!(row.contains("qwen3.5:9b"));
        assert!(row.contains("openai"), "wire abbreviated: {row}");
    }

    #[test]
    fn model_slash_opens_picker_or_sets_directly() {
        assert!(matches!(
            slash_command("/model"),
            Some(Slash {
                note: None,
                modal: Some(ModalKind::Model),
                event: None,
                ..
            })
        ));
        assert!(matches!(
            slash_command("/model groq/llama-3.3-70b"),
            Some(Slash {
                note: None,
                event: Some(Command::SetModel { selector }),
                ..
            }) if selector == "groq/llama-3.3-70b"
        ));
    }

    #[test]
    fn pushes_at_degenerate_width_terminate() {
        let mut t = Transcript::default(); // width 0 until first draw
        t.push(Line::User("some longer text".into()));
        t.push(Line::Note("x".repeat(50)));
        assert!(t.total_rows() >= 2);
    }

    #[test]
    fn transcript_renders_once_per_entry_and_resizes() {
        let mut t = Transcript::default();
        t.set_width(40);
        t.push(Line::User("hello".into()));
        t.push(Line::Note("note".into()));
        t.push(Line::Tool("→ read".into()));
        assert_eq!(t.render_passes(), 3, "one pass per pushed entry");
        t.set_width(40); // same width: no rebuild
        assert_eq!(t.render_passes(), 3, "same width must not re-render");
        t.set_width(72); // resize: one pass per entry again
        assert_eq!(t.render_passes(), 6, "resize rebuilds the cache");
        assert!(t.total_rows() > 0);
        assert!(t.row(0).is_some());
        assert!(t.row(t.total_rows()).is_none());
        t.clear();
        assert_eq!(t.total_rows(), 0);
    }

    #[test]
    fn window_range_pins_and_clamps() {
        // everything fits: always pinned from 0
        assert_eq!(window_range(10, 20, None), (0, true));
        assert_eq!(window_range(10, 20, Some(5)), (0, true));
        // pinned tail
        assert_eq!(window_range(100, 20, None), (80, true));
        // anchored mid-history
        assert_eq!(window_range(100, 20, Some(30)), (30, false));
        // anchor past the tail re-pins
        assert_eq!(window_range(100, 20, Some(95)), (80, true));
        // zero visible
        assert_eq!(window_range(100, 0, Some(10)), (0, true));
    }
    #[test]
    fn visible_rows_carves_out_chrome() {
        // input area + footer + transcript top border
        assert_eq!(visible_rows(24, 3), 19);
        assert_eq!(visible_rows(5, 3), 0, "never underflows");
        assert_eq!(visible_rows(0, 0), 0);
        // growth of the input eats the viewport one row at a time
        assert_eq!(visible_rows(24, 8), 14);
    }

    #[test]
    fn paging_roundtrip_repins_at_tail() {
        let mut scroll = None;
        page_up(&mut scroll, 100, 20);
        assert_eq!(scroll, Some(80 - 18), "pinned PgUp lands a page up");
        page_up(&mut scroll, 100, 20);
        assert_eq!(scroll, Some(80 - 36));
        page_down(&mut scroll, 100, 20);
        assert_eq!(scroll, Some(80 - 18));
        page_down(&mut scroll, 100, 20);
        assert_eq!(scroll, None, "reaching the tail re-pins");
        // small transcript: PgUp is a no-op
        let mut small = None;
        page_up(&mut small, 5, 20);
        assert_eq!(small, None);
    }

    #[test]
    fn short_session_takes_tail() {
        assert_eq!(short_session("s19a4f2e1b0-3f9c2a81d4b7"), Some("3f9c2a81"));
        assert_eq!(short_session("no-tail"), Some("tail"));
        assert_eq!(short_session(""), None);
    }

    #[test]
    fn session_picker_marks_the_current_session() {
        let picker = SessionPicker {
            current: Some("s1-bbbb22221111".to_string()),
            sessions: vec![
                ka_strand::StrandSummary {
                    path: std::path::PathBuf::from("/tmp/a.jsonl"),
                    id: "s1-aaaa11112222".to_string(),
                    ts: "2026-01-02T00:00:00Z".to_string(),
                    title: "older session".to_string(),
                    messages: 2,
                },
                ka_strand::StrandSummary {
                    path: std::path::PathBuf::from("/tmp/b.jsonl"),
                    id: "s1-bbbb22221111".to_string(),
                    ts: "2026-01-01T00:00:00Z".to_string(),
                    title: "active session".to_string(),
                    messages: 5,
                },
            ],
            selected: 0,
            filter: String::new(),
        };
        let rows = picker.rows();
        assert!(rows[1].0.contains("older session") && !rows[1].0.contains("current"));
        assert!(rows[2].0.contains("active session") && rows[2].0.contains("· current"));
    }

    #[test]
    fn session_picker_filters_and_picks() {
        let picker = SessionPicker {
            current: None,
            sessions: vec![ka_strand::StrandSummary {
                path: std::path::PathBuf::from("/tmp/a.jsonl"),
                id: "s1-aaaa11112222".to_string(),
                ts: "2026-01-01T00:00:00Z".to_string(),
                title: "fix the parser".to_string(),
                messages: 4,
            }],
            selected: 1,
            filter: "parser".to_string(),
        };
        let rows = picker.rows();
        assert_eq!(rows.len(), 2, "new-session row + the match");
        assert!(rows[1].0.contains("fix the parser"));
        assert_eq!(picker.pick().as_deref(), Some("s1-aaaa11112222"));

        let mut miss = picker.clone();
        miss.filter = "nomatch".to_string();
        assert_eq!(miss.rows().len(), 1);
        assert_eq!(miss.pick(), None, "only the new-session row remains");

        let mut fresh = picker.clone();
        fresh.selected = 0;
        assert_eq!(fresh.pick(), None, "row 0 = new session");
    }

    #[test]
    fn settings_save_skips_canned_model_placeholder() {
        // mirrored logic from the 's' handler: the guard is one expression
        let panel_model = "(canned)";
        let model = (panel_model != "(canned)").then(|| panel_model.to_string());
        assert_eq!(model, None);
        let model = (panel_model == "x/y").then(|| panel_model.to_string());
        assert_eq!(model, None);
        let real = "groq/llama-3.3-70b";
        let model = (real != "(canned)").then(|| real.to_string());
        assert_eq!(model.as_deref(), Some("groq/llama-3.3-70b"));
    }

    #[test]
    fn settings_effort_cycles_all_variants() {
        use ka_protocol::Effort;
        let mut panel = SettingsPanel {
            model: "x/y".to_string(),
            mode: ka_protocol::Mode::Guarded,
            effort: None,
            selected: 0,
            edit: None,
            providers: vec![],
            config_path: String::new(),
        };
        let seq = [
            Some(Effort::Low),
            Some(Effort::Medium),
            Some(Effort::High),
            Some(Effort::Max),
            None,
        ];
        for expected in seq {
            panel.cycle_effort();
            assert_eq!(panel.effort, expected);
        }
    }

    #[test]
    fn slash_commands_include_session_and_settings() {
        let names: Vec<String> = available_slash_commands()
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        for want in ["/session", "/resume", "/new", "/settings"] {
            assert!(names.contains(&want.to_string()), "missing {want}");
        }
        assert!(matches!(
            slash_command("/new"),
            Some(Slash {
                note: None,
                event: Some(Command::SwitchStrand { id }),
                ..
            }) if id == "new"
        ));
        assert!(matches!(
            slash_command("/settings"),
            Some(Slash {
                note: None,
                modal: Some(ModalKind::Settings),
                ..
            })
        ));
        assert!(matches!(
            slash_command("/resume"),
            Some(Slash {
                note: None,
                modal: Some(ModalKind::Session),
                ..
            })
        ));
    }

    #[test]
    fn input_buffer_edits_and_history() {
        let mut b = InputBuffer::default();
        for c in "héllo".chars() {
            b.insert(c);
        }
        assert_eq!(b.text, "héllo");
        b.left();
        b.backspace(); // cursor was between 'l' and 'o': removes the second 'l'
        assert_eq!(b.text, "hélo");
        assert_eq!(b.cursor, 3);
        b.end();
        b.insert('!');
        assert_eq!(b.text, "hélo!");
        let taken = b.take();
        assert_eq!(taken, "hélo!");
        assert!(b.text.is_empty());

        b.insert('x');
        let _ = b.take();
        b.history_prev();
        assert_eq!(b.text, "x");
        b.history_prev();
        assert_eq!(b.text, "hélo!");
        b.history_next();
        b.history_next();
        assert!(b.text.is_empty());
    }
    #[test]
    fn newline_and_backspace_roundtrip() {
        let mut b = InputBuffer::default();
        for c in "ab".chars() {
            b.insert(c);
        }
        b.newline();
        for c in "cd".chars() {
            b.insert(c);
        }
        assert_eq!(b.text, "ab\ncd");
        assert_eq!(b.rows(), vec!["ab", "cd"]);
        b.backspace();
        b.backspace();
        b.backspace();
        assert_eq!(b.text, "ab", "backspaces remove c, d, then the newline");
    }

    #[test]
    fn cursor_row_col_tracks_newlines() {
        let mut b = InputBuffer::default();
        for c in "ab\ncd\né".chars() {
            b.insert(c);
        }
        assert_eq!(b.cursor_row_col(), (2, 1));
        b.left();
        b.left();
        assert_eq!(b.cursor_row_col(), (1, 2));
        b.home();
        assert_eq!(b.cursor_row_col(), (0, 0));
    }

    #[test]
    fn insert_str_normalizes_crlf() {
        let mut b = InputBuffer::default();
        b.insert_str("pasted\r\nmulti\rline");
        assert_eq!(b.text, "pasted\nmulti\nline");
        assert_eq!(b.cursor_row_col(), (2, 4));
        b.insert('!');
        assert_eq!(b.text, "pasted\nmulti\nline!");
    }

    #[test]
    fn multiline_history_bypass() {
        let mut b = InputBuffer::default();
        for c in "old".chars() {
            b.insert(c);
        }
        b.take(); // history: ["old"]
        for c in "one\ntwo".chars() {
            b.insert(c);
        }
        assert_eq!(b.cursor_row_col(), (1, 3));
        // Up on multiline navigates rows, never history
        assert!(b.move_up());
        assert_eq!(b.cursor_row_col(), (0, 3), "column clamped to row end");
        assert_eq!(b.text, "one\ntwo", "history never overwrites the draft");
        assert!(b.move_down());
        assert_eq!(b.cursor_row_col(), (1, 3));
        // single-line text stays history territory
        let mut c = InputBuffer::default();
        for ch in "single".chars() {
            c.insert(ch);
        }
        assert!(!c.move_up());
    }

    #[test]
    fn spin_frame_cycles_braille_set() {
        let frames = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏";
        assert_eq!(spin_frame(0), '⠋');
        assert_eq!(spin_frame(119), '⠋', "same frame within one tick");
        assert_eq!(spin_frame(120), '⠙');
        // wraps after the full set
        assert_eq!(spin_frame(120 * frames.chars().count() as u128), '⠋');
        for ms in (0..3000).step_by(40) {
            assert!(frames.contains(spin_frame(ms)));
        }
    }

    #[test]
    fn live_stale_gates_reparse() {
        let t0 = Instant::now();
        assert!(!live_stale(t0, t0), "fresh cache is reused");
        assert!(!live_stale(t0, t0 + Duration::from_millis(79)));
        assert!(live_stale(t0, t0 + Duration::from_millis(80)));
        assert!(live_stale(t0, t0 + Duration::from_secs(5)));
    }

    #[test]
    fn meters_footer_shows_fields() {
        let m = Meters {
            effort: String::new(),
            session: String::new(),
            model: "ollama/qwen3.5:9b".into(),
            mode: "guarded".into(),
            context: (10_000, 100_000),
            cost: 0.0,
            cache_hit: None,
        };
        let f = m.footer();
        assert!(
            f.contains("qwen3.5:9b") && f.contains("guarded") && f.contains("ctx 10%"),
            "{f}"
        );
    }

    #[test]
    fn user_band_spans_full_width() {
        use super::push_block;
        use ratatui::text::Line as TuiLine;

        fn row_width(line: &TuiLine) -> usize {
            line.spans.iter().map(|s| s.content.chars().count()).sum()
        }

        // user band: padded to exactly the width, wrapped rows included
        let mut out: Vec<TuiLine> = Vec::new();
        push_block(&mut out, "short text", 40);
        assert_eq!(row_width(&out[0]), 40, "band must span exactly the width");
        let mut out2: Vec<TuiLine> = Vec::new();
        push_block(&mut out2, &"word ".repeat(30), 40);
        for (i, line) in out2.iter().take(4).enumerate() {
            assert_eq!(
                row_width(line),
                40,
                "row {i} of wrapped band must span the width"
            );
        }
    }

    #[test]
    fn gutter_rows_wrap_without_padding() {
        use super::push_gutter;
        use ratatui::text::Line as TuiLine;

        fn row_width(line: &TuiLine) -> usize {
            line.spans.iter().map(|s| s.content.chars().count()).sum()
        }

        let mut out: Vec<TuiLine> = Vec::new();
        push_gutter(&mut out, "short text", 40, "⚙ ", crate::palette::META);
        assert_eq!(out.len(), 2, "one row + trailing blank");
        assert!(row_width(&out[0]) <= 40, "gutter rows never pad to width");

        // long text wraps at the width
        let mut out2: Vec<TuiLine> = Vec::new();
        push_gutter(
            &mut out2,
            &"x".repeat(100),
            40,
            "⋯ ",
            crate::palette::THOUGHT,
        );
        assert!(out2.len() > 2);
        for (i, line) in out2.iter().take(3).enumerate() {
            assert!(row_width(line) <= 40, "row {i} overflows the width");
        }

        // degenerate width still terminates
        let mut out3: Vec<TuiLine> = Vec::new();
        push_gutter(
            &mut out3,
            "abc",
            0,
            "! ",
            ratatui::style::Style::new().fg(crate::palette::ERR),
        );
        assert!(!out3.is_empty());
    }

    #[test]
    fn assistant_entry_fills_output_surface() {
        // assistant rows carry the output background and fill the width;
        // the trailing gap row carries the same fill: empty rows stay uniform
        let out = super::render_line(&Line::Assistant("**hi** there".into()), 40);
        let text: String = out[0].spans.iter().map(|s| s.content.to_string()).collect();
        assert_eq!(text.trim_end(), "hi there");
        let filled: usize = out[0].spans.iter().map(|s| s.content.chars().count()).sum();
        assert_eq!(filled, 40, "surface fills the width");
        let slab = format!("{:?}", out[0]);
        assert!(slab.contains("Rgb(22, 26, 31)"), "output bg: {slab}");
        assert!(out.len() >= 2);
        let gap = out.last().unwrap();
        assert!(
            gap.spans
                .iter()
                .all(|s| s.style.bg == Some(ratatui::style::Color::Rgb(22, 26, 31))),
            "gap row filled at span level: {gap:?}"
        );
        assert_eq!(
            gap.spans
                .iter()
                .map(|s| s.content.chars().count())
                .sum::<usize>(),
            40
        );
    }

    #[test]
    fn multiline_band_prefixes_only_first_row() {
        use super::push_block;
        use ratatui::text::Line as TuiLine;

        let mut out: Vec<TuiLine> = Vec::new();
        push_block(&mut out, "alpha\nbeta", 40);
        let text = |l: &TuiLine| {
            l.spans
                .iter()
                .map(|s| s.content.to_string())
                .collect::<String>()
        };
        assert!(text(&out[0]).starts_with("❯ alpha"));
        assert!(
            text(&out[1]).starts_with("  beta"),
            "second source line indents"
        );
        assert_eq!(out.len(), 3, "two rows + trailing blank");
    }

    #[test]
    fn multiline_gutter_prefixes_only_first_row() {
        use super::push_gutter;
        use ratatui::text::Line as TuiLine;

        let mut out: Vec<TuiLine> = Vec::new();
        push_gutter(&mut out, "one\ntwo", 40, "⚙ ", crate::palette::META);
        let text = |l: &TuiLine| {
            l.spans
                .iter()
                .map(|s| s.content.to_string())
                .collect::<String>()
        };
        assert!(text(&out[0]).starts_with("⚙ one"));
        assert!(
            text(&out[1]).starts_with("  two"),
            "second source line indents"
        );
    }
    #[test]
    fn custom_command_loads_and_substitutes() {
        let dir = std::env::temp_dir().join(format!("ka-cmd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cmds = dir.join(".ka/commands");
        std::fs::create_dir_all(&cmds).unwrap();
        std::fs::write(cmds.join("review.md"), "Review this diff: $ARGUMENTS").unwrap();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&dir).unwrap();
        let body = custom_command("/review", Some("src/main.rs")).unwrap();
        assert_eq!(body, "Review this diff: src/main.rs");
        std::env::set_current_dir(prev).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn slash_commands_parse() {
        assert!(slash_command("/quit").unwrap().quit);
        let m = slash_command("/model openai/gpt-5.1").unwrap();
        assert!(!m.quit);
        assert!(matches!(m.event, Some(Command::SetModel { .. })));
        let md = slash_command("/mode free").unwrap();
        assert!(matches!(
            md.event,
            Some(Command::SetMode {
                mode: ka_protocol::Mode::Free
            })
        ));
        assert!(slash_command("plain text").is_none());
        assert!(
            matches!(
                slash_command("/model"),
                Some(Slash {
                    note: None,
                    modal: Some(ModalKind::Model),
                    ..
                })
            ),
            "bare /model opens the picker"
        );
    }

    #[test]
    fn apply_event_collects_tool_and_text_lines() {
        let mut lines = Transcript::default();
        let mut busy = true;
        let mut busy_since = None;
        let mut meters = Meters::default();
        let mut pending = None;
        let mut cost = 0.0;
        let mut usage = None;
        let mut a = String::new();
        let mut t = String::new();
        let mut tool = String::new();

        apply_event(
            &Event::Delta {
                kind: ka_protocol::DeltaKind::Text("hi ".into()),
            },
            &mut lines,
            &mut busy,
            &mut busy_since,
            &mut meters,
            &mut pending,
            &mut cost,
            &mut usage,
            &mut a,
            &mut t,
            &mut tool,
        );
        apply_event(
            &Event::Delta {
                kind: ka_protocol::DeltaKind::Text("there".into()),
            },
            &mut lines,
            &mut busy,
            &mut busy_since,
            &mut meters,
            &mut pending,
            &mut cost,
            &mut usage,
            &mut a,
            &mut t,
            &mut tool,
        );
        apply_event(
            &Event::CallStarted {
                tool: "read".into(),
                id: "c1".into(),
            },
            &mut lines,
            &mut busy,
            &mut busy_since,
            &mut meters,
            &mut pending,
            &mut cost,
            &mut usage,
            &mut a,
            &mut t,
            &mut tool,
        );
        apply_event(
            &Event::CallOutput {
                tool: "read".into(),
                id: "c1".into(),
                excerpt: "1\tfile body".into(),
                is_error: false,
                spill: None,
            },
            &mut lines,
            &mut busy,
            &mut busy_since,
            &mut meters,
            &mut pending,
            &mut cost,
            &mut usage,
            &mut a,
            &mut t,
            &mut tool,
        );
        apply_event(
            &Event::TurnFinished {
                stop: ka_protocol::Stop::Done,
                usage: ka_protocol::Usage {
                    input: 10,
                    output: 2,
                    ..Default::default()
                },
            },
            &mut lines,
            &mut busy,
            &mut busy_since,
            &mut meters,
            &mut pending,
            &mut cost,
            &mut usage,
            &mut a,
            &mut t,
            &mut tool,
        );

        assert!(!busy);
        let texts: Vec<&str> = lines
            .entries()
            .iter()
            .filter_map(|l| match l {
                Line::Assistant(t) | Line::Tool(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert!(texts.contains(&"hi there"), "{texts:?}");
        assert!(
            texts.iter().any(|t| t.contains("read") && t.contains("✓")),
            "{texts:?}"
        );
    }
}
