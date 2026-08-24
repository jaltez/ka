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

/// Footer state shown under the editor.
#[derive(Debug, Clone, Default)]
pub struct Meters {
    /// Active model selector.
    pub model: String,
    /// Permission mode.
    pub mode: String,
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
        format!("{} · {} · {}{}{}", self.model, self.mode, ctx, cost, cache)
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

/// Run the TUI over an engine handle. Blocks until exit.
pub async fn run(
    mut commands: mpsc::Sender<Command>,
    mut events: mpsc::Receiver<Event>,
    initial_model: &str,
) -> std::io::Result<Exit> {
    let mut terminal = ratatui::init();
    let result = app(&mut terminal, &mut commands, &mut events, initial_model).await;
    ratatui::restore();
    result
}

async fn app(
    terminal: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    commands: &mut mpsc::Sender<Command>,
    events: &mut mpsc::Receiver<Event>,
    initial_model: &str,
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
    let mut term_events = crossterm::event::EventStream::new();

    while exit.is_none() {
        let footer = meters.footer();
        let busy_now = busy;
        let ask = pending.clone();
        let input_snapshot = input.text.clone();
        let cursor = input.cursor;
        terminal.draw(|frame| {
            render(
                frame,
                &lines,
                &input_snapshot,
                cursor,
                &footer,
                busy_now,
                ask.as_ref(),
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
                                if let Some(evt) = cmd.event {
                                    let _ = commands.send(evt).await;
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
                        (KeyCode::Up, _) if !busy => input.history_prev(),
                        (KeyCode::Down, _) if !busy => input.history_next(),
                        (KeyCode::Left, _) => input.left(),
                        (KeyCode::Right, _) => input.right(),
                        (KeyCode::Home, _) => input.home(),
                        (KeyCode::End, _) => input.end(),
                        (KeyCode::Backspace, _) => input.backspace(),
                        (KeyCode::Char(c), _) => input.insert(c),
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

/// A parsed slash command.
struct Slash {
    event: Option<Command>,
    quit: bool,
    followup: Option<String>,
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
    if !matches!(head, "/quit" | "/exit" | "/model" | "/mode" | "/compact") {
        if let Some(body) = custom_command(head, rest) {
            return Some(Slash {
                event: Some(Command::Prompt {
                    text: body,
                    attachments: vec![],
                }),
                quit: false,
                followup: None,
            });
        }
    }
    match head {
        "/quit" | "/exit" => Some(Slash {
            event: None,
            quit: true,
            followup: None,
        }),
        "/model" => {
            let selector = rest?;
            Some(Slash {
                event: Some(Command::SetModel {
                    selector: selector.to_string(),
                }),
                quit: false,
                followup: None,
            })
        }
        "/plan" => {
            let task = rest.map(str::to_string).unwrap_or_default();
            Some(Slash {
                event: Some(Command::SetMode {
                    mode: ka_protocol::Mode::Plan,
                }),
                quit: false,
                followup: Some(format!(
                    "Plan this task. Research the codebase with read/glob/grep/pathfinder, \\
then write a concrete numbered implementation plan to .ka/plans/plan.md. Task: {task}"
                )),
            })
        }
        "/build" => Some(Slash {
            event: Some(Command::SetMode {
                mode: ka_protocol::Mode::Guarded,
            }),
            quit: false,
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
            })
        }
        "/compact" => {
            let focus = rest.map(str::to_string);
            Some(Slash {
                event: Some(Command::Compact { focus }),
                quit: false,
                followup: None,
            })
        }
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
            })
        }
        _ => None,
    }
}

fn render(
    frame: &mut ratatui::Frame,
    lines: &[Line],
    input: &str,
    cursor: usize,
    footer: &str,
    busy: bool,
    ask: Option<&PendingAsk>,
) {
    use ratatui::layout::Constraint::{Length, Min};
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::{Line as TuiLine, Span};
    use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

    let chunks =
        ratatui::layout::Layout::vertical([Min(3), Length(3), Length(1)]).split(frame.area());

    // transcript
    let mut render_lines: Vec<TuiLine> = Vec::new();
    for line in lines {
        let (style, prefix) = match line {
            Line::User(_) => (Style::default().fg(Color::Blue), "you"),
            Line::Assistant(_) => (Style::default().fg(Color::White), "ka"),
            Line::Thought(_) => (Style::default().fg(Color::DarkGray), "…"),
            Line::Tool(_) => (Style::default().fg(Color::Yellow), "⚙"),
            Line::Note(_) => (Style::default().fg(Color::Red), "!"),
        };
        if let Line::Assistant(text) = line {
            for (i, seg) in text.split('\n').enumerate() {
                let prefix = if i == 0 {
                    format!("{prefix} │ ")
                } else {
                    "    │ ".to_string()
                };
                render_lines.push(TuiLine::styled(format!("{prefix}{seg}"), style));
            }
        } else {
            let text = match line {
                Line::User(t) | Line::Thought(t) | Line::Tool(t) | Line::Note(t) => t,
                Line::Assistant(_) => unreachable!(),
            };
            render_lines.push(TuiLine::styled(format!("{prefix} │ {text}"), style));
        }
    }
    // tail-scroll: show only what fits (approximate with wrapping)
    let visible = chunks[0].height.saturating_sub(1) as usize;
    if render_lines.len() > visible {
        let start = render_lines.len() - visible;
        render_lines.drain(..start);
    }
    let transcript = Paragraph::new(render_lines)
        .block(Block::default().borders(Borders::TOP).title("ka"))
        .wrap(Wrap { trim: false });
    frame.render_widget(transcript, chunks[0]);

    // input
    let title = if busy {
        "input (enter=interject, +=defer, esc=abort)"
    } else {
        "input"
    };
    let input_widget = Paragraph::new(input.to_string())
        .block(Block::default().borders(Borders::ALL).title(title))
        .wrap(Wrap { trim: false });
    frame.render_widget(input_widget, chunks[1]);
    // cursor position
    let area = chunks[1];
    let col = (cursor as u16).min(area.width.saturating_sub(2));
    frame.set_cursor_position((area.x + 1 + col, area.y + 1));

    // footer
    let status = if busy { " ⋆ working" } else { "" };
    let footer_line = TuiLine::from(vec![
        Span::styled(footer.to_string(), Style::default().fg(Color::Gray)),
        Span::styled(
            status.to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
    ]);
    frame.render_widget(Paragraph::new(footer_line), chunks[2]);

    // ask modal
    if let Some(ask) = ask {
        let rect = centered(40, 8, frame.area());
        let clear = ratatui::widgets::Clear;
        frame.render_widget(clear, rect);
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
                Style::default()
            };
            text.push(TuiLine::styled(format!("{marker}{opt}"), style));
        }
        text.push(TuiLine::styled(
            "↑↓ select · enter confirm · esc deny",
            Style::default().fg(Color::DarkGray),
        ));
        let popup = Paragraph::new(text)
            .block(Block::default().borders(Borders::ALL).title("permission"))
            .wrap(Wrap { trim: true });
        frame.render_widget(popup, rect);
    }
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
