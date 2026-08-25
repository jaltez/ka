//! The ka TUI: streaming transcript, input editor, footer meters, and ask
//! dialogs. Built on ratatui; talks to the engine exclusively through the
//! Command/Event queues.

use std::collections::VecDeque;

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

/// Footer state shown under the editor.
#[derive(Debug, Clone, Default)]
pub struct Meters {
    /// Active model selector.
    pub model: String,
    /// Permission mode.
    pub mode: String,
    /// Active session (strand) id.
    pub session: String,
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
        format!(
            "{} · {} · {}{}{}{}",
            self.model, self.mode, ctx, cost, cache, tag
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
            rows.push((
                format!("#{tag}  {}", s.title),
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

/// Which modal is open (drawn above everything).
#[derive(Debug, Clone)]
pub enum Modal {
    /// Session picker.
    Session(SessionPicker),
    /// Settings panel.
    Settings(SettingsPanel),
}

/// Run the TUI over an engine handle. Blocks until exit.
pub async fn run(
    mut commands: mpsc::Sender<Command>,
    mut events: mpsc::Receiver<Event>,
    initial_model: &str,
    providers: Vec<ProviderInfo>,
) -> std::io::Result<Exit> {
    let mut terminal = ratatui::init();
    let result = app(
        &mut terminal,
        &mut commands,
        &mut events,
        initial_model,
        providers,
    )
    .await;
    ratatui::restore();
    result
}

async fn app(
    terminal: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    commands: &mut mpsc::Sender<Command>,
    events: &mut mpsc::Receiver<Event>,
    initial_model: &str,
    providers: Vec<ProviderInfo>,
) -> std::io::Result<Exit> {
    use crossterm::event::{Event as TermEvent, KeyCode, KeyModifiers};

    let mut lines: Vec<Line> = Vec::new();
    let mut input = InputBuffer::default();
    let mut meters = Meters {
        model: initial_model.to_string(),
        mode: "guarded".to_string(),
        ..Default::default()
    };
    let mut busy = false;
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
    let mut term_events = crossterm::event::EventStream::new();

    while exit.is_none() {
        let footer = meters.footer();
        let busy_now = busy;
        let ask = pending.clone();
        let input_snapshot = input.text.clone();
        let cursor = input.cursor;
        let live = if busy_now {
            Some((current_thought.clone(), current_assistant.clone()))
        } else {
            None
        };
        terminal.draw(|frame| {
            render(
                frame,
                &lines,
                &input_snapshot,
                cursor,
                &footer,
                busy_now,
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
                                        let _ = commands
                                            .send(Command::SaveSettings {
                                                model: Some(panel.model.clone()),
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
                        (KeyCode::Enter, _) => {
                            let text = input.take();
                            if text.trim().is_empty() {
                                continue;
                            }
                            if let Some(cmd) = slash_command(&text) {
                                lines.push(Line::User(text));
                                if let Some(kind) = cmd.modal {
                                    modal = Some(match kind {
                                        ModalKind::Session => {
                                            let sessions = std::env::current_dir()
                                                .ok()
                                                .and_then(|cwd| ka_strand::list(&cwd).ok())
                                                .unwrap_or_default();
                                            Modal::Session(SessionPicker {
                                                sessions,
                                                selected: 0,
                                                filter: String::new(),
                                            })
                                        }
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
                                if let Some(evt) = cmd.event {
                                    let is_switch =
                                        matches!(evt, Command::SwitchStrand { .. });
                                    let _ = commands.send(evt).await;
                                    if is_switch {
                                        busy = true;
                                    }
                                }
                                if let Some(follow) = cmd.followup {
                                    lines.push(Line::Note("(mode set; starting)".into()));
                                    busy = true;
                                    let _ = commands
                                        .send(Command::Prompt { text: follow, attachments: vec![] })
                                        .await;
                                }
                                if cmd.quit {
                                    exit = Some(Exit::Quit);
                                }
                                continue;
                            }
                            lines.push(Line::User(text.clone()));
                            let cmd = if busy {
                                // '+'-prefix defers; Enter interjects
                                if let Some(deferred) = text.strip_prefix('+') {
                                    Command::Defer { text: deferred.trim_start().to_string() }
                                } else {
                                    Command::Interject { text }
                                }
                            } else {
                                Command::Prompt { text, attachments: vec![] }
                            };
                            busy = true;
                            let _ = commands.send(cmd).await;
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
                        (KeyCode::Up, _) if !busy => input.history_prev(),
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
                }
            }
            maybe_evt = events.recv() => {
                match maybe_evt {
                    None => { exit = Some(Exit::EngineEnded); }
                    Some(evt) => {
                        apply_event(
                            &evt,
                            &mut lines,
                            &mut busy,
                            &mut meters,
                            &mut pending,
                            &mut pending_turn_cost,
                            &mut turn_usage,
                            &mut current_assistant,
                            &mut current_thought,
                            &mut current_tool,
                        );
                                    }
                }
            }
        }
    }
    Ok(exit.unwrap_or(Exit::Quit))
}

#[allow(clippy::too_many_arguments)]
fn apply_event(
    evt: &Event,
    lines: &mut Vec<Line>,
    busy: &mut bool,
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
                    lines.push(Line::Tool(std::mem::take(current_tool)));
                }
                *current_tool = format!("→ {tool}");
            }
        },
        Event::CallStarted { tool, .. } => {
            if !current_tool.is_empty() {
                lines.push(Line::Tool(std::mem::take(current_tool)));
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
                lines.push(Line::Tool(std::mem::take(current_tool)));
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
                lines.push(Line::Thought(std::mem::take(current_thought)));
            }
            if !current_assistant.trim().is_empty() {
                lines.push(Line::Assistant(std::mem::take(current_assistant)));
            }
            if !current_tool.is_empty() {
                lines.push(Line::Tool(std::mem::take(current_tool)));
            }
            *busy = false;
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
        Event::SessionInfo { id } => meters.session = id.clone(),
        Event::ModeChanged { mode } => {
            meters.mode = match mode {
                ka_protocol::Mode::Guarded => "guarded".to_string(),
                ka_protocol::Mode::Free => "free".to_string(),
                ka_protocol::Mode::Plan => "plan".to_string(),
            };
        }
        Event::Error { message, .. } => {
            lines.push(Line::Note(format!("! {message}")));
        }
        Event::Replay { messages } => {
            // a replay is the full transcript of the active session:
            // rebuild from scratch (startup on a fresh Vec, session switch
            // replaces the previous conversation)
            lines.clear();
            meters.cost = 0.0;
            meters.context = (0, 0);
            current_assistant.clear();
            current_thought.clear();
            current_tool.clear();
            for m in messages {
                if m.role == "user" {
                    lines.push(Line::User(m.content.clone()));
                } else {
                    lines.push(Line::Assistant(m.content.clone()));
                }
            }
        }
        Event::Note { message } => lines.push(Line::Note(message.clone())),
        Event::Idle => {}
        Event::DigestStarted => lines.push(Line::Note("⋯ digesting context…".to_string())),
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
            "switch model: vendor/model@effort".to_string(),
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
            "/settings".to_string(),
            "settings & provider status".to_string(),
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
                    cmds.push((name, "(custom)".to_string()));
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
}

/// Modal a slash command opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModalKind {
    /// Session picker.
    Session,
    /// Settings panel.
    Settings,
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
    ) {
        if let Some(body) = custom_command(head, rest) {
            return Some(Slash {
                event: Some(Command::Prompt {
                    text: body,
                    attachments: vec![],
                }),
                quit: false,
                followup: None,
                modal: None,
            });
        }
    }
    match head {
        "/quit" | "/exit" => Some(Slash {
            event: None,
            quit: true,
            followup: None,
            modal: None,
        }),
        "/model" => {
            let selector = rest?;
            Some(Slash {
                event: Some(Command::SetModel {
                    selector: selector.to_string(),
                }),
                quit: false,
                followup: None,
                modal: None,
            })
        }
        "/plan" => {
            let task = rest.map(str::to_string).unwrap_or_default();
            Some(Slash {
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
                event: Some(Command::Rewind { turns }),
                quit: false,
                followup: None,
                modal: None,
            })
        }
        "/compact" => {
            let focus = rest.map(str::to_string);
            Some(Slash {
                event: Some(Command::Compact { focus }),
                quit: false,
                followup: None,
                modal: None,
            })
        }
        "/session" | "/resume" => Some(Slash {
            event: None,
            quit: false,
            followup: None,
            modal: Some(ModalKind::Session),
        }),
        "/new" => Some(Slash {
            event: Some(Command::SwitchStrand {
                id: "new".to_string(),
            }),
            quit: false,
            followup: None,
            modal: None,
        }),
        "/settings" => Some(Slash {
            event: None,
            quit: false,
            followup: None,
            modal: Some(ModalKind::Settings),
        }),
        "/mode" => {
            let mode = match rest? {
                "free" => ka_protocol::Mode::Free,
                "plan" => ka_protocol::Mode::Plan,
                _ => ka_protocol::Mode::Guarded,
            };
            Some(Slash {
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
    lines: &[Line],
    input: &str,
    cursor: usize,
    footer: &str,
    busy: bool,
    ask: Option<&PendingAsk>,
    live: Option<&(String, String)>,
    popup: Option<&SlashPopup>,
    modal: Option<&Modal>,
) {
    use ratatui::layout::Constraint::{Length, Min};
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::{Line as TuiLine, Span};
    use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

    let chunks =
        ratatui::layout::Layout::vertical([Min(3), Length(3), Length(1)]).split(frame.area());
    let width = chunks[0].width;

    // ── transcript ────────────────────────────────────────────────
    let mut render_lines: Vec<TuiLine> = Vec::new();
    for line in lines {
        match line {
            Line::User(text) => {
                push_block(&mut render_lines, text, width, BlockStyle::User);
            }
            Line::Assistant(text) => {
                render_lines.extend(crate::markdown::render(text));
                render_lines.push(TuiLine::default());
            }
            Line::Thought(text) => {
                push_block(&mut render_lines, text, width, BlockStyle::Thought);
            }
            Line::Tool(text) => {
                push_block(&mut render_lines, text, width, BlockStyle::Tool);
            }
            Line::Note(text) => {
                push_block(&mut render_lines, text, width, BlockStyle::Note);
            }
        }
    }

    // ── live streaming region (text + thinking while busy) ────────
    if let Some((thought, text)) = live {
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
                render_lines.push(TuiLine::styled(
                    format!("⋯ {l}"),
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::ITALIC),
                ));
            }
        }
        if !text.trim().is_empty() {
            render_lines.extend(crate::markdown::render(text));
        }
    }

    // tail-scroll: keep only what fits (approximate with wrapping)
    let visible = chunks[0].height.saturating_sub(1) as usize;
    if render_lines.len() > visible {
        let start = render_lines.len() - visible;
        render_lines.drain(..start);
    }
    let transcript = Paragraph::new(render_lines)
        .block(Block::default().borders(Borders::TOP).title("ka"))
        .wrap(Wrap { trim: false });
    frame.render_widget(transcript, chunks[0]);

    // ── input ─────────────────────────────────────────────────────
    let title = if busy {
        "input (enter=interject, +=defer, esc=abort)"
    } else {
        "input"
    };
    let input_widget = Paragraph::new(input.to_string())
        .block(Block::default().borders(Borders::ALL).title(title))
        .wrap(Wrap { trim: false });
    frame.render_widget(input_widget, chunks[1]);
    let area = chunks[1];
    let col = (cursor as u16).min(area.width.saturating_sub(2));
    frame.set_cursor_position((area.x + 1 + col, area.y + 1));

    // ── footer ────────────────────────────────────────────────────
    let status = if busy { " ⋆ working" } else { "" };
    let footer_line = TuiLine::from(vec![
        Span::styled(footer.to_string(), Style::default().fg(Color::Gray)),
        Span::styled(
            status.to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
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
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            };
            let desc_trim: String = desc.chars().take(32).collect();
            text.push(TuiLine::styled(
                format!("{marker}{name:<12} {desc_trim}"),
                style,
            ));
        }
        text.push(TuiLine::styled(
            "tab complete · ↑↓ select",
            Style::default().fg(Color::DarkGray),
        ));
        let widget = Paragraph::new(text)
            .block(Block::default().borders(Borders::ALL).title("commands"))
            .wrap(Wrap { trim: false });
        frame.render_widget(widget, rect);
    }

    // ── ask modal ─────────────────────────────────────────────────
    if let Some(ask) = ask {
        let rect = centered(40, 8, frame.area());
        frame.render_widget(Clear, rect);
        let mut text = vec![TuiLine::styled(
            ask.question.clone(),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )];
        for (i, opt) in ask.options.iter().enumerate() {
            let marker = if i == ask.selected { "▶ " } else { "  " };
            let style = if i == ask.selected {
                Style::default().fg(Color::Cyan)
            } else {
                Style::default().fg(Color::Gray)
            };
            text.push(TuiLine::styled(format!("{marker}{opt}"), style));
        }
        text.push(TuiLine::styled(
            "↑↓ select · enter confirm · esc deny",
            Style::default().fg(Color::DarkGray),
        ));
        let widget = Paragraph::new(text)
            .block(Block::default().borders(Borders::ALL).title("permission"))
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
                    Span::styled("filter: ", Style::default().fg(Color::Gray)),
                    Span::styled(picker.filter.clone(), Style::default().fg(Color::Cyan)),
                ])];
                for (i, (label, detail)) in rows.iter().take((height as usize) - 4).enumerate() {
                    let marker = if i == picker.selected { "▶ " } else { "  " };
                    let style = if i == picker.selected {
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::Gray)
                    };
                    text.push(TuiLine::styled(
                        format!("{marker}{label}  —  {detail}"),
                        style,
                    ));
                }
                text.push(TuiLine::styled(
                    "type to filter · enter switch · esc close",
                    Style::default().fg(Color::DarkGray),
                ));
                let widget = Paragraph::new(text)
                    .block(Block::default().borders(Borders::ALL).title("sessions"))
                    .wrap(Wrap { trim: false });
                frame.render_widget(widget, rect);
            }
            Modal::Settings(panel) => {
                let height = (SettingsPanel::ROWS + panel.providers.len() + 6) as u16;
                let height = height.min(frame.area().height.saturating_sub(2));
                let width = 72.min(frame.area().width);
                let rect = centered(width, height, frame.area());
                frame.render_widget(Clear, rect);
                let sel = Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD);
                let dim = Style::default().fg(Color::Gray);
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
                    Style::default()
                        .fg(Color::Yellow)
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
                        Style::default().fg(Color::DarkGray),
                    ),
                ];
                // hint sits above the providers so it survives clipping
                // on short terminals
                let hint = if editing {
                    "typing edits the selector · enter apply · esc cancel"
                } else {
                    "enter edit/cycle · s save to config · esc close"
                };
                text.push(TuiLine::styled(hint, Style::default().fg(Color::DarkGray)));
                text.push(TuiLine::styled(
                    "providers:",
                    Style::default().fg(Color::Gray),
                ));
                let url_room = (width.saturating_sub(38)) as usize;
                for p in &panel.providers {
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
                                Style::default().fg(Color::Green)
                            } else {
                                Style::default().fg(Color::Red)
                            },
                        ),
                        Span::styled(format!("  {url}"), Style::default().fg(Color::DarkGray)),
                    ]));
                }
                let widget = Paragraph::new(text)
                    .block(Block::default().borders(Borders::ALL).title("settings"))
                    .wrap(Wrap { trim: false });
                frame.render_widget(widget, rect);
            }
        }
    }
}

/// Background-block styles for transcript roles.
enum BlockStyle {
    /// Blue block, "you ❯" prefix.
    User,
    /// Dark gray block, "⋯" prefix (thinking).
    Thought,
    /// Amber block, "⚙" prefix (tools).
    Tool,
    /// Red-tinted block, "!" prefix (notes/errors).
    Note,
}

fn push_block(
    out: &mut Vec<ratatui::text::Line<'static>>,
    text: &str,
    width: u16,
    kind: BlockStyle,
) {
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::Line as TuiLine;

    let (prefix, style) = match kind {
        BlockStyle::User => (
            "❯ ",
            Style::default()
                .fg(Color::LightBlue)
                .bg(Color::Rgb(30, 54, 96)),
        ),
        BlockStyle::Thought => (
            "⋯ ",
            Style::default()
                .fg(Color::Gray)
                .bg(Color::Rgb(38, 38, 46))
                .add_modifier(Modifier::ITALIC),
        ),
        BlockStyle::Tool => (
            "⚙ ",
            Style::default()
                .fg(Color::LightYellow)
                .bg(Color::Rgb(72, 60, 24)),
        ),
        BlockStyle::Note => (
            "! ",
            Style::default().fg(Color::Red).bg(Color::Rgb(76, 28, 28)),
        ),
    };
    // blocks span the full transcript width, edge to edge
    let usable = width as usize;
    for raw in text.lines() {
        // wrap long lines at the block width (char boundary)
        let mut start = 0;
        let chars: Vec<char> = raw.chars().collect();
        loop {
            let first = start == 0;
            let lead = if first { prefix } else { "  " };
            let room = usable.saturating_sub(lead.chars().count());
            let end = (start + room).min(chars.len());
            let mut segment: String = chars[start..end].iter().collect();
            let pad = usable.saturating_sub(segment.chars().count() + lead.chars().count());
            segment.push_str(&" ".repeat(pad));
            out.push(TuiLine::styled(format!("{lead}{segment}"), style));
            if end >= chars.len() {
                break;
            }
            start = end;
        }
    }
    out.push(TuiLine::default()); // spacing after each block
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

    #[test]
    fn short_session_takes_tail() {
        assert_eq!(short_session("s19a4f2e1b0-3f9c2a81d4b7"), Some("3f9c2a81"));
        assert_eq!(short_session("no-tail"), Some("tail"));
        assert_eq!(short_session(""), None);
    }

    #[test]
    fn session_picker_filters_and_picks() {
        let picker = SessionPicker {
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
                event: Some(Command::SwitchStrand { id }),
                ..
            }) if id == "new"
        ));
        assert!(matches!(
            slash_command("/settings"),
            Some(Slash {
                modal: Some(ModalKind::Settings),
                ..
            })
        ));
        assert!(matches!(
            slash_command("/resume"),
            Some(Slash {
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
    fn meters_footer_shows_fields() {
        let m = Meters {
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
    fn blocks_span_full_width() {
        use super::{BlockStyle, push_block};
        use ratatui::text::Line as TuiLine;

        for kind in [
            BlockStyle::User,
            BlockStyle::Thought,
            BlockStyle::Tool,
            BlockStyle::Note,
        ] {
            let mut out: Vec<TuiLine> = Vec::new();
            push_block(&mut out, "short text", 40, kind_label(&kind));
            let content = out[0].clone();
            let width: usize = content
                .spans
                .iter()
                .map(|s| s.content.chars().count())
                .sum();
            assert_eq!(
                width,
                40,
                "{:?} block must span exactly the width",
                kind_dbg(&kind)
            );
            // long text wraps and every wrapped row also spans the width
            let mut out2: Vec<TuiLine> = Vec::new();
            push_block(&mut out2, &"word ".repeat(30), 40, kind_label(&kind));
            for (i, line) in out2.iter().take(4).enumerate() {
                let w: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();
                assert_eq!(w, 40, "row {i} of wrapped block must span the width");
            }
        }
        fn kind_label(k: &BlockStyle) -> BlockStyle {
            match k {
                BlockStyle::User => BlockStyle::User,
                BlockStyle::Thought => BlockStyle::Thought,
                BlockStyle::Tool => BlockStyle::Tool,
                BlockStyle::Note => BlockStyle::Note,
            }
        }
        fn kind_dbg(k: &BlockStyle) -> &'static str {
            match k {
                BlockStyle::User => "User",
                BlockStyle::Thought => "Thought",
                BlockStyle::Tool => "Tool",
                BlockStyle::Note => "Note",
            }
        }
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
        assert!(slash_command("/model").is_none(), "selector required");
    }

    #[test]
    fn apply_event_collects_tool_and_text_lines() {
        let mut lines = Vec::new();
        let mut busy = true;
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
