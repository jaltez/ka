//! The ka TUI: streaming transcript, input editor, footer meters, and ask
//! dialogs. Built on ratatui; talks to the engine exclusively through the
//! Command/Event queues.

use std::collections::VecDeque;
use std::io::Write;
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
    /// Undo snapshots (text + cursor), most recent last; capped at 100.
    undo: Vec<(String, usize)>,
    /// Readline-style kill ring (single entry: kills REPLACE, not append).
    kill: Option<String>,
    /// Active reverse incremental search (Ctrl+R); None = not searching.
    pub search: Option<SearchState>,
}

/// State of a Ctrl+R reverse incremental history search. The search scans
/// `history` newest-first; the pre-search draft is snapshotted so Esc can
/// restore it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SearchState {
    /// Current query text.
    pub query: String,
    /// History index of the current match (None = no match yet).
    pub match_idx: Option<usize>,
    /// `(text, cursor)` captured when the search started.
    draft: (String, usize),
}

impl InputBuffer {
    /// Insert a character at the cursor.
    pub fn insert(&mut self, c: char) {
        self.push_undo();
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
        self.push_undo();
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
        self.push_undo();
        let byte = self.char_to_byte(self.cursor);
        self.text.insert(byte, '\n');
        self.cursor += 1;
        self.browsing = None;
    }

    /// Insert a string at the cursor; CRLF/CR normalized to `\n`.
    pub fn insert_str(&mut self, s: &str) {
        let normalized = s.replace("\r\n", "\n").replace('\r', "\n");
        if normalized.is_empty() {
            return;
        }
        self.push_undo();
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

    /// Seed the draft history from a resumed session's prompts (oldest
    /// first). Consecutive duplicates collapse, blank prompts are
    /// skipped, and the 100-entry cap holds by dropping the oldest.
    pub fn seed_history(&mut self, items: impl IntoIterator<Item = String>) {
        for item in items {
            if item.trim().is_empty() || self.history.back() == Some(&item) {
                continue;
            }
            self.history.push_back(item);
        }
        while self.history.len() > 100 {
            self.history.pop_front();
        }
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

    /// Snapshot (text, cursor) BEFORE a mutation so `undo` restores the
    /// state preceding the op. Cap at 100 entries (drop the oldest).
    /// Clear the whole draft (Ctrl+C on a non-empty input). Routed
    /// through the undo stack and kill ring like every destructive
    /// edit, so Ctrl+Z restores and Ctrl+Y yanks it back.
    pub fn clear_draft(&mut self) {
        if self.text.is_empty() {
            return;
        }
        self.push_undo();
        let killed = std::mem::take(&mut self.text);
        self.cursor = 0;
        self.kill_push(Some(killed));
    }

    fn push_undo(&mut self) {
        if self.undo.len() >= 100 {
            self.undo.remove(0);
        }
        self.undo.push((self.text.clone(), self.cursor));
    }

    /// Delete the char AT the cursor (forward delete); no-op at the end.
    pub fn delete_forward(&mut self) {
        let len = self.text.chars().count();
        if self.cursor >= len {
            return;
        }
        self.push_undo();
        let start = self.char_to_byte(self.cursor);
        let end = self.char_to_byte(self.cursor + 1);
        self.text.replace_range(start..end, "");
        self.browsing = None;
    }

    /// Move the cursor to the start of the current line.
    pub fn line_home(&mut self) {
        let mut start = 0;
        for (i, c) in self.text.chars().enumerate() {
            if i >= self.cursor {
                break;
            }
            if c == '\n' {
                start = i + 1;
            }
        }
        self.cursor = start;
    }

    /// Move the cursor to the end of the current line.
    pub fn line_end(&mut self) {
        let mut end = self.text.chars().count();
        for (i, c) in self.text.chars().enumerate().skip(self.cursor) {
            if c == '\n' {
                end = i;
                break;
            }
        }
        self.cursor = end;
    }

    /// Move the cursor back one word (whitespace-delimited).
    pub fn word_left(&mut self) {
        let mut i = self.cursor;
        while i > 0 && word_ws(self.text.chars().nth(i - 1)) {
            i -= 1;
        }
        while i > 0 && !word_ws(self.text.chars().nth(i - 1)) {
            i -= 1;
        }
        self.cursor = i;
    }

    /// Move the cursor forward one word (whitespace-delimited).
    pub fn word_right(&mut self) {
        let len = self.text.chars().count();
        let mut i = self.cursor;
        while i < len && word_ws(self.text.chars().nth(i)) {
            i += 1;
        }
        while i < len && !word_ws(self.text.chars().nth(i)) {
            i += 1;
        }
        self.cursor = i;
    }

    /// Delete the word before the cursor; returns the deleted text.
    pub fn delete_word_backward(&mut self) -> Option<String> {
        let mut target = self.cursor;
        while target > 0 && word_ws(self.text.chars().nth(target - 1)) {
            target -= 1;
        }
        while target > 0 && !word_ws(self.text.chars().nth(target - 1)) {
            target -= 1;
        }
        if target == self.cursor {
            return None;
        }
        self.push_undo();
        let deleted: String = self
            .text
            .chars()
            .skip(target)
            .take(self.cursor - target)
            .collect();
        self.delete_char_range(target, self.cursor);
        self.cursor = target;
        Some(deleted)
    }

    /// Delete the word after the cursor; returns the deleted text.
    pub fn delete_word_forward(&mut self) -> Option<String> {
        let len = self.text.chars().count();
        let mut target = self.cursor;
        while target < len && word_ws(self.text.chars().nth(target)) {
            target += 1;
        }
        while target < len && !word_ws(self.text.chars().nth(target)) {
            target += 1;
        }
        if target == self.cursor {
            return None;
        }
        self.push_undo();
        let deleted: String = self
            .text
            .chars()
            .skip(self.cursor)
            .take(target - self.cursor)
            .collect();
        self.delete_char_range(self.cursor, target);
        Some(deleted)
    }

    /// Delete from the start of the current line to the cursor; returns
    /// the deleted text. No-op (None) when already at the line start.
    pub fn delete_to_line_start(&mut self) -> Option<String> {
        let mut start = 0;
        for (i, c) in self.text.chars().enumerate() {
            if i >= self.cursor {
                break;
            }
            if c == '\n' {
                start = i + 1;
            }
        }
        if start == self.cursor {
            return None;
        }
        self.push_undo();
        let deleted: String = self
            .text
            .chars()
            .skip(start)
            .take(self.cursor - start)
            .collect();
        self.delete_char_range(start, self.cursor);
        self.cursor = start;
        Some(deleted)
    }

    /// Delete from the cursor to the end of the current line (not
    /// including the `\n`); returns the deleted text. No-op (None) when
    /// already at the line end.
    pub fn delete_to_line_end(&mut self) -> Option<String> {
        let mut end = self.text.chars().count();
        for (i, c) in self.text.chars().enumerate().skip(self.cursor) {
            if c == '\n' {
                end = i;
                break;
            }
        }
        if end == self.cursor {
            return None;
        }
        self.push_undo();
        let deleted: String = self
            .text
            .chars()
            .skip(self.cursor)
            .take(end - self.cursor)
            .collect();
        self.delete_char_range(self.cursor, end);
        Some(deleted)
    }

    /// Undo the last text mutation, restoring the pre-op snapshot.
    pub fn undo(&mut self) {
        if let Some((text, cursor)) = self.undo.pop() {
            self.text = text;
            self.cursor = cursor;
            self.browsing = None;
        }
    }

    /// Store deleted text into the kill slot. Deliberate simplification
    /// of readline's consecutive-kill append: every kill REPLACES.
    pub fn kill_push(&mut self, deleted: Option<String>) {
        if let Some(d) = deleted {
            self.kill = Some(d);
        }
    }

    /// Insert the kill-slot text at the cursor (insert_str semantics).
    pub fn yank(&mut self) {
        if let Some(k) = self.kill.clone() {
            self.insert_str(&k);
        }
    }

    /// Delete the char range `[start..end)` in CHAR indices, clearing
    /// `browsing` (text changed).
    fn delete_char_range(&mut self, start: usize, end: usize) {
        let byte_s = self.char_to_byte(start);
        let byte_e = self.char_to_byte(end);
        self.text.replace_range(byte_s..byte_e, "");
        self.browsing = None;
    }

    fn char_to_byte(&self, char_idx: usize) -> usize {
        self.text
            .char_indices()
            .nth(char_idx)
            .map(|(b, _)| b)
            .unwrap_or(self.text.len())
    }

    /// Start a reverse incremental search over `history` (newest first),
    /// snapshotting the current draft so [`InputBuffer::search_cancel`]
    /// can restore it. No-op while already searching.
    pub fn search_start(&mut self) {
        if self.search.is_some() {
            return;
        }
        self.search = Some(SearchState {
            query: String::new(),
            match_idx: None,
            draft: (self.text.clone(), self.cursor),
        });
    }

    /// Whether a reverse search is active.
    pub fn searching(&self) -> bool {
        self.search.is_some()
    }

    /// Type a character into the search query and jump to the newest
    /// matching history entry. No-op when not searching; an empty match
    /// set leaves the buffer untouched.
    pub fn search_push(&mut self, c: char) {
        let query = match self.search.as_mut() {
            Some(s) => {
                s.query.push(c);
                s.query.clone()
            }
            None => return,
        };
        let idx = self.find_match(&query, self.history.len());
        if let Some(s) = self.search.as_mut() {
            s.match_idx = idx;
        }
        self.show_match();
    }

    /// Step to the next OLDER match for the current query (Ctrl+R again).
    /// Stays on the current match when the history is exhausted; no-op
    /// when not searching or the query is still empty.
    pub fn search_next(&mut self) {
        let Some(s) = self.search.clone() else {
            return;
        };
        if s.query.is_empty() {
            return;
        }
        let from = s.match_idx.unwrap_or(self.history.len());
        if let Some(idx) = self.find_match(&s.query, from) {
            if let Some(live) = self.search.as_mut() {
                live.match_idx = Some(idx);
            }
            self.show_match();
        }
    }

    /// Remove the last query character and re-match from the newest
    /// entry. No-op when not searching or the query is empty.
    pub fn search_backspace(&mut self) {
        let query = match self.search.as_mut() {
            Some(s) => {
                if s.query.pop().is_none() {
                    return;
                }
                s.query.clone()
            }
            None => return,
        };
        let idx = self.find_match(&query, self.history.len());
        if let Some(s) = self.search.as_mut() {
            s.match_idx = idx;
        }
        self.show_match();
    }

    /// Accept the current match: exits search mode, leaving the matched
    /// history entry loaded in the buffer. No-op when not searching.
    pub fn search_accept(&mut self) {
        self.search = None;
    }

    /// Cancel the search and restore the pre-search draft. No-op when
    /// not searching.
    pub fn search_cancel(&mut self) {
        if let Some(s) = self.search.take() {
            self.text = s.draft.0;
            self.cursor = s.draft.1;
        }
    }

    /// The live query line for the input box title, e.g.
    /// ``rsearch: `do` `` or ``rsearch: `do` (no match)``.
    pub fn search_title(&self) -> Option<String> {
        self.search.as_ref().map(|s| {
            let miss = if s.match_idx.is_none() && !s.query.is_empty() {
                " (no match)"
            } else {
                ""
            };
            format!("rsearch: `{}`{miss}", s.query)
        })
    }

    /// Newest-first history index strictly above `from` whose entry
    /// contains `query`. Empty queries never match.
    fn find_match(&self, query: &str, from: usize) -> Option<usize> {
        if query.is_empty() {
            return None;
        }
        (0..from.min(self.history.len()))
            .rev()
            .find(|&i| self.history[i].contains(query))
    }

    /// Load the current match into the buffer (cursor at end); a missed
    /// query leaves the buffer as-is.
    fn show_match(&mut self) {
        let idx = self.search.as_ref().and_then(|s| s.match_idx);
        if let Some(i) = idx {
            self.load_history(i);
        }
    }
}

/// Whitespace for word navigation: space, newline, tab.
fn word_ws(c: Option<char>) -> bool {
    matches!(c, Some(' ' | '\n' | '\t'))
}

/// Transcript line kinds for rendering.
#[derive(Debug, Clone, PartialEq)]
pub enum Line {
    /// User input.
    User(String),
    /// Assistant text.
    Assistant(String),
    /// Reasoning (rendered dim).
    Thought(String),
    /// One or more consecutive tool calls, each one compact railed row.
    ToolBlock(Vec<ToolCall>),
    /// System/status note (`· ` gutter).
    Info(String),
    /// Caution row (`⚠ ` gutter): refused actions, usage errors.
    Warn(String),
    /// Failure row (`! ` gutter, bold): the loudest system tier.
    Err(String),
    /// Turn report: empty = a blank separator row, else `─ {msg}`.
    Report(String),
    /// `!cmd` passthrough output: plain foreground, no gutter.
    Shell(String),
    /// One-row turn verdict (`✓ done · 2.1s · 1.2k in · $0.0042`).
    Summary {
        glyph: char,
        tone: SummaryTone,
        text: String,
    },
}

/// Tone of a [`Line::Summary`] verdict: colors glyph+text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SummaryTone {
    Ok,
    Warn,
    Err,
}

/// Click target of one rendered transcript row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowRef {
    /// The whole entry (thinking blocks toggle on click).
    Entry(usize),
    /// One call within a [`Line::ToolBlock`]: `(entry, call)`.
    ToolCall(usize, usize),
}

/// One finished tool call inside a [`Line::ToolBlock`] row.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    /// `→ {tool} · {detail}` header (the live band's header verbatim).
    pub head: String,
    /// Whether the call finished successfully.
    pub ok: bool,
    /// First non-blank output line, capped at 40 columns.
    pub note: String,
    /// Full output excerpt from CallOutput (already capped upstream).
    pub excerpt: String,
    /// Spill-file path from CallOutput, when the engine spilled.
    pub spill: Option<String>,
    /// Local elapsed CallStarted→CallFinished; `None` when unknown
    /// (replayed turns carry no durations).
    pub dur: Option<f64>,
    /// Whether the full excerpt is expanded inline (fold state).
    pub expanded: bool,
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
    /// Thinking blocks expanded? Collapsed entries render their first
    /// line plus a count marker; Alt+T toggles.
    thoughts_open: bool,
    /// Entry indices toggled against `thoughts_open` — per-block
    /// click-to-collapse for individual thinking blocks.
    thought_overrides: std::collections::HashSet<usize>,
    /// Render passes issued (cache-audit in tests).
    renders: usize,
}

impl Transcript {
    /// Append an entry; renders it at the current width.
    pub fn push(&mut self, line: Line) {
        let w = self.width;
        let idx = self.rendered.len();
        self.rendered
            .push(render_line(&line, w, self.thought_open(idx)));
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

    /// Expand/collapse thinking blocks, rebuilding the cache on change.
    /// Effective open state for one entry (global XOR per-block override).
    fn thought_open(&self, entry: usize) -> bool {
        self.thoughts_open ^ self.thought_overrides.contains(&entry)
    }

    /// Toggle one thinking block (click target). Only multi-line
    /// blocks participate — a one-liner has nothing to hide.
    pub fn toggle_thought(&mut self, entry: usize) {
        let multi = matches!(self.lines.get(entry), Some(Line::Thought(t)) if t.contains('\n'));
        if !multi {
            return;
        }
        if !self.thought_overrides.remove(&entry) {
            self.thought_overrides.insert(entry);
        }
        if let Some(line) = self.lines.get(entry).cloned() {
            let w = self.width;
            let open = self.thought_open(entry);
            if let Some(slot) = self.rendered.get_mut(entry) {
                *slot = render_line(&line, w, open);
            }
        }
    }

    /// What a rendered row belongs to (click mapping): a plain entry
    /// (thought-toggle target) or one specific call inside a tool
    /// block (expand target, `(entry, call)`).
    pub fn row_ref_at(&self, row: usize) -> Option<RowRef> {
        let mut acc = 0usize;
        for (i, (line, rows)) in self.lines.iter().zip(&self.rendered).enumerate() {
            let next = acc + rows.len();
            if row < next {
                return Some(match line {
                    Line::ToolBlock(calls) if (row - acc) < calls.len() => {
                        RowRef::ToolCall(i, row - acc)
                    }
                    _ => RowRef::Entry(i),
                });
            }
            acc = next;
        }
        None
    }

    /// One tool call by `(entry, call)` index (click-to-expand source).
    pub fn tool_call(&self, entry: usize, call: usize) -> Option<&ToolCall> {
        match self.lines.get(entry) {
            Some(Line::ToolBlock(calls)) => calls.get(call),
            _ => None,
        }
    }

    /// Toggle one call's inline expansion (click / Ctrl+O target) and
    /// re-render just that block. Indices that no longer point at a
    /// tool call (rewound transcript) no-op.
    pub fn toggle_tool_call(&mut self, entry: usize, call: usize) {
        let Some(Line::ToolBlock(calls)) = self.lines.get_mut(entry) else {
            return;
        };
        let Some(c) = calls.get_mut(call) else {
            return;
        };
        c.expanded = !c.expanded;
        let line = self.lines[entry].clone();
        if let Some(slot) = self.rendered.get_mut(entry) {
            *slot = render_line(&line, self.width, false);
            self.renders += 1;
        }
    }

    /// `(entry, call)` of the most recent call in the last block — the
    /// Ctrl+O target.
    pub fn last_tool_ref(&self) -> Option<(usize, usize)> {
        self.lines
            .iter()
            .enumerate()
            .rev()
            .find_map(|(i, l)| match l {
                Line::ToolBlock(v) if !v.is_empty() => Some((i, v.len() - 1)),
                _ => None,
            })
    }

    /// Append a finished tool call, merging into the trailing block
    /// when consecutive: a run of calls reads as one block of railed
    /// rows; any other push between calls ends the group.
    pub fn push_tool_call(&mut self, call: ToolCall) {
        if let Some(Line::ToolBlock(v)) = self.lines.last_mut() {
            v.push(call);
            let idx = self.lines.len() - 1;
            if let Some(slot) = self.rendered.get_mut(idx) {
                let line = self.lines[idx].clone();
                *slot = render_line(&line, self.width, false);
                self.renders += 1;
            }
        } else {
            self.push_separated(Line::ToolBlock(vec![call]));
        }
    }

    pub fn set_thoughts_open(&mut self, open: bool) {
        if open != self.thoughts_open {
            self.thoughts_open = open;
            self.thought_overrides.clear();
            self.rebuild();
        }
    }

    fn rebuild(&mut self) {
        let w = self.width;
        self.rendered = self
            .lines
            .iter()
            .enumerate()
            .map(|(i, l)| render_line(l, w, self.thought_open(i)))
            .collect();
        self.renders += self.lines.len();
    }

    /// Drop everything (session switch).
    pub fn clear(&mut self) {
        self.lines.clear();
        self.rendered.clear();
    }

    /// Drop every entry from the `nth`-from-last user message onward
    /// (the visual half of `rewind n`). Returns the dropped prompt when
    /// the cut happened, `None` when there are fewer user messages.
    pub fn rewind_user(&mut self, nth_from_end: usize) -> Option<String> {
        let mut seen = 0usize;
        let mut cut = self.lines.len();
        for (i, line) in self.lines.iter().enumerate().rev() {
            if matches!(line, Line::User(_)) {
                seen += 1;
                if seen == nth_from_end {
                    cut = i;
                    break;
                }
            }
        }
        if seen < nth_from_end {
            return None;
        }
        let dropped = match self.lines.get(cut) {
            Some(Line::User(t)) => Some(t.clone()),
            _ => None,
        };
        self.lines.truncate(cut);
        self.rendered.truncate(cut);
        dropped
    }

    /// The source entries, in order.
    pub fn entries(&self) -> &[Line] {
        &self.lines
    }

    /// Cached rendered row count.
    /// Rendered row count of one entry (tests).
    pub fn rendered_rows_of(&self, entry: usize) -> usize {
        self.rendered.get(entry).map(Vec::len).unwrap_or(0)
    }

    pub fn total_rows(&self) -> usize {
        self.rendered.iter().map(Vec::len).sum()
    }

    /// Rendered row index where each user entry starts, in transcript
    /// order (▲▼ title-arrow jump targets).
    pub fn user_entry_rows(&self) -> Vec<usize> {
        let mut rows = Vec::new();
        let mut at = 0usize;
        for (line, rendered) in self.lines.iter().zip(&self.rendered) {
            if matches!(line, Line::User(_)) {
                rows.push(at);
            }
            at += rendered.len();
        }
        rows
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

    /// Append an entry, preceded by [`separate_before`] air when the
    /// cached tail is a different-family block. Every cached block push
    /// goes through this, so the vertical rhythm has exactly one source.
    pub fn push_separated(&mut self, line: Line) {
        separate_before(self, family(&line));
        self.push(line);
    }
    /// Case-insensitive search over the source entries starting at
    /// absolute rendered row `from_row`: the first entry whose FIRST row
    /// sits at or below `from_row` and whose text contains `needle`.
    /// Returns `(entry_index, first_row_offset)`; None on a miss.
    pub fn find_from(&self, from_row: usize, needle: &str) -> Option<(usize, usize)> {
        let lower = needle.to_lowercase();
        if lower.is_empty() {
            return None;
        }
        let mut offset = 0usize;
        for (i, line) in self.lines.iter().enumerate() {
            let len = self.rendered.get(i).map(Vec::len).unwrap_or(0);
            if offset >= from_row && line_text(line).to_lowercase().contains(&lower) {
                return Some((i, offset));
            }
            offset += len;
        }
        None
    }
    /// Render passes issued so far (tests).
    pub fn render_passes(&self) -> usize {
        self.renders
    }
}

/// The searchable text of a transcript entry. Tool blocks expose their
/// heads and notes (excerpts stay out of transcript search).
fn line_text(line: &Line) -> std::borrow::Cow<'_, str> {
    match line {
        Line::User(t)
        | Line::Assistant(t)
        | Line::Thought(t)
        | Line::Info(t)
        | Line::Warn(t)
        | Line::Err(t)
        | Line::Report(t)
        | Line::Shell(t) => t.into(),
        Line::Summary { text, .. } => text.into(),
        Line::ToolBlock(calls) => {
            let mut s = String::new();
            for c in calls {
                s.push_str(&c.head);
                s.push('\n');
                s.push_str(&c.note);
                s.push('\n');
            }
            s.into()
        }
    }
}

/// Coarse visual family of an entry: vertical air (one blank canvas row)
/// separates consecutive blocks of different families, while a
/// same-family run stays tight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    User,
    Assistant,
    Thought,
    Tool,
    /// Notes, turn reports, error rows — and the blank separator itself.
    Meta,
}

/// The [`Family`] of a transcript entry (a separator blank reads as
/// Meta, but [`separate_before`] no-ops on it before consulting family).
fn family(line: &Line) -> Family {
    match line {
        Line::User(_) => Family::User,
        Line::Assistant(_) => Family::Assistant,
        Line::Thought(_) => Family::Thought,
        Line::ToolBlock(_) => Family::Tool,
        Line::Info(_)
        | Line::Warn(_)
        | Line::Err(_)
        | Line::Shell(_)
        | Line::Summary { .. }
        | Line::Report(_) => Family::Meta,
    }
}

/// Air between blocks: push one blank canvas row (`Report("")`) unless
/// the transcript is fresh, the tail already is a blank, or the tail
/// belongs to the incoming family — same-family runs stay tight, and air
/// is never doubled. Separators only ever sit BETWEEN blocks: the final
/// entry of a session leaves no trailing blank.
fn separate_before(transcript: &mut Transcript, incoming: Family) {
    let Some(last) = transcript.entries().last() else {
        return;
    };
    if line_text(last).is_empty() || family(last) == incoming {
        return;
    }
    transcript.push(Line::Report(String::new()));
}
/// Render one transcript entry into styled rows at `width`. Thinking
/// entries collapse to their first line plus a count marker unless
/// `thoughts_open`.
fn render_line(line: &Line, width: u16, thoughts_open: bool) -> Vec<ratatui::text::Line<'static>> {
    use ratatui::text::Line as TuiLine;
    let mut out: Vec<TuiLine> = Vec::new();
    match line {
        Line::User(text) => push_block(&mut out, text, width),
        Line::Assistant(text) => {
            // assistant output on a subtle full-width surface: background
            // fill only, never border glyphs; blank rows above and below
            // carry the same fill, giving the card inner top/bottom air
            let bg = crate::palette::BG_OUTPUT;
            out.push(surface_blank(width, bg));
            let inner = width.saturating_sub(2 * CARD_MARGIN as u16);
            let mut rows = crate::markdown::render(text, inner);
            apply_output_surface(&mut rows, inner, bg);
            inset_rows(&mut rows, width, bg);
            out.extend(rows);
            out.push(surface_blank(width, bg));
        }
        Line::Thought(text) => {
            if thoughts_open || !text.contains('\n') {
                let gutter = if text.contains('\n') {
                    "⋯ ▾ "
                } else {
                    "⋯ "
                };
                push_gutter(&mut out, text, width, gutter, crate::palette::THOUGHT);
            } else {
                // collapsed: first line plus a count marker, one row
                let lines = text.lines().count();
                let marker = format!(" … +{lines}");
                let head = trunc_cols(
                    text.lines().next().unwrap_or(""),
                    width.saturating_sub(marker.chars().count() as u16 + 6) as usize,
                );
                push_gutter(
                    &mut out,
                    &format!("{head}{marker}"),
                    width,
                    "⋯ ▸ ",
                    crate::palette::THOUGHT,
                );
            }
        }
        // finished tool calls cache as compact railed rows — one header
        // row per call, head left, duration + verdict right-aligned;
        // an expanded call reveals its full output underneath
        Line::ToolBlock(calls) => {
            for call in calls {
                out.extend(tool_call_rows(call, width));
            }
        }
        Line::Info(text) => push_gutter(&mut out, text, width, "· ", crate::palette::META.into()),
        Line::Warn(text) => push_gutter(&mut out, text, width, "⚠ ", crate::palette::WARN.into()),
        Line::Err(text) => push_gutter(
            &mut out,
            text,
            width,
            "! ",
            ratatui::style::Style::new()
                .fg(crate::palette::ERR)
                .add_modifier(ratatui::style::Modifier::BOLD),
        ),
        // `!cmd` passthrough output: plain prose color, no gutter
        Line::Shell(text) => out.push(TuiLine::from(vec![ratatui::text::Span::styled(
            text.clone(),
            ratatui::style::Style::new().fg(crate::palette::FG),
        )])),
        // the turn verdict: tone-colored glyph, muted text, no `─ ` prefix
        Line::Summary { glyph, tone, text } => {
            let tone_style = match tone {
                SummaryTone::Ok => ratatui::style::Style::new().fg(crate::palette::OK),
                SummaryTone::Warn => ratatui::style::Style::new().fg(crate::palette::WARN),
                SummaryTone::Err => ratatui::style::Style::new()
                    .fg(crate::palette::ERR)
                    .add_modifier(ratatui::style::Modifier::BOLD),
            };
            out.push(TuiLine::from(vec![
                ratatui::text::Span::styled(format!("{glyph} "), tone_style),
                ratatui::text::Span::styled(
                    text.clone(),
                    ratatui::style::Style::new().fg(crate::palette::META),
                ),
            ]));
        }
        // the vertical separator is one true canvas-blank row (gutter
        // text is empty, so the prefix loop would emit nothing)
        Line::Report(text) if text.is_empty() => out.push(TuiLine::default()),
        Line::Report(text) => push_gutter(&mut out, text, width, "─ ", crate::palette::META.into()),
    }
    out
}

/// Cached ToolBlock rows for one call: the header (` │ ▸ → tool · detail
/// note ……… 1.2s ✓`) and, when expanded, the call's full output as faint
/// railed rows underneath — plus a spill pointer when the engine parked
/// the full output on disk. Rows clip at the pane edge, never wrap.
fn tool_call_rows(call: &ToolCall, width: u16) -> Vec<ratatui::text::Line<'static>> {
    let mut rows = vec![tool_call_header(call, width)];
    let expandable = !call.excerpt.is_empty() || call.spill.is_some();
    if !call.expanded || !expandable {
        return rows;
    }
    use ratatui::text::Span;
    let w = width as usize;
    // content rows: ` │   line` — rail, two columns of air, faint text;
    // long lines hard-continue with a wider prefix so nothing is lost
    for line in call.excerpt.lines() {
        let mut rest = line;
        let mut first = true;
        loop {
            let (air, room) = if first {
                ("  ", w.saturating_sub(5))
            } else {
                ("    ", w.saturating_sub(6))
            };
            let take = rest.chars().count().min(room.max(1));
            let segment: String = rest.chars().take(take).collect();
            rows.push(ratatui::text::Line::from(vec![
                Span::styled(" │ ", crate::palette::BORDER_QUIET_STYLE),
                Span::raw(air),
                Span::styled(
                    segment,
                    ratatui::style::Style::new().fg(crate::palette::FAINT),
                ),
            ]));
            if take >= rest.chars().count() {
                break;
            }
            rest = &rest[rest
                .char_indices()
                .take(take)
                .map(|(i, c)| i + c.len_utf8())
                .last()
                .unwrap_or(0)..];
            first = false;
        }
    }
    if call.spill.is_some() {
        rows.push(ratatui::text::Line::from(vec![
            Span::styled(" │ ", crate::palette::BORDER_QUIET_STYLE),
            Span::raw("  "),
            Span::styled(
                trunc_cols(
                    "… full output parked in a spill file — /spills",
                    w.saturating_sub(5),
                ),
                ratatui::style::Style::new().fg(crate::palette::META),
            ),
        ]));
    }
    rows
}

/// The one header row of a tool call: ` │ [▸|▾] → tool · detail note`
/// left, duration + verdict glyph right-aligned to the pane edge. The
/// fold marker appears only when there is something to expand.
fn tool_call_header(call: &ToolCall, width: u16) -> ratatui::text::Line<'static> {
    use ratatui::text::Span;
    use unicode_width::UnicodeWidthStr;
    let w = width as usize;
    // right cluster: `1.2s ✓` — the duration only earns cells once it
    // measured at least a tenth of a second
    let dur = call.dur.filter(|d| *d >= 0.1).map(|d| format!("{d:.1}s "));
    let cluster = dur.as_deref().map_or(0, |s| s.width()) + 1;
    let avail = w.saturating_sub(cluster).max(1);
    let expandable = !call.excerpt.is_empty() || call.spill.is_some();
    let marker = if expandable { 2 } else { 0 };
    // the rail `" │ "` is three columns
    let room = avail.saturating_sub(3 + marker);
    let head = trunc_cols(&call.head, room);
    let note = if call.note.is_empty() {
        String::new()
    } else {
        let note_room = room.saturating_sub(head.width() + 1);
        if note_room == 0 {
            String::new()
        } else {
            format!(" {}", trunc_cols(&call.note, note_room))
        }
    };
    let used = 3 + marker + head.width() + note.width();
    let mut spans = vec![Span::styled(" │ ", crate::palette::BORDER_QUIET_STYLE)];
    if expandable {
        spans.push(Span::styled(
            if call.expanded { "▾ " } else { "▸ " },
            ratatui::style::Style::new().fg(crate::palette::FAINT),
        ));
    }
    spans.push(Span::styled(
        head,
        ratatui::style::Style::new().fg(crate::palette::TOOL),
    ));
    if !note.is_empty() {
        spans.push(Span::styled(
            note,
            ratatui::style::Style::new().fg(crate::palette::FAINT),
        ));
    }
    if avail > used {
        spans.push(Span::raw(" ".repeat(avail - used)));
    }
    if let Some(d) = dur {
        spans.push(Span::styled(
            d,
            ratatui::style::Style::new().fg(crate::palette::FAINT),
        ));
    }
    let verdict = if call.ok { "✓" } else { "✗" };
    spans.push(Span::styled(
        verdict,
        ratatui::style::Style::new().fg(if call.ok {
            crate::palette::OK
        } else {
            crate::palette::ERR
        }),
    ));
    ratatui::text::Line::from(spans)
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

/// Scrollbar rail geometry for a track of `track_h` cells: returns
/// `(thumb_pos, thumb_len)` for a viewport showing `visible` of `total`
/// rows with the window anchored at `start`. The thumb keeps at least
/// one cell and never runs past the track end; `(0, 0)` means no rail.
fn rail_thumb(track_h: usize, total: usize, visible: usize, start: usize) -> (usize, usize) {
    if track_h == 0 || total <= visible {
        return (0, 0);
    }
    let len = (visible * track_h / total).max(1);
    let pos = (start * track_h / total).min(track_h - len);
    (pos, len)
}

/// Visible transcript rows for a terminal height: the top margin, input
/// area, footer, and transcript top border are the four rows carved out
/// of the viewport.
fn visible_rows(term_h: u16, input_h: u16) -> usize {
    // top margin + transcript top border + bottom strip + status bar
    term_h.saturating_sub(input_h + 4) as usize
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

/// Rows the input area needs: the /mode picker borrows the box for its
/// four tier rows; otherwise the draft wraps to the box width.
/// (Permission asks render as a centered modal and no longer borrow it.)
fn input_area_rows(picker: Option<&ModePicker>, draft: &str, width: usize) -> usize {
    if picker.is_some() {
        MODE_CHOICES.len()
    } else {
        wrap_rows(draft, width).len()
    }
}

/// One wrapped display row of the draft: the row's text plus the char
/// index in the draft where the row starts (cursor mapping).
#[derive(Debug, Clone, PartialEq, Eq)]
struct VisualRow {
    text: String,
    start: usize,
}

/// Wrap a draft into display rows at `width` display columns
/// (unicode-width aware): explicit newlines always break, long lines
/// break at the last space that fits, over-long tokens hard-break.
/// The draft itself is never mutated — this is a display fold.
fn wrap_rows(text: &str, width: usize) -> Vec<VisualRow> {
    let width = width.max(1);
    let mut out: Vec<VisualRow> = Vec::new();
    let mut base = 0usize; // char index where the current line starts
    for line in text.split('\n') {
        // row = (char offset within the line, char)
        let mut row: Vec<(usize, char)> = Vec::new();
        let mut cols = 0usize;
        for (i, ch) in line.chars().enumerate() {
            let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if cols + w > width && !row.is_empty() {
                // prefer breaking at the last space within the row
                // (never at position 0 — that would loop forever)
                match row.iter().rposition(|(_, c)| *c == ' ').filter(|&p| p > 0) {
                    Some(p) => {
                        let carry: Vec<(usize, char)> = row[p + 1..].to_vec();
                        row.truncate(p);
                        let start = base + row.first().map(|(i, _)| *i).unwrap_or(0);
                        out.push(VisualRow {
                            text: row.iter().map(|(_, c)| c).collect(),
                            start,
                        });
                        cols = carry
                            .iter()
                            .map(|(_, c)| unicode_width::UnicodeWidthChar::width(*c).unwrap_or(0))
                            .sum();
                        row = carry;
                    }
                    None => {
                        let start = base + row.first().map(|(i, _)| *i).unwrap_or(0);
                        out.push(VisualRow {
                            text: row.iter().map(|(_, c)| c).collect(),
                            start,
                        });
                        row.clear();
                        cols = 0;
                    }
                }
            }
            row.push((i, ch));
            cols += w;
        }
        let start = base + row.first().map(|(i, _)| *i).unwrap_or(0);
        out.push(VisualRow {
            text: row.iter().map(|(_, c)| c).collect(),
            start,
        });
        base += line.chars().count() + 1; // +1 for the newline
    }
    out
}

/// The visual row containing char position `cursor`: the last row
/// whose start is at or before it.
fn visual_cursor_row(rows: &[VisualRow], cursor: usize) -> usize {
    rows.iter().rposition(|r| r.start <= cursor).unwrap_or(0)
}

/// Inner text width of the input box at terminal width `term_w`:
/// 2 outer margin cols + 2 borders + 2 horizontal padding.
fn input_inner_w(term_w: u16) -> usize {
    term_w.saturating_sub(6) as usize
}

/// Max rows of diff detail shown in an ask form. The input box grows
/// at most five rows past its base (six content rows total), and the
/// options row must stay visible: the detail budget is whatever is
/// left after the question lines and the options row, never above 8.
const ASK_DETAIL_MAX: usize = 8;

/// Rows of detail the ask form can afford for `question_lines` lines.
/// `ask_detail_rows` may add a trailer, so one row of headroom is kept.
fn ask_detail_budget(question_lines: usize) -> usize {
    ASK_DETAIL_MAX
        .min(6usize.saturating_sub(question_lines + 1))
        .saturating_sub(1)
}

/// Body rows of the permission-ask modal: the question in the strong
/// foreground, the budgeted diff detail, then one numbered option per
/// row — the selected row wears the pink selection bar.
fn ask_modal_body(ask: &PendingAsk, width: usize) -> Vec<ratatui::text::Line<'static>> {
    use ratatui::text::Line as TuiLine;
    use ratatui::text::Span;
    let mut rows: Vec<ratatui::text::Line<'static>> = Vec::new();
    push_gutter(
        &mut rows,
        &ask.question,
        width as u16,
        "",
        ratatui::style::Style::new()
            .fg(crate::palette::FG_STRONG)
            .add_modifier(ratatui::style::Modifier::BOLD),
    );
    let q_lines = ask.question.split('\n').count();
    let budget = ask_detail_budget(q_lines);
    if budget > 0 {
        if let Some(detail) = &ask.detail {
            rows.extend(ask_detail_rows(detail, budget));
        }
    }
    for (i, opt) in ask.options.iter().enumerate() {
        let row = format!("{} {opt}", i + 1);
        if i == ask.selected {
            rows.push(TuiLine::from(Span::styled(
                pad_to_width(row, width),
                selection_style(),
            )));
        } else {
            rows.push(TuiLine::raw(row));
        }
    }
    rows
}

/// Colorized rows for an ask's diff detail: additions in OK, removals
/// in ERR, hunk headers in META, file headers in FAINT, context plain.
/// Clamped to `max` rows with a `… +N more` trailer.
fn ask_detail_rows(detail: &str, max: usize) -> Vec<ratatui::text::Line<'static>> {
    use ratatui::style::Style;
    use ratatui::text::Span;
    let mut rows: Vec<ratatui::text::Line<'static>> = detail
        .lines()
        .map(|l| {
            let style = if l.starts_with("+++") || l.starts_with("---") {
                Style::new().fg(crate::palette::FAINT)
            } else if l.starts_with("@@") {
                Style::new().fg(crate::palette::META)
            } else if l.starts_with('+') {
                Style::new().fg(crate::palette::OK)
            } else if l.starts_with('-') {
                Style::new().fg(crate::palette::ERR)
            } else {
                Style::new()
            };
            ratatui::text::Line::from(vec![ratatui::text::Span::styled(l.to_string(), style)])
        })
        .collect();
    if rows.len() > max {
        let more = rows.len() - max;
        rows.truncate(max);
        rows.push(ratatui::text::Line::from(vec![Span::styled(
            format!("… +{more} more"),
            Style::new().fg(crate::palette::FAINT),
        )]));
    }
    rows
}

/// The input title while busy: only the queue hint — the action hints
/// live in the status bar now.
fn busy_input_title(queued: usize) -> String {
    if queued == 0 {
        "input".to_string()
    } else {
        format!("input · {queued} queued")
    }
}

/// Take the queue head (FIFO) to auto-send on turn settle.
fn pop_queue_head(queue: &mut Vec<String>) -> Option<String> {
    (!queue.is_empty()).then(|| queue.remove(0))
}

/// Braille spinner frame for an elapsed-milliseconds clock.
fn spin_frame(elapsed_ms: u128) -> char {
    const F: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    F[(elapsed_ms / 120) as usize % F.len()]
}

/// The transient live working-indicator row: spinner glyph + elapsed
/// seconds while a turn streams, a static WARN line while a permission
/// ask holds the turn. Never cached — rebuilt every frame.
fn working_row(
    ask: Option<&PendingAsk>,
    busy_since: Option<Instant>,
    now: Instant,
) -> ratatui::text::Line<'static> {
    use ratatui::text::Span;
    let ms = busy_since.map_or(0, |t0| now.duration_since(t0).as_millis());
    // span-level styling: Paragraph paints span styles, not line styles
    let (text, style) = if ask.is_some() {
        (
            format!("… waiting for approval · {:.1}s", ms as f64 / 1000.0),
            crate::palette::WARN,
        )
    } else {
        (
            format!("{} working · {:.1}s", spin_frame(ms), ms as f64 / 1000.0),
            crate::palette::META,
        )
    };
    ratatui::text::Line::from(vec![Span::styled(text, style)])
}

fn live_stale(cached_at: Instant, now: Instant) -> bool {
    now.duration_since(cached_at).as_millis() >= 80
}

/// Format a duration for turn reports: sub-minute `3.2s`, else `m:ss`.
fn fmt_dur(secs: f64) -> String {
    if secs < 60.0 {
        format!("{secs:.1}s")
    } else {
        let m = (secs / 60.0).floor() as u64;
        let ss = (secs % 60.0).floor() as u64;
        format!("{m}:{ss:02}")
    }
}

/// Format a token count: 999, 1k, 1.2k, 123k.
fn fmt_tok(n: u64) -> String {
    if n < 1000 {
        format!("{n}")
    } else {
        let v = format!("{:.1}", n as f64 / 1000.0);
        let v = v.strip_suffix(".0").unwrap_or(&v);
        format!("{v}k")
    }
}

/// Trailing turn-report fields: elapsed, tokens, cache, cost — each only
/// when non-zero.
fn usage_tail(u: &ka_protocol::Usage, dur: f64) -> String {
    let mut tail = format!(" · {}", fmt_dur(dur));
    if u.input + u.output > 0 {
        tail.push_str(&format!(
            " · {} in · {} out",
            fmt_tok(u.input),
            fmt_tok(u.output)
        ));
    }
    if u.cache_read > 0 {
        tail.push_str(&format!(" · {} cache", fmt_tok(u.cache_read)));
    }
    if u.cost > 0.0 {
        tail.push_str(&format!(" · ${:.4}", u.cost));
    }
    tail
}

/// Base64-encode a string's UTF-8 bytes (standard alphabet, `=` padding).
/// Local helper — the workspace has no base64 dependency.
fn b64encode(data: impl AsRef<[u8]>) -> String {
    const ALPHA: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = data.as_ref();
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHA[((n >> 18) & 63) as usize] as char);
        out.push(ALPHA[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHA[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHA[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
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

/// Scroll a few lines up from the current anchor (pinned counts from the
/// tail). Wheel granularity — three lines per notch.
fn line_up(scroll: &mut Option<usize>, total: usize, visible: usize) {
    if visible == 0 || total <= visible {
        *scroll = None;
        return;
    }
    let max_anchor = total - visible;
    let cur = scroll.unwrap_or(max_anchor);
    *scroll = Some(cur.saturating_sub(WHEEL_STEP).min(max_anchor));
}

/// Scroll a few lines down; reaching the tail re-pins (None).
fn line_down(scroll: &mut Option<usize>, total: usize, visible: usize) {
    let Some(anchor) = *scroll else { return };
    if visible == 0 {
        *scroll = None;
        return;
    }
    let max_anchor = total.saturating_sub(visible);
    *scroll = if anchor + WHEEL_STEP >= max_anchor {
        None
    } else {
        Some(anchor + WHEEL_STEP)
    };
}

/// Transcript rows per mouse-wheel notch.
const WHEEL_STEP: usize = 3;

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
    /// Finished turns this session.
    pub turns: u64,
    /// Cumulative input tokens seen (incl. cache reads/writes).
    pub tokens_in: u64,
    /// Cumulative output tokens.
    pub tokens_out: u64,
    /// Cumulative cache-read tokens.
    pub cache_read: u64,
    /// Accumulated busy seconds across finished turns.
    pub elapsed: f64,
}

/// Bootstrap inventory for the sidebar (the [`Event::Inventory`] payload,
/// kept beside the transcript card it also renders).
#[derive(Debug, Clone, Default)]
pub struct Inventory {
    /// Built-in + MCP tool names.
    pub tools: Vec<String>,
    /// Per configured MCP server: name, connect ok, tool count.
    pub mcp: Vec<ka_protocol::McpSummary>,
    /// Discovered subagent names.
    pub agents: Vec<String>,
    /// Discovered skill names.
    pub skills: Vec<String>,
    /// Advertised MCP prompts (`server/name (args)`).
    pub prompts: Vec<String>,
}

/// Data behind the bottom-strip popups. Session figures come from the
/// footer meters at render time; this state carries the rest.
#[derive(Debug, Clone, Default)]
pub struct SidebarState {
    /// Bootstrap inventory.
    pub inventory: Inventory,
    /// Live todo list ([`Event::Todos`]; whole-list replacement).
    pub todos: Vec<ka_protocol::TodoItem>,
    /// Working directory, display-shortened.
    pub cwd: String,
    /// Git branch when cheaply detectable at startup.
    pub branch: Option<String>,
    /// The active session's display title ([`Event::Title`]; stored
    /// record or auto-generated). None until the engine announces one.
    pub title: Option<String>,
}

/// Clickable bottom-strip buttons, recorded at render time so the
/// mouse handler can hit-test without duplicating the layout math
/// (capture mode only — native mode uses the keyboard shortcuts).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct StripZones {
    /// The todos button.
    todos: ratatui::layout::Rect,
    /// The skills button.
    skills: ratatui::layout::Rect,
    /// The info button.
    info: ratatui::layout::Rect,
}

/// Which strip button a click landed on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StripButton {
    Todos,
    Skills,
    Info,
}

impl StripZones {
    /// Does a terminal-cell click land on a button? Returns the button.
    fn hit(&self, x: u16, y: u16) -> Option<StripButton> {
        let pos = ratatui::layout::Position { x, y };
        if self.todos.contains(pos) {
            Some(StripButton::Todos)
        } else if self.skills.contains(pos) {
            Some(StripButton::Skills)
        } else if self.info.contains(pos) {
            Some(StripButton::Info)
        } else {
            None
        }
    }
}

/// Clickable ▲▼ jump targets on the transcript title row (recorded at
/// render time like [`SidebarZone`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TitleArrows {
    /// ▲ — jump to the previous user message.
    up: ratatui::layout::Rect,
    /// ▼ — jump to the next user message.
    down: ratatui::layout::Rect,
}

impl TitleArrows {
    /// Does a terminal-cell click land on one of the arrows? Returns
    /// `Some(true)` for ▲ (previous), `Some(false)` for ▼ (next).
    fn hit(&self, x: u16, y: u16) -> Option<bool> {
        let pos = ratatui::layout::Position { x, y };
        if self.up.contains(pos) {
            Some(true)
        } else if self.down.contains(pos) {
            Some(false)
        } else {
            None
        }
    }
}

/// Inner text width of the transcript column: the terminal narrows by
/// one margin column on each side and the paragraph's side padding.
/// Both the cache (`Transcript::set_width`) and the live surface derive
/// from this so they always agree.
fn transcript_width(term_w: u16) -> u16 {
    term_w.saturating_sub(4) // 2 margin cols + the paragraph's side padding
}

/// Truncate a string to `max_cols` display columns (unicode-width aware),
/// marking the cut with `…`.
fn trunc_cols(s: &str, max_cols: usize) -> String {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
    if s.width() <= max_cols {
        return s.to_string();
    }
    let keep = max_cols.saturating_sub(1);
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let w = UnicodeWidthChar::width(c).unwrap_or(0);
        if used + w > keep {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    out
}

/// Shorten a cwd for one sidebar row: the last two components.
fn shorten_cwd(cwd: &std::path::Path) -> String {
    let parts: Vec<_> = cwd
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    match parts.len() {
        0 => String::new(),
        1 => parts[0].clone(),
        n => format!("…/{}/{}", parts[n - 2], parts[n - 1]),
    }
}

/// One-shot git branch detection for the sidebar info section (the
/// engine's per-turn snapshot never crosses the protocol; this is the
/// cheap local equivalent — empty when not a repo). `symbolic-ref`
/// covers fresh repos with no commits yet, where `rev-parse HEAD`
/// fails.
fn detect_branch() -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["symbolic-ref", "--short", "HEAD"])
        .output()
        .ok()?;
    let branch = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !branch.is_empty()).then_some(branch)
}
/// Popup rows for the todos strip button: done items struck through,
/// the first pending item accented as "next".
fn todos_rows(sidebar: &SidebarState, width: usize) -> Vec<ratatui::text::Line<'static>> {
    use ratatui::style::{Modifier, Style};
    use ratatui::text::{Line as TuiLine, Span};
    if sidebar.todos.is_empty() {
        return vec![TuiLine::styled(
            "(no live todo list — the todo hand drives this)".to_string(),
            crate::palette::META,
        )];
    }
    let done_style = Style::new()
        .fg(crate::palette::FAINT)
        .add_modifier(Modifier::CROSSED_OUT);
    let first_pending = sidebar
        .todos
        .iter()
        .position(|t| t.state == ka_protocol::TodoState::Pending);
    sidebar
        .todos
        .iter()
        .enumerate()
        .map(|(i, t)| match t.state {
            ka_protocol::TodoState::Done => TuiLine::from(vec![
                Span::styled("\u{2713} ", done_style),
                Span::styled(trunc_cols(&t.text, width.saturating_sub(2)), done_style),
            ]),
            ka_protocol::TodoState::Pending if Some(i) == first_pending => TuiLine::from(vec![
                Span::styled("\u{b7} ".to_string(), crate::palette::ACCENT_BOLD),
                Span::styled(
                    trunc_cols(&t.text, width.saturating_sub(2)),
                    crate::palette::ACCENT_BOLD,
                ),
            ]),
            ka_protocol::TodoState::Pending => TuiLine::from(format!(
                "\u{b7} {}",
                trunc_cols(&t.text, width.saturating_sub(2))
            )),
        })
        .collect()
}

/// Popup rows for the skills strip button: the discovery inventory —
/// skills, agents, MCP servers — one labeled section each.
fn inventory_rows(sidebar: &SidebarState, width: usize) -> Vec<ratatui::text::Line<'static>> {
    use ratatui::text::{Line as TuiLine, Span};
    // the one section-header glyph: `▸ name`, accent bold
    let header = |name: &str| TuiLine::styled(format!("▸ {name}"), crate::palette::ACCENT_BOLD);
    let plain = |s: String| TuiLine::from(s);
    let names = |label: &str, items: &[String]| {
        std::iter::once(header(label))
            .chain(items.iter().map(|s| plain(trunc_cols(s, width))))
            .collect::<Vec<_>>()
    };
    let mut out = names("skills", &sidebar.inventory.skills);
    out.extend(names("agents", &sidebar.inventory.agents));
    out.push(header("mcp"));
    for m in &sidebar.inventory.mcp {
        let row = if m.ok {
            TuiLine::from(vec![
                Span::raw(format!("{} ", trunc_cols(&m.name, width.saturating_sub(4)))),
                Span::styled("\u{2713}".to_string(), crate::palette::OK),
                Span::raw(format!(" {}", m.tools)),
            ])
        } else {
            TuiLine::from(vec![
                Span::raw(format!("{} ", trunc_cols(&m.name, width.saturating_sub(4)))),
                Span::styled("\u{2717}".to_string(), crate::palette::ERR),
            ])
        };
        out.push(row);
    }
    if out.iter().all(|l| {
        l.spans.is_empty() || {
            let s: String = l.spans.iter().map(|s| s.content.clone()).collect();
            ["skills", "agents", "mcp"].contains(&s.as_str())
        }
    }) {
        out.insert(
            0,
            TuiLine::styled(
                "(nothing discovered — skills, agents, MCP servers land here)".to_string(),
                crate::palette::META,
            ),
        );
    }
    out
}

/// Popup rows for the info strip button: cwd:branch, session, model,
/// effort, cost, context — the old sidebar's session + info sections.
fn info_rows(
    sidebar: &SidebarState,
    meters: &Meters,
    width: usize,
) -> Vec<ratatui::text::Line<'static>> {
    let plain = |s: String| ratatui::text::Line::from(s);
    let mut rows = Vec::new();
    match &sidebar.branch {
        Some(b) => rows.push(plain(trunc_cols(&format!("{}:{b}", sidebar.cwd), width))),
        None if !sidebar.cwd.is_empty() => {
            rows.push(plain(trunc_cols(&sidebar.cwd, width)));
        }
        _ => {}
    }
    let (used, window) = meters.context;
    let ctx_row = if window > 0 {
        format!(
            "ctx {used}/{window} {}%",
            (used as f64 / window as f64 * 100.0) as u64
        )
    } else {
        format!("ctx {used}")
    };
    for row in [
        short_session(&meters.session).map(|t| format!("session #{t}")),
        (!meters.model.is_empty()).then(|| format!("model {}", meters.model)),
        (!meters.effort.is_empty()).then(|| format!("effort {}", meters.effort)),
        (meters.cost > 0.0).then(|| format!("cost ${:.4}", meters.cost)),
        (used > 0).then_some(ctx_row),
    ]
    .into_iter()
    .flatten()
    {
        rows.push(plain(trunc_cols(&row, width)));
    }
    if rows.is_empty() {
        rows.push(plain("(no session facts yet)".to_string()));
    }
    rows
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
    /// Optional rendered detail (e.g. a unified diff) above the options.
    pub detail: Option<String>,
}

/// Agent summaries injected by the CLI for `/agents` (ka-term stays
/// ka-engine-free).
pub static AGENTS: std::sync::OnceLock<Vec<(String, String)>> = std::sync::OnceLock::new();

/// Notification settings injected by the CLI ([tui] bell / notify):
/// the bell rings on turn completion and permission asks; the command
/// (when set) runs once per finished turn with a small JSON payload on
/// stdin — `notify-send ka "done"` is the canonical use.
pub static NOTIFY: std::sync::OnceLock<NotifySettings> = std::sync::OnceLock::new();

/// What the CLI resolves from `[tui]` config before `run`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NotifySettings {
    /// Ring the terminal bell on turn completion and asks.
    pub bell: bool,
    /// Optional shell command; JSON `{"event","stop"}` on stdin.
    pub command: Option<String>,
}

/// Fire turn-completion/ask notifications: bell byte first (terminals
/// mute it by user choice), then the optional command detached.
fn fire_notifications(event: &str, stop: &str) {
    let settings = NOTIFY.get().cloned().unwrap_or_default();
    if settings.bell {
        let _ = std::io::Write::write_all(&mut std::io::stdout(), b"\x07");
        let _ = std::io::Write::flush(&mut std::io::stdout());
    }
    let Some(command) = settings.command else {
        return;
    };
    // fixed-vocabulary payload: no user text, no escaping needed
    let payload = format!("{{\"event\":\"{event}\",\"stop\":\"{stop}\"}}");
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        let spawned = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&command)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn();
        if let Ok(mut child) = spawned {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(payload.as_bytes()).await;
            }
            let _ = child.wait().await;
        }
    });
}

/// Short human tag for a strand id: the first 8 chars of its random tail.
pub fn short_session(id: &str) -> Option<&str> {
    id.split_once('-')
        .map(|(_, tail)| &tail[..tail.len().min(8)])
}

/// Relative age of an RFC3339 timestamp against `now_secs` (epoch).
fn rel_age_at(ts: &str, now_secs: i64) -> String {
    let parts: Vec<&str> = ts.split(['-', 'T', ':', 'Z', '+', '.']).collect();
    let nums: Vec<i64> = parts
        .iter()
        .take(6)
        .map(|p| p.parse::<i64>().ok())
        .collect::<Option<Vec<i64>>>()
        .unwrap_or_default();
    if nums.len() < 6 {
        return ts.to_string();
    }
    let (y, mo, d, h, mi, s) = (nums[0], nums[1], nums[2], nums[3], nums[4], nums[5]);
    // days since epoch: Howard Hinnant's civil algorithm
    let yy = if mo <= 2 { y - 1 } else { y };
    let era = if yy >= 0 { yy } else { yy - 399 } / 400;
    let yoe = yy - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    let secs = days * 86400 + h * 3600 + mi * 60 + s;
    let diff = now_secs - secs;
    if diff < 60 {
        "just now".to_string()
    } else if diff < 3600 {
        format!("{}m ago", diff / 60)
    } else if diff < 86400 {
        format!("{}h ago", diff / 3600)
    } else if diff < 604800 {
        format!("{}d ago", diff / 86400)
    } else {
        format!("{}w ago", diff / 604800)
    }
}

/// Relative age against the wall clock.
pub fn rel_age(ts: &str) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    rel_age_at(ts, now)
}

/// Rows for the `/usage` popup: session figures, recent strands, total.
fn usage_rows(meters: &Meters, summaries: &[ka_strand::StrandSummary]) -> Vec<String> {
    let hit = if meters.tokens_in > 0 {
        format!(
            "{:.0}%",
            meters.cache_read as f32 / meters.tokens_in as f32 * 100.0
        )
    } else {
        "—".to_string()
    };
    let mut rows = vec![
        "▸ session".to_string(),
        format!(
            "{} turns · {} in / {} out · cache {} · hit {hit}",
            meters.turns,
            fmt_tok(meters.tokens_in),
            fmt_tok(meters.tokens_out),
            fmt_tok(meters.cache_read),
        ),
        format!("${:.4} · {} busy", meters.cost, fmt_dur(meters.elapsed)),
        String::new(),
        "▸ recent sessions".to_string(),
    ];
    for s in summaries.iter().take(8) {
        let title: String = s.title.chars().take(34).collect();
        rows.push(format!(
            "{title} · {} · {} tok · ${:.2}",
            rel_age(&s.ts),
            fmt_tok(s.tokens),
            s.cost,
        ));
    }
    if summaries.is_empty() {
        rows.push("(no recorded sessions)".to_string());
    }
    let tot_tok: u64 = summaries.iter().map(|s| s.tokens).sum();
    let tot_cost: f64 = summaries.iter().map(|s| s.cost).sum();
    rows.push(String::new());
    rows.push(format!(
        "▸ total · {} sessions · {} tok · ${tot_cost:.2}",
        summaries.len(),
        fmt_tok(tot_tok),
    ));
    rows
}

/// Rows for the `/context` popup: one bar per component scaled to the
/// window, plus a used/free footer.
fn context_rows(parts: &[ka_protocol::ContextPart], window: u64) -> Vec<String> {
    const BAR_W: usize = 20;
    let used: u64 = parts.iter().map(|p| p.tokens).sum();
    let mut rows = vec!["▸ context".to_string()];
    for p in parts {
        let filled = if window > 0 {
            ((p.tokens as f64 / window as f64) * BAR_W as f64).round() as usize
        } else {
            0
        }
        .min(BAR_W);
        let bar = format!("{}{}", "█".repeat(filled), "░".repeat(BAR_W - filled));
        let pct = (p.tokens * 100).checked_div(used).unwrap_or(0);
        rows.push(format!(
            "{:<10} {bar} {:>7} ({pct}%)",
            p.name,
            fmt_tok(p.tokens)
        ));
    }
    rows.push(String::new());
    let free = window.saturating_sub(used);
    let pct = (used * 100).checked_div(window).unwrap_or(0);
    rows.push(format!(
        "▸ used {} of {} ({pct}%) · free {}",
        fmt_tok(used),
        fmt_tok(window),
        fmt_tok(free)
    ));
    rows
}

/// The session cwd handed to [`run`]: the plan path must anchor where
/// the ENGINE anchors (its resolved cwd, honoring the `cwd` config
/// override), not wherever the process happens to stand.
pub static PLAN_CWD: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

/// The plan file path for this session: root-anchored (nearest `.git`
/// ancestor of the session cwd, else the session cwd), matching the
/// plan-mode system prompt and the engine's writable-path gate. Falls
/// back to the process cwd before [`run`] hands the session cwd over.
fn plan_file_path() -> std::path::PathBuf {
    let base = PLAN_CWD
        .get()
        .cloned()
        .or_else(|| std::env::current_dir().ok());
    match base {
        Some(cwd) => ka_engine::project_root(&cwd).join(".ka/plans/plan.md"),
        None => std::path::PathBuf::from(".ka/plans/plan.md"),
    }
}

/// The shared follow-up prompt that starts a build turn from the plan
/// file (both `/build` and `/approve`). The plan file anchors at the
/// root project (nearest `.git` ancestor, else cwd), matching the
/// plan-mode system prompt.
fn build_followup() -> String {
    format!(
        "Switching to build mode. Read {} and implement it step by \
step now; verify each step.",
        plan_file_path().display()
    )
}

/// True when the plan file exists and was written after `/plan` started
/// its turn: a fresh plan on disk → suggest `/approve`.
fn plan_drafted(plan_started: Option<std::time::SystemTime>, plan_path: &std::path::Path) -> bool {
    let Some(started) = plan_started else {
        return false;
    };
    plan_path
        .metadata()
        .ok()
        .and_then(|m| m.modified().ok())
        .is_some_and(|mt| mt >= started)
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
            let mut detail = format!("{} msgs · {}", s.messages, rel_age(&s.ts));
            // only pay for digits the session earned: sub-cent costs and
            // zero token counts add no separators
            if s.cost >= 0.005 {
                detail.push_str(&format!(" · ${:.2}", s.cost));
            }
            if s.tokens > 0 {
                detail.push_str(&format!(" · {} tok", fmt_tok(s.tokens)));
            }
            rows.push((format!("#{tag}  {}{here}", s.title), detail));
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

/// The permission tiers `/mode` offers, in picker order:
/// (mode, label, one-line description).
const MODE_CHOICES: [(ka_protocol::Mode, &str, &str); 4] = [
    (
        ka_protocol::Mode::Guarded,
        "needs approval",
        "ask before every edit and command",
    ),
    (
        ka_protocol::Mode::AcceptEdits,
        "accept edits",
        "auto-apply file edits, ask for commands",
    ),
    (ka_protocol::Mode::Free, "full access", "no prompts"),
    (
        ka_protocol::Mode::Plan,
        "plan",
        "read-only planning (build with /build)",
    ),
];

/// Footer label for a mode (`needs-approval`, `accept-edits`, …).
fn mode_label(mode: ka_protocol::Mode) -> &'static str {
    match mode {
        ka_protocol::Mode::Guarded => "needs-approval",
        ka_protocol::Mode::AcceptEdits => "accept-edits",
        ka_protocol::Mode::Free => "full-access",
        ka_protocol::Mode::Plan => "plan",
    }
}

/// Parse a footer label back to a mode (settings panel bootstrap).
fn mode_from_label(label: &str) -> ka_protocol::Mode {
    match label {
        "accept-edits" => ka_protocol::Mode::AcceptEdits,
        "full-access" => ka_protocol::Mode::Free,
        "plan" => ka_protocol::Mode::Plan,
        _ => ka_protocol::Mode::Guarded,
    }
}

/// The `/mode` picker: borrows the input box like the permission form
/// does; ↑↓/1-4 navigate, Enter confirms, Esc cancels.
#[derive(Debug, Clone)]
pub struct ModePicker {
    /// Selected row index.
    pub selected: usize,
}

impl ModePicker {
    /// A picker preselecting the given mode's row.
    pub fn for_mode(mode: ka_protocol::Mode) -> Self {
        Self {
            selected: MODE_CHOICES
                .iter()
                .position(|(m, _, _)| *m == mode)
                .unwrap_or(0),
        }
    }

    /// The mode the selected row stands for.
    pub fn pick(&self) -> ka_protocol::Mode {
        MODE_CHOICES[self.selected.min(MODE_CHOICES.len() - 1)].0
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
    /// Vendor lock (stage two of `/provider`): when set, only this
    /// vendor's models are listed and Esc returns to the provider stage.
    pub vendor: Option<String>,
    /// List only models of configured providers (keyless or key
    /// present). The vendor-locked post-connect drill sets this; the
    /// unified `/model` list leaves it off and partitions
    /// configured-first instead.
    pub configured_only: bool,
    /// Selected row.
    pub selected: usize,
    /// Typed filter (substring over the id).
    pub filter: String,
}

/// One picker row for a model. Must stay on a single line under the
/// modal's inner width (68 - 2 borders = 66) or Paragraph wraps and the
/// tail models clip off the picker — the exact bug that hid installed
/// ollama models behind two-line rows.
fn model_row(m: &ModelInfo, ctx: &str, key: &str) -> String {
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
    format!("{id:<34} {ctx:>5} {price:<10} {wire:<7}{key}")
}

impl ModelPicker {
    /// Rows after filtering. A vendor lock (the provider stage of
    /// `/provider`) narrows the pool, `configured_only` drops models
    /// whose key is missing, and the substring filter applies last.
    /// The unified list (no vendor lock, `configured_only` off) puts
    /// models of configured providers first — keyless or keyed-in —
    /// the rest after; order is stable within both groups.
    pub fn rows(&self) -> Vec<&ModelInfo> {
        let f = self.filter.to_lowercase();
        let matches = |m: &ModelInfo| {
            self.vendor
                .as_deref()
                .is_none_or(|v| m.id.split('/').next() == Some(v))
                && (!self.configured_only || m.key_env.is_empty() || m.key_set)
                && (f.is_empty() || m.id.to_lowercase().contains(&f))
        };
        if self.unified() {
            let mut configured: Vec<&ModelInfo> = Vec::new();
            let mut rest: Vec<&ModelInfo> = Vec::new();
            for m in self.models.iter().filter(|m| matches(m)) {
                if Self::configured(m) {
                    configured.push(m);
                } else {
                    rest.push(m);
                }
            }
            configured.extend(rest);
            configured
        } else {
            self.models.iter().filter(|m| matches(m)).collect()
        }
    }

    /// The unified `/model` list: no vendor lock, configured-only off.
    fn unified(&self) -> bool {
        !self.configured_only && self.vendor.is_none()
    }

    /// A model of a configured provider: keyless, or its key is set.
    /// Drives the partition in [`rows`](Self::rows) and the
    /// `─ not configured ─` separator in the modal render.
    fn configured(m: &ModelInfo) -> bool {
        m.key_env.is_empty() || m.key_set
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
/// The `/provider` picker: choose a provider to connect before any of
/// its models can be selected.
#[derive(Debug, Clone)]
pub struct ProviderPicker {
    pub providers: Vec<ProviderInfo>,
    /// `(vendor, model count)` pairs backing the per-row detail line.
    pub counts: Vec<(String, usize)>,
    /// Selected row.
    pub selected: usize,
    /// Typed filter (substring over name or env var).
    pub filter: String,
}

impl ProviderPicker {
    /// Build the picker with per-vendor model counts from the catalog.
    pub fn new(providers: Vec<ProviderInfo>, models: &[ModelInfo]) -> Self {
        let mut counts: Vec<(String, usize)> = Vec::new();
        for m in models {
            if let Some(vendor) = m.id.split('/').next() {
                match counts.iter_mut().find(|(v, _)| v == vendor) {
                    Some((_, n)) => *n += 1,
                    None => counts.push((vendor.to_string(), 1)),
                }
            }
        }
        Self {
            providers,
            counts,
            selected: 0,
            filter: String::new(),
        }
    }

    /// Rows after filtering: `(provider, detail)` pairs, detail reading
    /// `ENV ✓|✗ · n models` or `keyless · local · n models`.
    pub fn rows(&self) -> Vec<(&ProviderInfo, String)> {
        let f = self.filter.to_lowercase();
        self.providers
            .iter()
            .filter(|p| {
                f.is_empty()
                    || p.name.to_lowercase().contains(&f)
                    || p.env_var.to_lowercase().contains(&f)
            })
            .map(|p| {
                let n = self
                    .counts
                    .iter()
                    .find(|(v, _)| *v == p.name)
                    .map_or(0, |(_, n)| *n);
                let key = if p.env_var.is_empty() {
                    "keyless · local".to_string()
                } else if p.key_set {
                    format!("{} ✓", p.env_var)
                } else {
                    format!("{} ✗", p.env_var)
                };
                (
                    p,
                    format!("{key} · {n} model{}", if n == 1 { "" } else { "s" }),
                )
            })
            .collect()
    }

    /// The provider Enter connects (or, when already keyed, whose model
    /// list Enter opens).
    pub fn pick(&self) -> Option<&ProviderInfo> {
        self.rows().get(self.selected).map(|(p, _)| *p)
    }
}

/// Mark every model (by `key_env`) and provider (by `env_var`) keyed by
/// `env_var` as connected, so pickers reflect a key saved this session.
fn mark_key_set(models: &mut [ModelInfo], providers: &mut [ProviderInfo], env_var: &str) {
    for m in models.iter_mut() {
        if m.key_env == env_var {
            m.key_set = true;
        }
    }
    for p in providers.iter_mut() {
        if p.env_var == env_var {
            p.key_set = true;
        }
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
    /// Provider picker (`/provider`).
    Provider(ProviderPicker),
    /// API key prompt for a provider.
    Key(KeyPrompt),
    /// Spill-file viewer (/spills).
    Spills {
        /// Spilled-output file paths, oldest first.
        items: Vec<String>,
        /// Selected row index.
        selected: usize,
    },
    /// MCP prompt picker (/prompt): `server/name (args)` rows.
    Prompts {
        /// Prompt rows in inventory order.
        items: Vec<String>,
        /// Selected row index.
        selected: usize,
    },
    /// Double-Esc rewind menu: pick a past user message to rewind to
    /// (or edit & resend).
    Rewind {
        /// (turns-from-end, prompt text); 1 = most recent.
        items: Vec<(usize, String)>,
        /// Selected row.
        selected: usize,
    },
    /// Memory viewer (/memory): project + user memory files, plus the
    /// staged-inbox review flow (⏎ accept → project, u → user,
    /// d discard).
    Memory {
        /// Rendered rows (path header + content lines).
        rows: Vec<String>,
        /// Staged memory proposals from `.ka/memory/inbox.md`.
        inbox: Vec<String>,
        /// Selected inbox row.
        selected: usize,
    },
    /// Usage dashboard (/usage): session + recent session rows.
    Usage {
        /// Rendered rows (section headers + figures + total).
        rows: Vec<String>,
    },
    /// Context breakdown (/context): rendered rows.
    Context {
        /// Rendered rows (bars + footer).
        rows: Vec<String>,
    },
    /// Strand tree (/tree): current session + descendants.
    Tree {
        /// Rendered rows (`title · date · N msgs`).
        items: Vec<String>,
        /// Switch targets aligned with `items`.
        targets: Vec<String>,
        /// Selected row index.
        selected: usize,
    },
    /// /tasks picker: background task/job/DAP rows; Enter pages the
    /// selected task's full result.
    Tasks {
        /// Row label plus its parseable task id (None for job-/dap rows).
        entries: Vec<(Option<u64>, String)>,
        /// Selected row index.
        selected: usize,
    },
    /// Full result of one background task (the pager over
    /// `Command::TaskDetail`).
    TaskDetail {
        /// Task id (roster `t-<id>`).
        id: u64,
        /// Uncapped result text.
        text: String,
        /// Scroll anchor (None = pinned to the tail).
        scroll: Option<usize>,
    },
    /// /debug overlay: live DAP session roster rows.
    Debug {
        /// Rendered roster rows.
        rows: Vec<String>,
        /// Scroll anchor (None = pinned to the tail).
        scroll: Option<usize>,
    },
    /// Live delegate todo list (bottom-strip button / alt+O).
    Todos {
        /// Rendered todo rows.
        rows: Vec<ratatui::text::Line<'static>>,
    },
    /// Discovered skills, agents, MCP servers (bottom strip / ctrl+T).
    Skills {
        /// Rendered inventory rows.
        rows: Vec<ratatui::text::Line<'static>>,
    },
    /// Session facts: cwd:branch, session, model, effort, cost, ctx.
    Info {
        /// Rendered fact rows.
        rows: Vec<ratatui::text::Line<'static>>,
    },
    /// Help overlay.
    Help,
}

/// Which modal the next roster event should open instead of rendering
/// transcript rows (set by `/tasks` and `/debug`, consumed once).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingModal {
    Tasks,
    Debug,
}

/// Visible rows in the TaskDetail/Debug pager modals — the render
/// height cap (30) minus the box chrome (border 2 + padding 2 + 1).
/// The key side shrinks this to the transcript pane the same way the
/// renderer's `.min(modal_area.height)` does, so scrolling always
/// matches what is on screen.
const PAGER_VISIBLE: usize = 25;

/// The pager's window height for a transcript pane `view_rows` tall —
/// the same number the renderer derives from its clamped box height,
/// so page keys never step past what is actually shown.
fn pager_visible(view_rows: usize) -> usize {
    PAGER_VISIBLE.min(view_rows.saturating_sub(5)).max(1)
}

/// Task id off a /tasks roster row: `t-<id>  …` → `Some(id)`, any
/// other row shape (job-`, dap, placeholders) → None.
fn task_id_of_row(row: &str) -> Option<u64> {
    let row = row.trim();
    let digits = row.strip_prefix("t-")?;
    let end = digits.find(char::is_whitespace).unwrap_or(digits.len());
    digits[..end].parse().ok()
}

/// Push the terminal sequences for one mouse mode. Captured = SGR
/// button reporting (wheel scrolls, click zones live, ⇧drag selects);
/// native = capture off + DECSET 1007, so the wheel still arrives —
/// as ↑/↓ — while plain drag selection is the terminal's own.
fn apply_mouse_mode(mouse_captured: bool) {
    let mut out = std::io::stdout();
    if mouse_captured {
        let _ = std::io::Write::write_all(&mut out, b"\x1b[?1007l");
        let _ = std::io::Write::flush(&mut out);
        let _ = crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture);
    } else {
        let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture);
        let _ = std::io::Write::write_all(&mut out, b"\x1b[?1007h");
        let _ = std::io::Write::flush(&mut out);
    }
}

/// The `[tui] mouse` value for a capture flag (SaveSettings payload) —
/// `"capture"` (default) or `"native"`, matching the config docs.
fn mouse_setting(captured: bool) -> Option<String> {
    Some(if captured {
        "capture".to_string()
    } else {
        "native".to_string()
    })
}

/// The Ctrl+M toggle toast: mode names match the persisted
/// `[tui] mouse` values.
fn mouse_mode_text(captured: bool) -> String {
    format!(
        "🖱 mouse {} — {}",
        if captured { "capture" } else { "native" },
        if captured {
            "wheel scrolls, strip buttons clickable, ⇧drag selects, right-click pastes; ↑/↓ = prompt history"
        } else {
            "plain drag selects natively; the wheel scrolls the chat (kitty: no alternate-scroll — Ctrl+M back to capture for the wheel); ↑/↓ scroll, Ctrl+P/N = prompt history"
        }
    )
}

/// Key handling shared by the pager modals (TaskDetail, Debug): the
/// page keys scroll the window, Home jumps to the top, End re-pins to
/// the tail; any other key closes. Returns true when the modal should
/// close. `visible` mirrors the renderer's window height.
fn pager_keys(
    code: crossterm::event::KeyCode,
    total: usize,
    scroll: &mut Option<usize>,
    visible: usize,
) -> bool {
    use crossterm::event::KeyCode;
    match code {
        KeyCode::PageUp => {
            page_up(scroll, total, visible);
            false
        }
        KeyCode::PageDown => {
            page_down(scroll, total, visible);
            false
        }
        KeyCode::Home => {
            if total > visible {
                *scroll = Some(0);
            }
            false
        }
        KeyCode::End => {
            *scroll = None;
            false
        }
        _ => true,
    }
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
    /// Vendor to drill into (its model list) once the key is saved.
    pub drill: Option<String>,
    /// Model selector to apply once the key saves (the unified
    /// `/model` pick on a keyed-but-unset vendor).
    pub pending_model: Option<String>,
}

/// Run the TUI over an engine handle. Blocks until exit.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    mut commands: mpsc::Sender<Command>,
    mut events: mpsc::Receiver<Event>,
    initial_model: &str,
    providers: Vec<ProviderInfo>,
    models: Vec<ModelInfo>,
    agents: Vec<(String, String)>,
    header_glyph: &str,
    notify: NotifySettings,
    mouse_capture: bool,
    fresh: bool,
    session_cwd: std::path::PathBuf,
) -> std::io::Result<Exit> {
    let _ = AGENTS.set(agents.clone());
    let _ = NOTIFY.set(notify);
    let _ = PLAN_CWD.set(session_cwd);
    // Register raw mode in THIS crate's crossterm before ratatui flips
    // it through its own (0.28) copy: crossterm's parser decides whether
    // `\n` means Enter or Ctrl+J by reading its own per-crate raw-mode
    // flag, and only the copy that called enable_raw_mode knows. Without
    // this, every lone \n sends the draft instead of breaking the line,
    // and run_external restores a raw state for editors instead of the
    // cooked one it started from.
    let _ = crossterm::terminal::enable_raw_mode();
    let mut terminal = ratatui::init();
    // Kitty keyboard protocol: Shift+Enter as a distinct key + bracketed
    // paste. Best effort — hosts without support degrade to plain Enter;
    // Ctrl+J always works as the newline fallback. Mouse mode defaults
    // to NATIVE: no capture, so plain drag selects/pastes natively and
    // the wheel scrolls the chat via DECSET 1007 (kitty: no 1007 — set
    // `[tui] mouse = "capture"` for the wheel). Capture mode restores
    // the dashboard: wheel reporting, ▲▼/skills-header click zones,
    // ⇧drag selects, sidebar visible. Ctrl+M toggles at runtime
    // (kitty-protocol terminals only — plain terminals read Ctrl+M as
    // Enter) and persists the choice.
    let _ = crossterm::execute!(
        std::io::stdout(),
        crossterm::event::PushKeyboardEnhancementFlags(
            crossterm::event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | crossterm::event::KeyboardEnhancementFlags::REPORT_EVENT_TYPES
        ),
        crossterm::event::EnableBracketedPaste,
    );
    // native by default (plain drag selection, full-width chat, wheel
    // via alternate-scroll). `[tui] mouse = "capture"` starts captured —
    // wheel scroll, ▲▼/skills clicks, right-click paste, ⇧drag selects,
    // sidebar visible. Ctrl+M toggles and persists either way.
    if mouse_capture {
        let _ = crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture);
    } else {
        // native mode: alternate scroll translates the wheel to ↑/↓,
        // which scroll the chat there
        let _ = std::io::Write::write_all(&mut std::io::stdout(), b"\x1b[?1007h");
        let _ = std::io::Write::flush(&mut std::io::stdout());
    }
    let result = app(
        &mut terminal,
        &mut commands,
        &mut events,
        initial_model,
        providers,
        models,
        agents,
        header_glyph,
        mouse_capture,
        fresh,
    )
    .await;
    let _ = crossterm::execute!(
        std::io::stdout(),
        crossterm::event::PopKeyboardEnhancementFlags,
        crossterm::event::DisableBracketedPaste,
        crossterm::event::DisableMouseCapture
    );
    // native sessions leave DECSET 1007 set; release it so the next
    // alt-screen app starts from a clean slate
    let _ = std::io::Write::write_all(&mut std::io::stdout(), b"\x1b[?1007l");
    let _ = std::io::Write::flush(&mut std::io::stdout());
    ratatui::restore();
    // Two linked crossterm copies keep separate raw-mode snapshots: our
    // enable at the top of this function runs first (saving cooked
    // state), then ratatui::init enables raw again through ratatui's own
    // 0.28 copy — whose snapshot is therefore already-raw state.
    // ratatui::restore only unwinds that copy, so disabling through THIS
    // crate's copy here is what restores cooked mode; without it the
    // shell is left unable to echo keystrokes.
    let _ = crossterm::terminal::disable_raw_mode();
    let (exit, resume) = result?;
    if let Some((hint, resume_cmd)) = resume {
        println!();
        println!("{hint}");
        // the shell's ↑ should offer the way back too (best effort —
        // bash only shows it in new shells / after `history -n`)
        append_shell_history(&resume_cmd);
    }
    Ok(exit)
}

#[allow(clippy::too_many_arguments)]
async fn app(
    terminal: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    commands: &mut mpsc::Sender<Command>,
    events: &mut mpsc::Receiver<Event>,
    initial_model: &str,
    mut providers: Vec<ProviderInfo>,
    mut models: Vec<ModelInfo>,
    agents: Vec<(String, String)>,
    header_glyph: &str,
    mut mouse_captured: bool,
    mut fresh: bool,
) -> std::io::Result<(Exit, Option<(String, String)>)> {
    use crossterm::event::{Event as TermEvent, KeyCode, KeyModifiers};
    let _ = agents.clone();

    let mut scroll: Option<usize> = None;
    let mut transcript = Transcript::default();

    let mut view_rows;
    let mut input = InputBuffer::default();
    let mut meters = Meters {
        model: initial_model.to_string(),
        // default mode is Free (yolo default); the bootstrap
        // ModeChanged event re-renders this with the true state
        mode: mode_label(ka_protocol::Mode::Free).to_string(),
        ..Default::default()
    };
    let mut busy = false;
    let mut busy_since: Option<Instant> = None;
    let mut quit_armed: Option<Instant> = None;
    // double-Esc (rewind menu) tracking
    let mut last_esc: Option<Instant> = None;
    let mut last_user: Option<String> = None;
    let mut last_error: Option<String> = None;
    let mut live_cache: Option<(String, u16, Vec<ratatui::text::Line<'static>>, Instant)> = None;
    let mut turn_ended = false;
    let mut pending: Option<PendingAsk> = None;
    // user-deferred prompts (`+` while busy), held locally and auto-sent
    // FIFO, one per turn settle (see the TurnFinished arm below)
    let mut queue: Vec<String> = Vec::new();
    let mut turn_produced = false;
    let mut turn_usage: Option<(u64, u64, u64)> = None; // (input+cache_read, total_in_seen, output)
    let mut plan_started: Option<std::time::SystemTime> = None;
    let mut current_assistant = String::new();
    let mut spills: Vec<String> = Vec::new();
    let mut current_thought = String::new();
    let mut current_tool = String::new();
    let mut live_tool: Option<LiveTool> = None;
    // the most recent finished call's (entry, call) slot: Ctrl+O and
    // click-to-expand toggle its inline output
    let mut last_tool_ref: Option<(usize, usize)> = None;
    // transient action feedback: (message, shown-at); auto-expires
    let mut toast: Option<(String, Instant)> = None;
    // pending image attachment: staged by `/image <path>`, consumed by
    // the next sent prompt
    let mut pending_image: Option<ka_protocol::ImagePart> = None;
    let mut sidebar = SidebarState {
        cwd: shorten_cwd(&std::env::current_dir().unwrap_or_default()),
        branch: detect_branch(),
        ..Default::default()
    };
    let mut exit = None;
    let mut slash_popup: Option<SlashPopup> = None;
    let mut path_popup: Option<PathPopup> = None;
    // last /find query + the row to resume a bare /find after
    let mut find_last: Option<(String, usize)> = None;
    let mut modal: Option<Modal> = None;
    // /tasks and /debug set this so the roster event opens the modal
    // instead of rendering transcript rows; consumed once
    let mut pending_modal: Option<PendingModal> = None;
    let mut mode_picker: Option<ModePicker> = None;
    // clickable sidebar regions + ▲▼ title-row jump targets, refreshed
    // every frame by render()
    let strip_zone: std::cell::Cell<Option<StripZones>> = std::cell::Cell::new(None);
    let title_arrows: std::cell::Cell<Option<TitleArrows>> = std::cell::Cell::new(None);
    let tx_content: std::cell::Cell<Option<(ratatui::layout::Rect, usize)>> =
        std::cell::Cell::new(None);
    // the transcript width the last render actually used (0 until the
    // first frame): the tick-side markdown cache keys on it so cache
    // and frame can never disagree about the band/surface width
    let tx_width: std::cell::Cell<u16> = std::cell::Cell::new(0);
    let mut term_events = crossterm::event::EventStream::new();
    let mut spin = tokio::time::interval(Duration::from_millis(120));

    while exit.is_none() {
        let busy_now = busy;
        let ask = pending.clone();
        let input_snapshot = input.text.clone();
        let rsearch = input.search_title();
        let (term_w, term_h) = match terminal.size() {
            Ok(s) => (s.width, s.height),
            Err(_) => (80, 24),
        };
        // the markdown cache keys on the width render actually used
        // (one tick of staleness after a resize is fine: the surface
        // fill re-pads live rows at the frame's width)
        let last_w = tx_width.get();
        let md_width = if last_w == 0 {
            transcript_width(term_w)
        } else {
            last_w
        };
        let input_h = input_height(input_area_rows(
            mode_picker.as_ref(),
            &input.text,
            input_inner_w(term_w),
        ));
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
            live_cache.as_ref().map(|(_, _, rows, _)| LiveBlock {
                thought: current_thought.clone(),
                tool_header: current_tool.clone(),
                live_tool: live_tool.clone(),
                md: rows.clone(),
            })
        } else {
            live_cache = None;
            None
        };
        turn_ended = false;
        let cursor = input.cursor;
        terminal.draw(|frame| {
            render(
                frame,
                &mut transcript,
                scroll,
                &input_snapshot,
                cursor,
                busy_now,
                busy_since,
                Instant::now(),
                queue.len(),
                ask.as_ref(),
                live.as_ref(),
                slash_popup.as_ref(),
                path_popup.as_ref(),
                rsearch.as_deref(),
                modal.as_ref(),
                mode_picker.as_ref(),
                &meters,
                &sidebar,
                fresh,
                mouse_captured,
                header_glyph,
                &strip_zone,
                toast
                    .as_ref()
                    .filter(|(_, at)| at.elapsed() < TOAST_TTL)
                    .map(|(m, _)| m.as_str()),
                &title_arrows,
                &tx_content,
                &tx_width,
            );
        })?;

        tokio::select! {
            biased;
            maybe_term = term_events.next() => {
                if let Some(Ok(TermEvent::Key(key))) = maybe_term {
                    if key.kind == crossterm::event::KeyEventKind::Release {
                        continue;
                    }
                    // Ctrl+C, ahead of every other capture (asks, modals,
                    // popups all swallow chars): busy aborts the run; a
                    // reverse search is cancelled; a non-empty input is
                    // cleared; an empty input quits on the second press
                    if (key.code, key.modifiers) == (KeyCode::Char('c'), KeyModifiers::CONTROL) {
                        if busy {
                            let _ = commands.send(Command::Abort).await;
                            quit_armed = None;
                        } else if input.searching() {
                            input.search_cancel();
                            quit_armed = None;
                        } else if !input.text.is_empty() {
                            input.clear_draft();
                            slash_popup = update_suggestions(&input.text);
                            quit_armed = None;
                        } else if quit_armed
                            .is_some_and(|at| at.elapsed() < std::time::Duration::from_secs(3))
                        {
                            exit = Some(Exit::Quit);
                        } else {
                            quit_armed = Some(Instant::now());
                            pop_toast(&mut toast, "press ctrl+c again to exit");
                        }
                        continue;
                    }
                    // any other keypress breaks a quit arm
                    quit_armed = None;
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
                                let label =
                                    ask.options.get(choice).cloned().unwrap_or_default();
                                pending = None;
                                transcript
                                    .push_separated(Line::Info(format!("· {label}")));
                                let _ = commands.send(Command::Answer { question: id, choice }).await;
                            }
                            KeyCode::Esc => {
                                let id = ask.id.clone();
                                let deny = ask.options.len().saturating_sub(1);
                                let label = ask.options.get(deny).cloned().unwrap_or_default();
                                pending = None;
                                transcript.push_separated(Line::Info(format!("· {label}")));
                                let _ = commands.send(Command::Answer { question: id, choice: deny }).await;
                            }
                            // direct pick: 1..9 answers that option at once
                            KeyCode::Char(c) if c.is_ascii_digit() && c != '0' => {
                                let idx = (c as u8 - b'1') as usize;
                                if idx < ask.options.len() {
                                    let id = ask.id.clone();
                                    let label = ask.options[idx].clone();
                                    pending = None;
                                    transcript
                                        .push_separated(Line::Info(format!("· {label}")));
                                    let _ =
                                        commands.send(Command::Answer { question: id, choice: idx }).await;
                                }
                            }
                            _ => {}
                        }
                        continue;
                    }
                    // The /mode picker captures input next: it borrows the
                    // input box exactly like the permission ask does
                    if let Some(pk) = mode_picker.as_mut() {
                        match key.code {
                            KeyCode::Esc => mode_picker = None,
                            KeyCode::Up => pk.selected = pk.selected.saturating_sub(1),
                            KeyCode::Down => {
                                if pk.selected + 1 < MODE_CHOICES.len() {
                                    pk.selected += 1;
                                }
                            }
                            // direct pick: 1..4 jump to that row
                            KeyCode::Char(c) if c.is_ascii_digit() && c != '0' => {
                                let idx = (c as u8 - b'1') as usize;
                                if idx < MODE_CHOICES.len() {
                                    pk.selected = idx;
                                }
                            }
                            KeyCode::Enter => {
                                let mode = pk.pick();
                                mode_picker = None;
                                let _ = commands.send(Command::SetMode { mode }).await;
                                // persist like the model pick does: mode wins,
                                // the other settings keep their saved values
                                let _ = commands
                                    .send(Command::SaveSettings {
                                        model: None,
                                        effort: None,
                                        mode: Some(mode),
                                        mouse: None,
                                    })
                                    .await;
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
                                    fresh = target == "new";
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
                                    let drill = prompt.drill.clone();
                                    let env_var = prompt.env_var.clone();
                                    let pending_model = prompt.pending_model.clone();
                                    let apply = pending_model.filter(|_| !value.is_empty());
                                    if !value.is_empty() {
                                        let _ = commands
                                            .send(Command::SaveApiKey {
                                                env_var: env_var.clone(),
                                                value,
                                            })
                                            .await;
                                        // the saved key resolves in this
                                        // process now; reflect it in both
                                        // lists so ✓/✗ marks stay truthful
                                        mark_key_set(&mut models, &mut providers, &env_var);
                                        pop_toast(&mut toast, "✓ key saved");
                                    }
                                    if let Some(selector) = apply {
                                        // the key unblocked this unified
                                        // `/model` pick: apply it now
                                        let _ = commands
                                            .send(Command::SetModel {
                                                selector: selector.clone(),
                                            })
                                            .await;
                                        let _ = commands
                                            .send(Command::SaveSettings {
                                                model: Some(selector),
                                                effort: None,
                                                mode: None,
                                                mouse: None,
                                            })
                                            .await;
                                        modal = None;
                                    } else {
                                        modal = drill.map(|vendor| {
                                            Modal::Model(ModelPicker {
                                                models: models.clone(),
                                                vendor: Some(vendor),
                                                configured_only: true,
                                                selected: 0,
                                                filter: String::new(),
                                            })
                                        });
                                    }
                                }
                                KeyCode::Char(c) => prompt.input.push(c),
                                _ => {}
                            },
                            Modal::Provider(picker) => match key.code {
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
                                    let picked = picker.pick().cloned();
                                    match picked {
                                        // connected (or keyless): list its models
                                        Some(p) if p.env_var.is_empty() || p.key_set => {
                                            modal = Some(Modal::Model(ModelPicker {
                                                models: models.clone(),
                                                vendor: Some(p.name),
                                                configured_only: true,
                                                selected: 0,
                                                filter: String::new(),
                                            }));
                                        }
                                        // unconfigured keyed provider: ask
                                        // for the key, then drill into its
                                        // models once it saves
                                        Some(p) => {
                                            let doc_url = models
                                                .iter()
                                                .find(|m| {
                                                    m.id.split('/').next()
                                                        == Some(p.name.as_str())
                                                        && !m.key_env.is_empty()
                                                })
                                                .map(|m| m.doc_url.clone())
                                                .unwrap_or_default();
                                            modal = Some(Modal::Key(KeyPrompt {
                                                env_var: p.env_var,
                                                provider: p.name.clone(),
                                                doc_url,
                                                input: String::new(),
                                                drill: Some(p.name),
                                                pending_model: None,
                                            }));
                                        }
                                        None => modal = None,
                                    }
                                }
                                KeyCode::Char(c) => {
                                    picker.filter.push(c);
                                    picker.selected = 0;
                                }
                                _ => {}
                            },
                            Modal::Model(picker) => match key.code {
                                KeyCode::Esc => {
                                    // a vendor-locked list returns to the
                                    // provider stage; the plain list closes
                                    modal = picker.vendor.as_deref().map(|_| {
                                        Modal::Provider(ProviderPicker::new(
                                            providers.clone(),
                                            &models,
                                        ))
                                    });
                                }
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
                                        // a keyed model without its key
                                        // asks for one first; the pick
                                        // applies once the key saves
                                        let missing = picker
                                            .models
                                            .iter()
                                            .find(|m| m.id == selector)
                                            .is_some_and(|m| {
                                                !m.key_set && !m.key_env.is_empty()
                                            });
                                        if missing {
                                            if let Some(m) =
                                                picker.models.iter().find(|m| m.id == selector)
                                            {
                                                modal = Some(Modal::Key(KeyPrompt {
                                                    env_var: m.key_env.clone(),
                                                    provider: selector
                                                        .split('/')
                                                        .next()
                                                        .unwrap_or("")
                                                        .to_string(),
                                                    doc_url: m.doc_url.clone(),
                                                    input: String::new(),
                                                    drill: None,
                                                    pending_model: Some(selector.clone()),
                                                }));
                                                continue;
                                            }
                                        }
                                        let _ = commands
                                            .send(Command::SetModel {
                                                selector: selector.clone(),
                                            })
                                            .await;
                                        // the pick also becomes the default for
                                        // future conversations
                                        let _ = commands
                                            .send(Command::SaveSettings {
                                                model: Some(selector),
                                                effort: None,
                                                mode: None,
                                                mouse: None,
                                            })
                                            .await;
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
                                            // hand off to the /mode picker: it
                                            // borrows the input box, so the
                                            // settings panel closes first
                                            let mode = panel.mode;
                                            modal = None;
                                            mode_picker = Some(ModePicker::for_mode(mode));
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
                                                mouse: None,
                                            })
                                            .await;
                                    }
                                    _ => {}
                                }
                            }
                            Modal::Rewind { items, selected } => {
                                match key.code {
                                    KeyCode::Esc => modal = None,
                                    KeyCode::Up => {
                                        *selected = selected.saturating_sub(1);
                                    }
                                    KeyCode::Down => {
                                        if *selected + 1 < items.len() {
                                            *selected += 1;
                                        }
                                    }
                                    KeyCode::Enter | KeyCode::Char('r') | KeyCode::Char('e') => {
                                        let edit = key.code == KeyCode::Char('e');
                                        let (turns, prompt) = items[*selected].clone();
                                        modal = None;
                                        let _ = commands
                                            .send(Command::Rewind { turns: turns as u32 })
                                            .await;
                                        transcript.rewind_user(turns);
                                        if edit {
                                            input.text = prompt;
                                            input.cursor = input.text.chars().count();
                                            transcript.push_separated(Line::Info(
                                                "✎ edit & resend — ⏎ sends when ready"
                                                    .into(),
                                            ));
                                        } else {
                                            transcript.push_separated(Line::Info(format!(
                                                "⏪ rewound {turns} turn(s) — files unchanged (/undo restores edits)"
                                            )));
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            Modal::Memory {
                                rows,
                                inbox,
                                selected,
                            } => {
                                match key.code {
                                    KeyCode::Esc => modal = None,
                                    KeyCode::Up if !inbox.is_empty() => {
                                        *selected = selected.saturating_sub(1);
                                    }
                                    KeyCode::Down if !inbox.is_empty() => {
                                        *selected = (*selected + 1).min(inbox.len() - 1);
                                    }
                                    KeyCode::Enter
                                    | KeyCode::Char('a')
                                    | KeyCode::Char('u')
                                    | KeyCode::Char('d')
                                        if !inbox.is_empty() =>
                                    {
                                        let cwd = std::env::current_dir()
                                            .unwrap_or_else(|_| std::path::PathBuf::from("."));
                                        let line = inbox.remove(*selected);
                                        if key.code != KeyCode::Char('d') {
                                            let user = key.code == KeyCode::Char('u');
                                            // best-effort: a failed append leaves the
                                            // note staged for a later retry
                                            if accept_memory_note(&cwd, &line, user).is_ok() {
                                                write_memory_inbox(&cwd, inbox);
                                            } else {
                                                inbox.insert(*selected, line);
                                            }
                                        } else {
                                            write_memory_inbox(&cwd, inbox);
                                        }
                                        if *selected >= inbox.len() {
                                            *selected = inbox.len().saturating_sub(1);
                                        }
                                        *rows = memory_modal_rows(&cwd);
                                    }
                                    _ => {}
                                }
                            }
                                            Modal::Usage { .. } => {
                                                modal = None;
                                            }
                                            Modal::Todos { .. } | Modal::Skills { .. } | Modal::Info { .. } => {
                                                modal = None;
                                            }
                            Modal::Context { .. } => {
                                modal = None;
                            }
                            Modal::Tree {
                                items,
                                targets,
                                selected,
                            } => match key.code {
                                KeyCode::Esc => modal = None,
                                KeyCode::Up => {
                                    *selected = selected.saturating_sub(1);
                                }
                                KeyCode::Down => {
                                    if *selected + 1 < items.len() {
                                        *selected += 1;
                                    }
                                }
                                KeyCode::Enter => {
                                    if let Some(id) = targets.get(*selected).cloned() {
                                        fresh = id == "new";
                                        let _ = commands
                                            .send(Command::SwitchStrand { id })
                                            .await;
                                        busy = true;
                                    }
                                }
                                _ => {}
                            },
                            Modal::Prompts { items, selected } => match key.code {
                                KeyCode::Esc => modal = None,
                                KeyCode::Up => {
                                    *selected = selected.saturating_sub(1);
                                }
                                KeyCode::Down => {
                                    if *selected + 1 < items.len() {
                                        *selected += 1;
                                    }
                                }
                                KeyCode::Enter => {
                                    if let Some(row) = items.get(*selected).cloned() {
                                        let (spec, args) = parse_prompt_row(&row);
                                        modal = None;
                                        if args.is_empty() {
                                            if let Some((server, name)) = spec {
                                                let _ = commands
                                                    .send(Command::CallPrompt {
                                                        server,
                                                        name,
                                                        args: Default::default(),
                                                    })
                                                    .await;
                                                busy = true;
                                            }
                                        } else {
                                            // arg-picking: prefill the input; the
                                            // user appends key=value pairs
                                            if let Some((server, name)) = spec {
                                                input.text =
                                                    format!("/prompt {server}/{name} ");
                                                input.cursor = input.text.chars().count();
                                            }
                                        }
                                    }
                                }
                                _ => {}
                            },
                            Modal::Spills { items, selected } => match key.code {
                                KeyCode::Esc => modal = None,
                                KeyCode::Up => {
                                    *selected = selected.saturating_sub(1);
                                }
                                KeyCode::Down => {
                                    if *selected + 1 < items.len() {
                                        *selected += 1;
                                    }
                                }
                                KeyCode::Enter => {
                                    if let Some(path) = items.get(*selected).cloned() {
                                        modal = None;
                                        if !std::path::Path::new(&path).exists() {
                                            transcript.push_separated(Line::Info(format!(
                                                "spill file is gone: {path}"
                                            )));
                                        } else {
                                            let opened =
                                                match std::env::var("PAGER")
                                                    .ok()
                                                    .filter(|p| !p.is_empty())
                                                {
                                                    Some(pager) => run_external(
                                                        &pager,
                                                        &[path.as_str()],
                                                        terminal,
                                                        mouse_captured,
                                                    ),
                                                    None => run_external(
                                                        "less",
                                                        &["-R", path.as_str()],
                                                        terminal,
                                                        mouse_captured,
                                                    ),
                                                };
                                            if let Err(e) = opened {
                                                transcript.push_separated(Line::Info(format!("pager failed: {e}")));
                                            }
                                        }
                                    }
                                }
                                _ => {}
                            },
                            Modal::Tasks { entries, selected } => match key.code {
                                KeyCode::Esc => modal = None,
                                KeyCode::Up => {
                                    *selected = selected.saturating_sub(1);
                                }
                                KeyCode::Down => {
                                    if *selected + 1 < entries.len() {
                                        *selected += 1;
                                    }
                                }
                                KeyCode::Enter => {
                                    // page the selected task's full result;
                                    // the modal stays open and is replaced
                                    // when TaskDetail arrives
                                    if let Some((Some(id), _)) = entries.get(*selected) {
                                        let _ = commands.send(Command::TaskDetail { id: *id }).await;
                                    }
                                }
                                _ => {}
                            },
                            Modal::TaskDetail { text, scroll, .. } => {
                                let visible = pager_visible(view_rows);
                                if pager_keys(key.code, text.lines().count(), scroll, visible) {
                                    modal = None;
                                }
                            }
                            Modal::Debug { rows, scroll } => {
                                let visible = pager_visible(view_rows);
                                if pager_keys(key.code, rows.len(), scroll, visible) {
                                    modal = None;
                                }
                            }
                        }
                        continue;
                    }
                    // Path-completion popup keys. Mutually exclusive with the
                    // slash popup; unlisted keys close the popup and fall
                    // through to the main match.
                    if path_popup.is_some() {
                        enum PathKey {
                            Navigate,
                            Accept,
                            Backspace,
                            Char(char),
                            Close,
                            Fallthrough,
                        }
                        let intent = match key.code {
                            KeyCode::Up | KeyCode::Down => PathKey::Navigate,
                            KeyCode::Tab | KeyCode::Enter => PathKey::Accept,
                            KeyCode::Esc => PathKey::Close,
                            KeyCode::Backspace => PathKey::Backspace,
                            KeyCode::Char(c) => {
                                if key
                                    .modifiers
                                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                                {
                                    // shortcuts like Ctrl+C must not be eaten
                                    // as literal input: close and fall through
                                    PathKey::Fallthrough
                                } else {
                                    PathKey::Char(c)
                                }
                            }
                            _ => PathKey::Fallthrough,
                        };
                        match intent {
                            PathKey::Navigate => {
                                if let Some(pp) = path_popup.as_mut() {
                                    match key.code {
                                        KeyCode::Up => {
                                            pp.selected = pp.selected.saturating_sub(1)
                                        }
                                        _ => {
                                            if pp.selected + 1 < pp.entries.len() {
                                                pp.selected += 1;
                                            }
                                        }
                                    }
                                }
                            }
                            PathKey::Accept => {
                                let snapshot = path_popup.as_ref().and_then(|pp| {
                                    pp.entries.get(pp.selected).cloned().map(|e| {
                                        (e, pp.token_start, pp.prefix.clone(), pp.mentions)
                                    })
                                });
                                match snapshot {
                                    None => path_popup = None,
                                    Some(((name, is_dir), tok_start, prefix, mentions)) => {
                                        let tok_len = path_token(&input.text, input.cursor)
                                            .map(|(_, t)| t.chars().count())
                                            .unwrap_or(0);
                                        let suffix = if is_dir {
                                            "/"
                                        } else if mentions {
                                            // a finished mention reads like a word
                                            " "
                                        } else {
                                            ""
                                        };
                                        let insert = format!("{prefix}{name}{suffix}");
                                        let (text, cursor) = complete_token(
                                            &input.text,
                                            tok_start,
                                            tok_len,
                                            &insert,
                                        );
                                        input.text = text;
                                        input.cursor = cursor;
                                        if is_dir {
                                            // descend: list the entered directory
                                            let word =
                                                path_token(&input.text, input.cursor)
                                                    .map(|(_, t)| t)
                                                    .unwrap_or_default();
                                            let (matches, new_prefix) = if mentions {
                                                let rel =
                                                    word.strip_prefix('@').unwrap_or(&word);
                                                let root = std::env::current_dir()
                                                    .unwrap_or_default()
                                                    .join(rel);
                                                (walk_files(&root, WALK_CAP), format!("@{rel}"))
                                            } else {
                                                let (dir2, base2, _) =
                                                    split_path_token(&word);
                                                (
                                                    list_matches(&dir2, &base2),
                                                    dir2,
                                                )
                                            };
                                            path_popup = if matches.is_empty() {
                                                None
                                            } else {
                                                Some(PathPopup {
                                                    entries: matches,
                                                    selected: 0,
                                                    token_start: tok_start,
                                                    prefix: new_prefix,
                                                    mentions,
                                                })
                                            };
                                        } else {
                                            path_popup = None;
                                        }
                                    }
                                }
                            }
                            PathKey::Backspace => {
                                path_popup = None;
                                input.backspace();
                                slash_popup = update_suggestions(&input.text);
                            }
                            PathKey::Char(c) => {
                                path_popup = None;
                                input.insert(c);
                                slash_popup = update_suggestions(&input.text);
                            }
                            PathKey::Close => path_popup = None,
                            PathKey::Fallthrough => path_popup = None,
                        }
                        if !matches!(intent, PathKey::Fallthrough) {
                            continue;
                        }
                    }
                    match (key.code, key.modifiers) {
                        (KeyCode::Char('l'), KeyModifiers::CONTROL) => {
                            // viewport wipe; transcript state untouched
                            let _ = terminal.clear();
                            continue;
                        }
                        (KeyCode::Char('t'), KeyModifiers::CONTROL) => {
                            // skills & inventory popup (button on the strip)
                            modal = Some(Modal::Skills {
                                rows: inventory_rows(&sidebar, 64),
                            });
                            continue;
                        }
                        (KeyCode::Char('o'), KeyModifiers::CONTROL) => {
                            // expand/collapse the last tool call in place
                            match last_tool_ref {
                                Some((e, c)) => transcript.toggle_tool_call(e, c),
                                None => transcript
                                    .push_separated(Line::Info("· no tool output yet".into())),
                            }
                            continue;
                        }
                        (KeyCode::Char('o'), KeyModifiers::ALT) => {
                            // todos popup (button on the strip) — works
                            // mid-turn too, like Ctrl+T
                            modal = Some(Modal::Todos {
                                rows: todos_rows(&sidebar, 64),
                            });
                            continue;
                        }
                        (KeyCode::Char('i'), KeyModifiers::ALT) => {
                            // session info popup (button on the strip)
                            modal = Some(Modal::Info {
                                rows: info_rows(&sidebar, &meters, 64),
                            });
                            continue;
                        }
                        (KeyCode::Char('t'), KeyModifiers::ALT) => {
                            // fold/unfold thinking blocks in the transcript
                            transcript.set_thoughts_open(!transcript.thoughts_open);
                            continue;
                        }
                        (KeyCode::Char('v'), KeyModifiers::CONTROL) => {
                            // ctrl+v reads the system clipboard into
                            // the input; terminals that intercept
                            // ctrl+v deliver it as bracketed paste
                            // instead
                            if modal.is_none() && pending.is_none() {
                                let text = clipboard_text().await;
                                input.insert_str(&text);
                            }
                        }
                        // Reverse history search: while active it captures
                        // editing keys, so its arms sit ahead of the
                        // standard Esc/Enter/Backspace/char handling.
                        (KeyCode::Char('m'), KeyModifiers::CONTROL)
                            if !busy && slash_popup.is_none() && path_popup.is_none() =>
                        {
                            // Ctrl+M: the runtime mouse toggle — capture
                            // (dashboard: sidebar + click zones + ⇧drag)
                            // ⇄ native (text mode: full-width chat, plain
                            // drag selects, wheel via alternate-scroll).
                            // kitty-protocol terminals deliver Ctrl+M as a
                            // distinct key; plain terminals read it as
                            // Enter, so there `[tui] mouse = "capture"`
                            // is the way back. The choice persists.
                            mouse_captured = !mouse_captured;
                            apply_mouse_mode(mouse_captured);
                            let _ = commands
                                .send(Command::SaveSettings {
                                    model: None,
                                    effort: None,
                                    mode: None,
                                    mouse: mouse_setting(mouse_captured),
                                })
                                .await;
                            pop_toast(&mut toast, mouse_mode_text(mouse_captured));
                        }
                        (KeyCode::Char('r'), KeyModifiers::CONTROL)
                            if input.searching()
                                || (!busy && slash_popup.is_none() && path_popup.is_none()) =>
                        {
                            if input.searching() {
                                input.search_next();
                            } else {
                                input.search_start();
                            }
                        }
                        (KeyCode::Esc, _) if input.searching() => input.search_cancel(),
                        (KeyCode::Enter, _) if input.searching() => input.search_accept(),
                        (KeyCode::Backspace, _) if input.searching() => input.search_backspace(),
                        (KeyCode::Tab, _) if input.searching() => {}
                        (KeyCode::Up, _) if input.searching() => {}
                        (KeyCode::Down, _) if input.searching() => {}
                        (KeyCode::Esc, _) if input.searching() => input.search_cancel(),
                        // a popup or path-completion overlay owns Esc:
                        // close it, never abort a running turn
                        (KeyCode::Esc, _)
                            if slash_popup.is_some() || path_popup.is_some() =>
                        {
                            slash_popup = None;
                            path_popup = None;
                            continue;
                        }
                        (KeyCode::Esc, _) if busy => {
                            let _ = commands.send(Command::Abort).await;
                        }
                        (KeyCode::Esc, _)
                            if !busy
                                && slash_popup.is_none()
                                && path_popup.is_none()
                                && pending.is_none()
                                && input.text.is_empty() =>
                        {
                            // double-Esc on an empty idle input opens the
                            // rewind menu (claude-code muscle memory)
                            let now = Instant::now();
                            let doubled = last_esc
                                .is_some_and(|t| now.duration_since(t) < Duration::from_millis(600));
                            if doubled {
                                last_esc = None;
                                let items: Vec<(usize, String)> = transcript
                                    .entries()
                                    .iter()
                                    .rev()
                                    .filter_map(|l| match l {
                                        Line::User(t) => Some(t.clone()),
                                        _ => None,
                                    })
                                    .take(20)
                                    .enumerate()
                                    .map(|(i, t)| (i + 1, t))
                                    .collect();
                                if !items.is_empty() {
                                    modal = Some(Modal::Rewind {
                                        items,
                                        selected: 0,
                                    });
                                }
                            } else {
                                last_esc = Some(now);
                            }
                        }
                        (KeyCode::Esc, _) if scroll.is_some() => scroll = None,
                        (KeyCode::Enter, KeyModifiers::SHIFT) => input.newline(),
                        (KeyCode::Char('j'), KeyModifiers::CONTROL) => input.newline(),
                        (KeyCode::Enter, _) => {
                            let text = input.take();
                            // a submitted line invalidates any completion popup
                            slash_popup = None;
                            path_popup = None;
                            if text.trim().is_empty() {
                                continue;
                            }
                            if text.trim() == "/find" || text.trim().starts_with("/find ") {
                                // transcript search: purely local, no engine
                                // roundtrip. Fresh query starts at the scroll
                                // anchor (tail viewport top when pinned); a
                                // bare /find resumes just past the last hit.
                                let fresh = text
                                    .trim()
                                    .strip_prefix("/find")
                                    .map(str::trim)
                                    .filter(|q| !q.is_empty());
                                let (q, from) = match fresh {
                                    Some(q) => {
                                        let total = transcript.total_rows();
                                        let anchor = scroll
                                            .unwrap_or_else(|| total.saturating_sub(view_rows.min(total)));
                                        (q.to_string(), anchor)
                                    }
                                    None => match &find_last {
                                        Some((q, row)) => (q.clone(), row + 1),
                                        None => {
                                            transcript.push_separated(Line::Info("no previous /find".into()));
                                            continue;
                                        }
                                    },
                                };
                                match transcript.find_from(from, &q) {
                                    Some((_, offset)) => {
                                        scroll = Some(offset);
                                        find_last = Some((q, offset));
                                    }
                                    None => {
                                        transcript.push_separated(Line::Info(format!("no matches for '{q}'")));
                                        // resume past the searched-from row so a
                                        // later /find re-scans only fresh rows
                                        find_last = Some((q, from));
                                    }
                                }
                                continue;
                            }
                            if text.trim() == "/retry" {
                                if busy {
                                    transcript.push_separated(Line::Warn(
                                        "⏳ turn running — esc to abort first".into(),
                                    ));
                                } else if let Some(p) = last_user.clone() {
                                    // retry = a fresh turn with the same prompt
                                    transcript.push_separated(Line::User(p.clone()));
                                    busy = true;
                                    let _ = commands.send(Command::Prompt { text: p, schema: None, images: Vec::new() }).await;
                                } else {
                                    transcript.push_separated(Line::Info("nothing to retry yet".into()));
                                }
                                continue;
                            }
                            if text.trim() == "/context" {
                                // mid-turn the engine cannot serve the
                                // breakdown (the turn owns the command
                                // channel) — gate it like /retry
                                if busy {
                                    transcript.push_separated(Line::Warn(
                                        "⏳ turn running — /context works when idle".into(),
                                    ));
                                } else {
                                    let _ = commands.send(Command::ContextBreakdown).await;
                                }
                                continue;
                            }
                            if text.trim() == "/copy" {
                                let last = transcript
                                    .entries()
                                    .iter()
                                    .rev()
                                    .find_map(|l| match l {
                                        Line::Assistant(s) => Some(s.clone()),
                                        _ => None,
                                    });
                                match last {
                                    None => transcript.push_separated(Line::Info(
                                        "no assistant reply to copy".into(),
                                    )),
                                    Some(s) => {
                                        // OSC52 into the clipboard; the ack is a toast
                                        let mut stdout = std::io::stdout().lock();
                                        let _ = stdout.write_all(b"\x1b]52;c;");
                                        let _ = stdout.write_all(b64encode(&s).as_bytes());
                                        let _ = stdout.write_all(b"\x07");
                                        let _ = stdout.flush();
                                        pop_toast(&mut toast, "✓ copied last reply");
                                    }
                                }
                                continue;
                            }
                            // /image: stage an attachment instead of a prompt
                            if text.starts_with("/image ") || text == "/image" {
                                match handle_image_command(&text, &mut pending_image) {
                                    Some(Ok(note)) => {
                                        pop_toast(&mut toast, note);
                                    }
                                    Some(Err(e)) => transcript.push_separated(Line::Info(e)),
                                    None => {}
                                }
                                input.text.clear();
                                input.cursor = 0;
                                continue;
                            }
                            // /clip: paste a clipboard image as the
                            // next prompt's attachment
                            if text.trim() == "/clip" {
                                input.text.clear();
                                input.cursor = 0;
                                let staged = match clip_image_bytes().await {
                                    Ok(bytes) => match image_part_from_bytes(&bytes) {
                                        Ok(part) => {
                                            let kb = bytes.len() / 1024;
                                            let note = format!(
                                                "[clip attachment · {} · {kb}KB]",
                                                part.media_type
                                            );
                                            pending_image = Some(part);
                                            Ok(note)
                                        }
                                        Err(e) => Err(e),
                                    },
                                    Err(e) => Err(e),
                                };
                                match staged {
                                    Ok(note) => pop_toast(&mut toast, note),
                                    Err(e) => transcript.push_separated(Line::Info(e)),
                                }
                                continue;
                            }
                            if let Some(cmd) = slash_command(&text) {
                                transcript.push_separated(Line::User(text));
                                if matches!(
                                    cmd.event,
                                    Some(Command::SetMode {
                                        mode: ka_protocol::Mode::Plan,
                                    })
                                ) {
                                    plan_started = Some(std::time::SystemTime::now());
                                }
                                if let Some(note) = cmd.note {
                                    // usage errors warn; other notes stay muted
                                    transcript.push_separated(if note.starts_with("usage:") {
                                        Line::Warn(note)
                                    } else {
                                        Line::Info(note)
                                    });
                                }
                                        if let Some(kind) = cmd.modal {
                                            match kind {
                                                ModalKind::Mode => {
                                                    // the picker borrows the input
                                                    // box, preselecting the mode
                                                    // shown in the footer
                                                    mode_picker = Some(ModePicker::for_mode(
                                                        mode_from_label(&meters.mode),
                                                    ));
                                                }
                                                ModalKind::Key => {
                // /key: open the prompt for the current model's provider
                let current = meters
                    .model
                    .trim_start_matches('+')
                    .split('@')
                    .next()
                    .unwrap_or("")
                    .to_string();
                let vendor = current.split('/').next().unwrap_or("").to_string();
                let info = models
                    .iter()
                    .find(|m| m.id == current)
                    .or_else(|| models.iter().find(|m| current.starts_with(&m.id)));
                let prompt = info.and_then(|m| {
                    (!m.key_env.is_empty()).then(|| KeyPrompt {
                        env_var: m.key_env.clone(),
                        provider: vendor.clone(),
                        doc_url: m.doc_url.clone(),
                        input: String::new(),
                        drill: None,
                        pending_model: None,
                    })
                });
                match prompt {
                    Some(p) => modal = Some(Modal::Key(p)),
                    None => transcript.push_separated(Line::Warn("no api key variable is known for this model".into())),
                }
                                                }
                                                _ => {
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
                                                current: (!meters.session.is_empty())
                                                    .then(|| meters.session.clone()),
                                            })
                                        }
                                        ModalKind::Help => Modal::Help,
                                        ModalKind::Model => Modal::Model(ModelPicker {
                                            models: models.clone(),
                                            vendor: None,
                                            configured_only: false,
                                            selected: 0,
                                            filter: String::new(),
                                        }),
                                        ModalKind::Provider => Modal::Provider(
                                            ProviderPicker::new(providers.clone(), &models),
                                        ),
                                        ModalKind::Settings => Modal::Settings(SettingsPanel {
                                            model: meters.model.clone(),
                                            mode: mode_from_label(&meters.mode),
                                            effort: None,
                                            selected: 0,
                                            edit: None,
                                            providers: providers.clone(),
                                            config_path: ka_config_path(),
                                        }),
                                        ModalKind::Spills => Modal::Spills {
                                            items: spills.clone(),
                                            selected: 0,
                                        },
                                        ModalKind::Prompts => Modal::Prompts {
                                            items: sidebar.inventory.prompts.clone(),
                                            selected: 0,
                                        },
                                        ModalKind::Memory => {
                                            let cwd = std::env::current_dir()
                                                .unwrap_or_else(|_| std::path::PathBuf::from("."));
                                            // safe mode: memory is not loaded into
                                            // the session — the viewer must not
                                            // pretend otherwise
                                            let rows = if ka_engine::conventions::bare_mode() {
                                                vec![
                                                    "(no memory files)".to_string(),
                                                    "safe mode: memory tiers are not loaded"
                                                        .to_string(),
                                                ]
                                            } else {
                                                memory_modal_rows(&cwd)
                                            };
                                            let inbox = if ka_engine::conventions::bare_mode() {
                                                Vec::new()
                                            } else {
                                                read_memory_inbox(&cwd)
                                            };
                                            Modal::Memory {
                                                rows,
                                                inbox,
                                                selected: 0,
                                            }
                                        }
                                        ModalKind::Usage => {
                                            let sessions = std::env::current_dir()
                                                .ok()
                                                .and_then(|cwd| ka_strand::list(&cwd).ok())
                                                .unwrap_or_default();
                                            Modal::Usage {
                                                rows: usage_rows(&meters, &sessions),
                                            }
                                        }
                                        ModalKind::Tree => {
                                            let cwd = std::env::current_dir()
                                                .unwrap_or_else(|_| std::path::PathBuf::from("."));
                                            let all = ka_strand::list(&cwd).unwrap_or_default();
                                            let current = if meters.session.is_empty() {
                                                None
                                            } else {
                                                Some(meters.session.clone())
                                            };
                                            let inner_w = 68usize.saturating_sub(4);
                                            let (items, targets) =
                                                tree_modal_rows(&all, current.as_deref(), inner_w);
                                            Modal::Tree {
                                                items,
                                                targets,
                                                selected: 0,
                                            }
                                        }
                                        // Mode borrows the input box (outer
                                        // arm) and Key the outer arm above:
                                        // neither ever becomes a modal
                                        ModalKind::Mode | ModalKind::Key => Modal::Help,
                                        // unreachable in practice: /tasks and
                                        // /debug send events, and the modal is
                                        // built from the event payload — these
                                        // arms only satisfy exhaustiveness
                                        ModalKind::Tasks => Modal::Tasks {
                                            entries: Vec::new(),
                                            selected: 0,
                                        },
                                        ModalKind::Debug => Modal::Debug {
                                            rows: Vec::new(),
                                            scroll: None,
                                        },
                                    });
                            }
                                            }
                        }
                                // /tasks and /debug flag the roster event to
                                // open its modal instead of transcript rows
                                if modal.is_none() {
                                    match cmd.event {
                                        Some(Command::ListTasks) => {
                                            pending_modal = Some(PendingModal::Tasks)
                                        }
                                        Some(Command::DebugRoster) => {
                                            pending_modal = Some(PendingModal::Debug)
                                        }
                                        _ => {}
                                    }
                                }
                                if let Some(evt) = cmd.event {
                                    let mut is_switch = false;
                                    if let Command::SwitchStrand { id } = &evt {
                                        is_switch = true;
                                        fresh = id == "new";
                                    }
                                    let _ = commands.send(evt).await;
                                    if is_switch {
                                        busy = true;
                                    }
                                }
                                if let Some(follow) = cmd.followup {
                                    transcript.push_separated(Line::Info("(mode set; starting)".into()));
                                    busy = true;
                                    let _ = commands
                                        .send(Command::Prompt { text: follow, schema: None, images: Vec::new() })
                                        .await;
                                }
                                if cmd.quit {
                                    exit = Some(Exit::Quit);
                                }
                                continue;
                            }
                            // '+'-prefix while busy queues the draft
                            // locally; the head auto-sends when the turn
                            // settles (TurnFinished arm in the event loop)
                            if busy {
                                if let Some(deferred) = text.strip_prefix('+') {
                                    let item = deferred.trim().to_string();
                                    if !item.is_empty() {
                                        queue.push(item);
                                        transcript.push_separated(Line::Info(format!(
                                            "⏳ queued · {} waiting for this turn to end",
                                            queue.len()
                                        )));
                                    }
                                    continue;
                                }
                            }
                            // `!` passthrough: run a shell command directly
                            // (no turn, no gate — the user typed it); output
                            // lands in the transcript and rides the next
                            // prompt as context
                            if let Some(shell_cmd) = text
                                .strip_prefix('!')
                                .map(str::trim)
                                .filter(|c| !c.is_empty())
                            {
                                let command = shell_cmd.to_string();
                                transcript.push_separated(Line::Info(format!("» {command}")));
                                let _ = commands.send(Command::Shell { command }).await;
                                continue;
                            }
                            transcript.push_separated(Line::User(text.clone()));
                            let cmd = if busy {
                                transcript.push_separated(Line::Info(
                                    "⚡ steering this turn".into(),
                                ));
                                Command::Interject { text }
                            } else {
                                // plain new turn: the /retry target; a
                                // staged /image attachment rides along
                                last_user = Some(text.clone());
                                if let Some(img) = &pending_image {
                                    let kb = img.data.len() * 3 / 4 / 1024;
                                    transcript.push_separated(Line::Info(format!(
                                        "[img attachment · {} · {kb}KB]",
                                        img.media_type
                                    )));
                                }
                                Command::Prompt {
                                    text,
                                    schema: None,
                                    images: pending_image.take().into_iter().collect(),
                                }
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
                            } else if path_popup.is_none() {
                                // @-mention completion on an @-led token
                                // (longer than the bare sigil)
                                if let Some((start, token)) =
                                    path_token(&input.text, input.cursor)
                                        .filter(|(_, t)| mention_token(t).is_some())
                                {
                                    let query = mention_token(&token).unwrap_or_default();
                                    let matches = mention_matches(
                                        &walk_files(&std::env::current_dir().unwrap_or_default(), WALK_CAP),
                                        query,
                                    );
                                    if !matches.is_empty() {
                                        path_popup = Some(PathPopup {
                                            entries: matches,
                                            selected: 0,
                                            token_start: start,
                                            prefix: "@".to_string(),
                                            mentions: true,
                                        });
                                    }
                                } else {
                                    // file-path completion on a path-like token
                                    // (slash completion keeps priority for
                                    // `/`-led single-token input)
                                    let attempt = path_token(&input.text, input.cursor).and_then(
                                        |(start, token)| {
                                            let pathish = !token.is_empty()
                                                && (token.starts_with('.')
                                                    || token.starts_with('~')
                                                    || token.starts_with('/')
                                                    || token.contains('/'))
                                                && token != "."
                                                && token != "..";
                                            if !pathish {
                                                return None;
                                            }
                                            let (dir_part, base, _is_abs) =
                                                split_path_token(&token);
                                            // bare "~" lists $HOME, so `~` Tab
                                            // completes to `~/notes.txt`
                                            let (dir_part, base) =
                                                if dir_part.is_empty() && token == "~" {
                                                    ("~/".to_string(), String::new())
                                                } else {
                                                    (dir_part, base)
                                                };
                                            Some((start, token, dir_part, base))
                                        },
                                    );
                                    if let Some((start, token, dir_part, base)) = attempt {
                                        let matches = list_matches(&dir_part, &base);
                                        if matches.len() == 1 && !matches[0].1 {
                                            // one file: complete inline, silently
                                            let insert = format!("{dir_part}{}", matches[0].0);
                                            let (text, cursor) = complete_token(
                                                &input.text,
                                                start,
                                                token.chars().count(),
                                                &insert,
                                            );
                                            input.text = text;
                                            input.cursor = cursor;
                                        } else if !matches.is_empty() {
                                            // a directory (or many): offer the popup
                                            path_popup = Some(PathPopup {
                                                entries: matches,
                                                selected: 0,
                                                token_start: start,
                                                prefix: dir_part,
                                                mentions: false,
                                            });
                                        }
                                    }
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
                        // Ctrl+↑/↓: jump between user messages — the
                        // keyboard twin of the ▲▼ arrows, works in both
                        // mouse modes
                        (KeyCode::Up, KeyModifiers::CONTROL) => {
                            let rows = transcript.user_entry_rows();
                            let total = transcript.total_rows();
                            jump_to_user_message(&mut scroll, &rows, total, view_rows, true);
                        }
                        (KeyCode::Down, KeyModifiers::CONTROL) => {
                            let rows = transcript.user_entry_rows();
                            let total = transcript.total_rows();
                            jump_to_user_message(&mut scroll, &rows, total, view_rows, false);
                        }
                        // arrows are mode-dependent: captured (default)
                        // = prompt history; native = scroll the chat (the
                        // wheel arrives as ↑/↓ there via alternate-scroll)
                        (KeyCode::Up, _) if !busy && !mouse_captured => {
                            line_up(
                                &mut scroll,
                                transcript.total_rows(),
                                view_rows,
                            );
                        }
                        (KeyCode::Down, _) if !busy && !mouse_captured => {
                            line_down(
                                &mut scroll,
                                transcript.total_rows(),
                                view_rows,
                            );
                        }
                        (KeyCode::Up, _) if !busy => input.history_prev(),
                        (KeyCode::Down, _)
                            if !busy && slash_popup.is_none() && input.text.contains('\n') =>
                        {
                            input.move_down();
                        }
                        (KeyCode::Down, _) if !busy => input.history_next(),
                        // Ctrl+P / Ctrl+N: prompt history in every mode
                        // (readline muscle memory; the only history keys
                        // in native mode)
                        (KeyCode::Char('p'), KeyModifiers::CONTROL)
                            if !busy && slash_popup.is_none() && path_popup.is_none() =>
                        {
                            input.history_prev();
                        }
                        (KeyCode::Char('n'), KeyModifiers::CONTROL)
                            if !busy && slash_popup.is_none() && path_popup.is_none() =>
                        {
                            input.history_next();
                        }
                        // Line-editing keys. Alt combos rely on the kitty
                        // keyboard protocol; terminals that ignore it deliver
                        // Esc-then-key, which at idle degrades to a harmless
                        // no-op or a typed char — busy-turn Esc abort is
                        // unchanged.
                        (KeyCode::Char('a'), KeyModifiers::CONTROL) => input.line_home(),
                        (KeyCode::Char('e'), KeyModifiers::CONTROL) => input.line_end(),
                        (KeyCode::Char('b'), KeyModifiers::CONTROL) => input.left(),
                        (KeyCode::Char('f'), KeyModifiers::CONTROL) => input.right(),
                        (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
                            input.delete_forward();
                            slash_popup = update_suggestions(&input.text);
                        }
                        (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
                            let del = input.delete_to_line_start();
                            input.kill_push(del);
                            slash_popup = update_suggestions(&input.text);
                        }
                        (KeyCode::Char('k'), KeyModifiers::CONTROL) => {
                            let del = input.delete_to_line_end();
                            input.kill_push(del);
                            slash_popup = update_suggestions(&input.text);
                        }
                        (KeyCode::Char('w'), KeyModifiers::CONTROL) => {
                            let del = input.delete_word_backward();
                            input.kill_push(del);
                            slash_popup = update_suggestions(&input.text);
                        }
                        (KeyCode::Backspace, m)
                            if m.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                        {
                            let del = input.delete_word_backward();
                            input.kill_push(del);
                            slash_popup = update_suggestions(&input.text);
                        }
                        (KeyCode::Char('y'), KeyModifiers::CONTROL) => {
                            input.yank();
                            slash_popup = update_suggestions(&input.text);
                        }
                        (KeyCode::Char('q'), KeyModifiers::CONTROL) => {
                            // recall: pop the LAST queued item back into
                            // the draft to edit and re-defer
                            if let Some(item) = queue.pop() {
                                input.text = item;
                                input.cursor = input.text.chars().count();
                                slash_popup = update_suggestions(&input.text);
                            }
                        }
                        // some terminals deliver ctrl+_ / ctrl+- as chars
                        (KeyCode::Char('z'), KeyModifiers::CONTROL)
                        | (KeyCode::Char('_'), KeyModifiers::CONTROL)
                        | (KeyCode::Char('-'), KeyModifiers::CONTROL) => {
                            input.undo();
                            slash_popup = update_suggestions(&input.text);
                        }
                        (KeyCode::Left, m)
                            if m.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                        {
                            input.word_left();
                        }
                        (KeyCode::Right, m)
                            if m.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                        {
                            input.word_right();
                        }
                        (KeyCode::Char('b'), KeyModifiers::ALT) => input.word_left(),
                        (KeyCode::Char('f'), KeyModifiers::ALT) => input.word_right(),
                        (KeyCode::Char('d'), KeyModifiers::ALT) => {
                            let del = input.delete_word_forward();
                            input.kill_push(del);
                            slash_popup = update_suggestions(&input.text);
                        }
                        (KeyCode::Delete, KeyModifiers::ALT) => {
                            let del = input.delete_word_forward();
                            input.kill_push(del);
                            slash_popup = update_suggestions(&input.text);
                        }
                        (KeyCode::Char('e'), KeyModifiers::ALT) if !busy => {
                            // draft → temp file → $EDITOR → read back
                            let stamp = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_millis())
                                .unwrap_or(0);
                            let path = std::env::temp_dir().join(format!(
                                "ka-edit-{}-{stamp}.txt",
                                std::process::id()
                            ));
                            let path_str = path.to_string_lossy().into_owned();
                            match std::fs::write(&path, &input.text) {
                                Err(e) => {
                                    transcript.push_separated(Line::Info(format!("edit failed: {e}")));
                                }
                                Ok(()) => {
                                    let editor = std::env::var("EDITOR")
                                        .ok()
                                        .filter(|e| !e.is_empty())
                                        .unwrap_or_else(|| "vi".to_string());
                                    match run_external(
                                        &editor,
                                        &[path_str.as_str()],
                                        terminal,
                                        mouse_captured,
                                    ) {
                                        Err(e) => transcript.push_separated(Line::Info(format!("editor failed: {e}"))),
                                        Ok(()) => {
                                            let read = std::fs::read_to_string(&path);
                                            let _ = std::fs::remove_file(&path);
                                            match read {
                                                Ok(edited) => {
                                                    input.text.clear();
                                                    input.cursor = 0;
                                                    input.insert_str(&edited);
                                                    slash_popup =
                                                        update_suggestions(&input.text);
                                                }
                                                Err(e) => transcript.push_separated(Line::Info(format!(
                                                    "edit failed: {e}"
                                                ))),
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        (KeyCode::Left, _) => input.left(),
                        (KeyCode::Right, _) => input.right(),
                        (KeyCode::Home, _) => input.home(),
                        (KeyCode::End, _) => input.end(),
                        (KeyCode::Backspace, _) => {
                            input.backspace();
                            slash_popup = update_suggestions(&input.text);
                        }
                        (KeyCode::Delete, KeyModifiers::NONE) => {
                            input.delete_forward();
                            slash_popup = update_suggestions(&input.text);
                        }
                        (KeyCode::Char(c), _) => {
                            input.insert(c);
                            slash_popup = update_suggestions(&input.text);
                        }
                        _ => {}
                    }
                } else if let Some(Ok(TermEvent::Paste(text))) = maybe_term {
                    // paste routes through the same overlay precedence as
                    // keystrokes; single-line fields strip whitespace (API
                    // keys and filters never contain it, clipboards often do)
                    if pending.is_some() {
                        // option dialogs have nothing to paste into
                    } else if let Some(open) = modal.as_mut() {
                        match open {
                            Modal::Key(prompt) => {
                                prompt.input.extend(text.chars().filter(|c| !c.is_whitespace()));
                            }
                            Modal::Prompts { .. } => {}
                            Modal::Memory { .. } => {}
                            Modal::Rewind { .. } => {}
                            Modal::Tree { .. } => {}
                            Modal::Session(picker) => {
                                picker.filter.extend(text.chars().filter(|c| !c.is_whitespace()));
                            }
                            Modal::Model(picker) => {
                                picker.filter.extend(text.chars().filter(|c| !c.is_whitespace()));
                                picker.selected = 0;
                            }
                            Modal::Provider(picker) => {
                                picker.filter.extend(text.chars().filter(|c| !c.is_whitespace()));
                                picker.selected = 0;
                            }
                            Modal::Settings(panel) => {
                                if let Some(edit) = panel.edit.as_mut() {
                                    edit.extend(text.chars().filter(|c| !c.is_whitespace()));
                                }
                            }
                            Modal::Help
                            | Modal::Spills { .. }
                            | Modal::Usage { .. }
                            | Modal::Context { .. }
                            | Modal::Tasks { .. }
                            | Modal::TaskDetail { .. }
                            | Modal::Debug { .. }
                            | Modal::Todos { .. }
                            | Modal::Skills { .. }
                            | Modal::Info { .. } => {}
                        }
                    } else if path_popup.is_some() {
                        path_popup = None;
                        input.insert_str(&text);
                        slash_popup = update_suggestions(&input.text);
                    } else if !input.searching() && text.split_whitespace().count() == 1 {
                        // bracketed paste carrying exactly one existing
                        // image path: attach it instead of inserting text
                        let trimmed = text.trim();
                        if !trimmed.is_empty() {
                            let as_path = std::path::PathBuf::from(trimmed);
                            if as_path.is_file() && sniff_image_type(&as_path).is_some() {
                                match image_part_from_path(&as_path) {
                                    Ok(part) => {
                                        let kb = part.data.len() * 3 / 4 / 1024;
                                        let name = as_path
                                            .file_name()
                                            .map(|n| n.to_string_lossy().into_owned())
                                            .unwrap_or_default();
                                        pending_image = Some(part);
                                        pop_toast(
                                            &mut toast,
                                            format!("[img {name} · {kb}KB] — press enter to send"),
                                        );
                                        slash_popup = None;
                                        continue;
                                    }
                                    Err(e) => {
                                        transcript.push_separated(Line::Info(e));
                                        slash_popup = None;
                                        continue;
                                    }
                                }
                            }
                        }
                        input.insert_str(&text);
                        slash_popup = update_suggestions(&input.text);
                    } else if input.searching() {
                        for c in text.chars().filter(|c| !c.is_whitespace()) {
                            input.search_push(c);
                        }
                    } else {
                        input.insert_str(&text);
                        slash_popup = update_suggestions(&input.text);
                    }
                } else if let Some(Ok(TermEvent::Mouse(mouse_evt))) = maybe_term {
                    // captured-mouse interactions: the wheel scrolls the
                    // chat at line granularity (page keys keep their
                    // page step); overlays keep focus. In native mode no
                    // mouse events arrive at all — drag/paste are the
                    // terminal's own.
                    if modal.is_none()
                        && pending.is_none()
                        && slash_popup.is_none()
                        && path_popup.is_none()
                        && mouse_captured
                    {
                        match mouse_evt.kind {
                            // a click on the skills header toggles the
                            // section; checked before the wheel arms so a
                            // click never also scrolls
                            crossterm::event::MouseEventKind::Down(
                                crossterm::event::MouseButton::Left,
                            ) => {
                                // ▲▼ jump arrows on the transcript title
                                // row: step between user messages
                                // click on a thinking block toggles that
                                // block (collapsed by default, like the
                                // sidebar's collapsible sections). The
                                // content area starts below the title row,
                                // so this never eats the ▲▼ arrow clicks.
                                if let Some((area, start)) = tx_content.get()
                                    && area.contains(ratatui::layout::Position {
                                        x: mouse_evt.column,
                                        y: mouse_evt.row,
                                    })
                                {
                                    let row = start + (mouse_evt.row - area.y) as usize;
                                    match transcript.row_ref_at(row) {
                                        // a tool row toggles its inline
                                        // expansion
                                        Some(RowRef::ToolCall(e, c)) => {
                                            transcript.toggle_tool_call(e, c);
                                        }
                                        // a thinking block toggles
                                        Some(RowRef::Entry(entry)) => {
                                            transcript.toggle_thought(entry);
                                        }
                                        None => {}
                                    }
                                }
                                if let Some(up) = title_arrows
                                    .get()
                                    .and_then(|z| z.hit(mouse_evt.column, mouse_evt.row))
                                {
                                    let rows = transcript.user_entry_rows();
                                    let total = transcript.total_rows();
                                    jump_to_user_message(
                                        &mut scroll, &rows, total, view_rows, up,
                                    );
                                } else if let Some(button) =
                                    strip_zone.get().and_then(|z| z.hit(mouse_evt.column, mouse_evt.row))
                                {
                                    // a strip button: open its popup
                                    modal = Some(match button {
                                        StripButton::Todos => Modal::Todos {
                                            rows: todos_rows(&sidebar, 64),
                                        },
                                        StripButton::Skills => Modal::Skills {
                                            rows: inventory_rows(&sidebar, 64),
                                        },
                                        StripButton::Info => Modal::Info {
                                            rows: info_rows(&sidebar, &meters, 64),
                                        },
                                    });
                                }
                            }
                            // right click pastes the clipboard into the
                            // input
                            crossterm::event::MouseEventKind::Down(
                                crossterm::event::MouseButton::Right,
                            ) => {
                                let text = clipboard_text().await;
                                input.insert_str(&text);
                            }
                            crossterm::event::MouseEventKind::ScrollUp => {
                                line_up(&mut scroll, transcript.total_rows(), view_rows);
                            }
                            crossterm::event::MouseEventKind::ScrollDown => {
                                line_down(&mut scroll, transcript.total_rows(), view_rows);
                            }
                            _ => {}
                        }
                    }
                }
            }
            maybe_evt = events.recv() => {
                match maybe_evt {
                    None => { exit = Some(Exit::EngineEnded); }
                    Some(evt) => {
                        let replayed = matches!(evt, Event::Replay { .. });
                        if let Event::Replay { messages } = &evt {
                            // ↑/↓ recall the resumed session's earlier
                            // prompts: seed the draft history with them
                            input.seed_history(
                                messages
                                    .iter()
                                    .filter(|m| m.role == "user" && !m.digest)
                                    .map(|m| m.content.clone()),
                            );
                        }
                        // /tasks and /debug modals: a flagged roster event
                        // reroutes into the modal (skipping the transcript
                        // fallback below); TaskDetail always pages
                        if pending_modal.is_some() {
                            match &evt {
                            // mode/model acks are transient, not transcript rows
                            Event::ModeChanged { mode } => {
                                pop_toast(&mut toast, format!("mode · {}", mode_label(*mode)));
                            }
                            Event::ModelChanged { selector } => {
                                pop_toast(&mut toast, format!("model · {selector}"));
                            }
                                Event::Tasks { rows }
                                    if pending_modal == Some(PendingModal::Tasks) =>
                                {
                                    pending_modal = None;
                                    modal = Some(Modal::Tasks {
                                        entries: rows
                                            .iter()
                                            .map(|r| (task_id_of_row(r), r.clone()))
                                            .collect(),
                                        selected: 0,
                                    });
                                    continue;
                                }
                                Event::DebugRoster { rows }
                                    if pending_modal == Some(PendingModal::Debug) =>
                                {
                                    pending_modal = None;
                                    modal = Some(Modal::Debug {
                                        rows: rows.clone(),
                                        scroll: None,
                                    });
                                    continue;
                                }
                                _ => {}
                            }
                        }
                        match &evt {
                            Event::TaskDetail { id, text } => {
                                modal = Some(Modal::TaskDetail {
                                    id: *id,
                                    text: text.clone(),
                                    scroll: None,
                                });
                            }
                            Event::DebugRoster { rows } => {
                                // no pending modal: transcript fallback
                                transcript
                                    .push_separated(Line::Info("▸ debug sessions".into()));
                                for row in rows {
                                    transcript
                                        .push_separated(Line::Report(row.clone()));
                                }
                            }
                            _ => {}
                        }
                        if apply_event(
                            &evt,
                            &mut transcript,
                            &mut busy,
                            &mut busy_since,
                            &mut meters,
                            &mut pending,
                            &mut turn_produced,
                            &mut turn_usage,
                            &mut current_assistant,
                            &mut current_thought,
                            &mut current_tool,
                            &mut live_tool,
                            &mut last_user,
                            &mut last_error,
                            &mut spills,
                            &mut sidebar,
                        )
                        .is_some()
                        {
                            // Ctrl+O and click-to-expand target this call
                            last_tool_ref = transcript.last_tool_ref();
                        }
                        // a live title lands while the session picker is
                        // open: patch its row too (the picker otherwise
                        // reloads from the strand on every open, and the
                        // Title record is already persisted by then)
                        if let Event::Title { title } = &evt {
                            if !title.is_empty() {
                                if let Some(Modal::Session(picker)) = modal.as_mut() {
                                    if let Some(s) = picker
                                        .sessions
                                        .iter_mut()
                                        .find(|s| s.id == meters.session)
                                    {
                                        s.title = title.clone();
                                    }
                                }
                            }
                        }
                        if matches!(evt, Event::TurnFinished { .. }) {
                            turn_ended = true;
                            // one queued item per settle: the head becomes
                            // the next turn. Non-turn Idles (SetMode, …) are
                            // separate events and never reach this arm; an
                            // unanswered ask holds the queue until answered.
                            if pending.is_none() {
                                if let Some(next) = pop_queue_head(&mut queue) {
                                    transcript.push_separated(Line::User(next.clone()));
                                    last_user = Some(next.clone());
                                    busy = true;
                                    let _ = commands.send(Command::Prompt { text: next, schema: None, images: Vec::new() }).await;
                                }
                            }
                        }
                        if matches!(evt, Event::TurnFinished { .. })
                            && meters.mode == mode_label(ka_protocol::Mode::Plan)
                            && plan_drafted(plan_started, &plan_file_path())
                        {
                            plan_started = None;
                            transcript.push_separated(Line::Info(format!(
                                "Plan drafted — review {}, then /approve to build",
                                plan_file_path().display()
                            )));
                        }
                        if let Event::ContextBreakdown { parts, window } = &evt {
                            modal = Some(Modal::Context {
                                rows: context_rows(parts, *window),
                            });
                        }
                        if replayed {
                            scroll = None;
                        }
                    }
                }
            }
            // redraws while busy (spinner/clock) or while a toast is
            // alive; the arm also prunes the expired toast so ticking
            // actually stops once it fades
            _ = spin.tick(), if busy || toast.is_some() => {
                if toast.as_ref().is_some_and(|(_, at)| at.elapsed() >= TOAST_TTL) {
                    toast = None;
                }
            }
        }
    }
    let exit = exit.unwrap_or(Exit::Quit);
    let resume = resume_hint(
        &meters.session,
        meters.turns,
        busy,
        !transcript.entries().is_empty(),
    );
    Ok((exit, resume))
}

/// The after-exit resume hint: the command that brings this session
/// back. Worth printing whenever the session exists and something
/// happened in it — a finished turn, a turn still in flight when the
/// user quit, or history replayed from an earlier run: closing an
/// older chat without a new turn still deserves the way back.
fn resume_hint(
    session: &str,
    turns: u64,
    busy: bool,
    has_history: bool,
) -> Option<(String, String)> {
    if session.is_empty() || (turns == 0 && !busy && !has_history) {
        return None;
    }
    let tag = short_session(session).unwrap_or("?");
    let resume_cmd = format!("ka --session {tag}");
    let hint = format!("⟡ session saved — resume with: ka -c   (or: {resume_cmd})");
    Some((hint, resume_cmd))
}

/// Shape of one appended history line, sniffed from the file's last
/// non-empty line: zsh extended history (`: <start>:<elapsed>;cmd`)
/// gets the matching `:<ts>:0;<cmd>` shape, anything else (bash is
/// plain lines, empty file) appends the bare command.
fn history_line(existing_last: Option<&str>, cmd: &str, ts: u64) -> String {
    let zsh = existing_last.and_then(|l| {
        let rest = l.strip_prefix(": ")?;
        let (start, rest) = rest.split_once(':')?;
        let (elapsed, _) = rest.split_once(';')?;
        (!start.is_empty()
            && start.chars().all(|c| c.is_ascii_digit())
            && !elapsed.is_empty()
            && elapsed.chars().all(|c| c.is_ascii_digit()))
        .then_some(())
    });
    match zsh {
        Some(()) => format!(":{ts}:0;{cmd}"),
        None => cmd.to_string(),
    }
}

/// Append to an explicit history file (missing/unreadable files are
/// skipped — never created here).
fn append_history_to(path: &std::path::Path, cmd: &str, ts: u64) {
    let Ok(content) = std::fs::read_to_string(path) else {
        return;
    };
    let last = content.lines().rev().find(|l| !l.trim().is_empty());
    let line = history_line(last, cmd, ts);
    let mut out = content;
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&line);
    out.push('\n');
    let _ = std::fs::write(path, out);
}

/// Append the resume command to $HISTFILE. Skips silently when unset;
/// never fails the exit — history bookkeeping is best effort.
fn append_shell_history(cmd: &str) {
    // bash and zsh do NOT export HISTFILE to children — falling back to
    // the conventional files is what makes this work at all there
    let target = std::env::var("HISTFILE").ok().or_else(|| {
        let home = std::env::var("HOME").ok()?;
        let zsh = std::path::Path::new(&home).join(".zsh_history");
        if zsh.is_file() {
            return Some(zsh.to_string_lossy().into_owned());
        }
        let bash = std::path::Path::new(&home).join(".bash_history");
        bash.is_file().then(|| bash.to_string_lossy().into_owned())
    });
    if let Some(p) = target {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        append_history_to(std::path::Path::new(&p), cmd, ts);
    }
}

/// Live in-flight tool call: rolling output preview (live region only)
/// plus the last emission, which becomes the compact finish row's note.
#[derive(Debug, Clone)]
struct LiveTool {
    id: String,
    /// Most recent output lines, oldest first; capped in render.
    preview: Vec<String>,
    /// Last CallOutput excerpt and its error flag.
    last: Option<(String, bool)>,
    /// Local start time: CallStarted→CallFinished becomes the row's
    /// duration.
    started: Instant,
}

/// Tool call header: `→ {tool}`, or `→ {tool} · {detail}` when the
/// engine supplied an argument summary (the CallStarted detail field).
fn tool_header(tool: &str, detail: &str) -> String {
    if detail.is_empty() {
        format!("→ {tool}")
    } else {
        format!("→ {tool} · {detail}")
    }
}

/// How long a toast stays on screen before it self-destructs.
const TOAST_TTL: Duration = Duration::from_secs(4);

/// Set (or replace) the transient action toast.
fn pop_toast(toast: &mut Option<(String, Instant)>, msg: impl Into<String>) {
    *toast = Some((msg.into(), Instant::now()));
}

/// A finished call's note: the first non-blank output line, capped at
/// 40 columns (empty when the call produced nothing readable).
fn tool_note(excerpt: &str) -> String {
    let first = excerpt.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    trunc_cols(first, 40)
}

#[allow(clippy::too_many_arguments)]
fn apply_event(
    evt: &Event,
    transcript: &mut Transcript,
    busy: &mut bool,
    busy_since: &mut Option<Instant>,
    meters: &mut Meters,
    pending: &mut Option<PendingAsk>,
    turn_produced: &mut bool,
    turn_usage: &mut Option<(u64, u64, u64)>,
    current_assistant: &mut String,
    current_thought: &mut String,
    current_tool: &mut String,
    live_tool: &mut Option<LiveTool>,
    last_user: &mut Option<String>,
    last_error: &mut Option<String>,
    // spill-file paths seen this session (for /spills); deduped, capped
    spills: &mut Vec<String>,
    // sidebar state: inventory + live todo list
    sidebar: &mut SidebarState,
) -> Option<ToolCall> {
    // the most recently finished call, surfaced for Ctrl+O / expand
    let mut finished: Option<ToolCall> = None;
    match evt {
        Event::TurnStarted { .. } => {
            *busy = true;
            *busy_since = Some(Instant::now());
            *current_assistant = String::new();
            *current_thought = String::new();
            *current_tool = String::new();
            *live_tool = None;
            *turn_produced = false;
            *turn_usage = None;
            // a new turn invalidates a stale in-turn error buffer; the
            // retry target survives so /retry can resend the prompt
            // that produced the failure
            *last_error = None;
        }
        Event::Delta { kind } => match kind {
            ka_protocol::DeltaKind::Text(t) => {
                *turn_produced = true;
                current_assistant.push_str(t);
            }
            ka_protocol::DeltaKind::Thought(t) => {
                *turn_produced = true;
                current_thought.push_str(t);
            }
            ka_protocol::DeltaKind::Call { tool, id } => {
                *turn_produced = true;
                // chronology: text streamed before the call must render
                // above it, exactly like the turn-end flush
                flush_live_text(transcript, current_thought, current_assistant);
                *current_tool = tool_header(tool, "");
                *live_tool = Some(LiveTool {
                    id: id.clone(),
                    preview: Vec::new(),
                    last: None,
                    started: Instant::now(),
                });
            }
        },
        Event::CallStarted { tool, id, detail } => {
            *turn_produced = true;
            flush_live_text(transcript, current_thought, current_assistant);
            match live_tool.as_ref() {
                // the streaming header for this exact call is still the
                // live row: upgrade it in place with the argument detail
                Some(lt) if lt.id == *id && !current_tool.is_empty() => {
                    *current_tool = tool_header(tool, detail);
                }
                // cold start (no stream header) or a different call:
                // flush a never-finished header as a bare row, then
                // open the fresh live block
                _ => {
                    if !current_tool.is_empty() {
                        transcript.push_tool_call(ToolCall {
                            head: std::mem::take(current_tool),
                            ok: false,
                            note: String::new(),
                            excerpt: String::new(),
                            spill: None,
                            dur: None,
                            expanded: false,
                        });
                    }
                    *current_tool = tool_header(tool, detail);
                    *live_tool = Some(LiveTool {
                        id: id.clone(),
                        preview: Vec::new(),
                        last: None,
                        started: Instant::now(),
                    });
                }
            }
        }
        Event::CallOutput {
            id,
            excerpt,
            is_error,
            spill,
            ..
        } => {
            *turn_produced = true;
            if let Some(path) = spill {
                record_spill(spills, path);
            }
            match live_tool.as_mut() {
                // live block: refresh the rolling preview; the compact
                // note is built from `last` when the call finishes
                Some(lt) if lt.id == *id => {
                    observe_preview(&mut lt.preview, excerpt);
                    lt.last = Some((excerpt.clone(), *is_error));
                }
                // no matching block (replay paths, id drift): the note
                // is lost — the row still renders from its head
                _ => {}
            }
        }
        Event::CallFinished { ok, .. } => {
            // collapse the live block: the preview was transient; the
            // compact ` │ → tool … ✓` row is what gets cached
            if let Some(lt) = live_tool.take() {
                let (note, excerpt) = match lt.last {
                    Some((excerpt, _)) => (tool_note(&excerpt), excerpt),
                    None => (String::new(), String::new()),
                };
                let call = ToolCall {
                    head: std::mem::take(current_tool),
                    ok: *ok,
                    note,
                    excerpt,
                    spill: None,
                    dur: Some(lt.started.elapsed().as_secs_f64()),
                    expanded: false,
                };
                transcript.push_tool_call(call.clone());
                finished = Some(call);
            }
        }
        Event::Ask { id, questions } => {
            if let Some(q) = questions.first() {
                *pending = Some(PendingAsk {
                    id: id.clone(),
                    question: q.text.clone(),
                    options: q.options.clone(),
                    detail: q.detail.clone(),
                    selected: 0,
                });
            }
            fire_notifications("permission_ask", "ask");
        }
        Event::ShellOutput {
            command,
            output,
            note,
        } => {
            // `!` passthrough result: plain rows under the » command row
            let tail = note
                .as_deref()
                .map(|n| format!(" ({n})"))
                .unwrap_or_default();
            let body = if output.is_empty() {
                format!("(no output){tail}")
            } else {
                format!("{output}{tail}")
            };
            for line in body.lines().take(40) {
                transcript.push_separated(Line::Shell(line.to_string()));
            }
            let hidden = body.lines().count().saturating_sub(40);
            if hidden > 0 {
                transcript.push_separated(Line::Info(format!("… +{hidden} more")));
            }
            transcript.push_separated(Line::Info(format!(
                "» {command} — output joins your next prompt"
            )));
        }
        Event::Tasks { rows } => {
            // /tasks dashboard snapshot rendered as report rows
            transcript.push_separated(Line::Info("▸ background tasks".into()));
            for row in rows {
                transcript.push_separated(Line::Report(row.clone()));
            }
        }
        Event::TurnFinished { stop, usage } => {
            let elapsed = busy_since.map_or(0.0, |t| t.elapsed().as_secs_f64());
            flush_live_text(transcript, current_thought, current_assistant);
            // a call still running at turn end closes as a row too
            let mut finished_call = None;
            if let Some(lt) = live_tool.take() {
                let (note, excerpt) = match lt.last {
                    Some((excerpt, _)) => (tool_note(&excerpt), excerpt),
                    None => (String::new(), String::new()),
                };
                let call = ToolCall {
                    head: std::mem::take(current_tool),
                    ok: true,
                    note,
                    excerpt,
                    spill: None,
                    dur: Some(lt.started.elapsed().as_secs_f64()),
                    expanded: false,
                };
                transcript.push_tool_call(call.clone());
                finished_call = Some(call);
            } else {
                current_tool.clear();
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
            meters.turns += 1;
            meters.tokens_in += in_seen;
            meters.tokens_out += usage.output;
            meters.cache_read += usage.cache_read;
            meters.elapsed += elapsed;
            let tail = usage_tail(usage, elapsed);
            let silent = matches!(stop, ka_protocol::Stop::Done)
                && usage.input + usage.output + usage.cache_read == 0
                && usage.cost == 0.0;
            // silent = replayed turns (resume): no notification for those
            if !silent {
                let stop_label = match stop {
                    ka_protocol::Stop::Done => "done",
                    ka_protocol::Stop::Aborted => "aborted",
                    ka_protocol::Stop::Length => "length",
                    ka_protocol::Stop::Error => "error",
                };
                fire_notifications("turn_finished", stop_label);
            }
            if !silent {
                let (glyph, tone, text) = match stop {
                    ka_protocol::Stop::Done => ('✓', SummaryTone::Ok, format!("done{tail}")),
                    ka_protocol::Stop::Aborted => (
                        '◐',
                        SummaryTone::Warn,
                        format!("aborted · partial kept{tail}"),
                    ),
                    ka_protocol::Stop::Length => (
                        '◑',
                        SummaryTone::Warn,
                        format!("stopped at output limit{tail}"),
                    ),
                    ka_protocol::Stop::Error => {
                        let body = match last_error.take() {
                            Some(msg) => {
                                let msg: String = if msg.chars().count() > 90 {
                                    msg.chars().take(89).chain(std::iter::once('…')).collect()
                                } else {
                                    msg
                                };
                                format!("failed · {msg} · /retry{tail}")
                            }
                            None => format!("failed · /retry{tail}"),
                        };
                        ('✗', SummaryTone::Err, body)
                    }
                };
                transcript.push_separated(Line::Summary { glyph, tone, text });
                // best-effort desktop notification (OSC 9); one write
                let label = match stop {
                    ka_protocol::Stop::Done => format!("done · {}", fmt_dur(elapsed)),
                    ka_protocol::Stop::Aborted => "aborted".to_string(),
                    ka_protocol::Stop::Length => format!("stopped · {}", fmt_dur(elapsed)),
                    ka_protocol::Stop::Error => "failed".to_string(),
                };
                let mut out = std::io::stdout().lock();
                let _ = out.write_all(format!("\x1b]9;ka · {label}\x07").as_bytes());
                let _ = out.flush();
            }
            finished = finished_call;
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
            meters.mode = mode_label(*mode).to_string();
        }
        Event::Error { message, .. } => {
            if *busy {
                // in-turn error: buffer it; the TurnFinished report row
                // surfaces it once the turn settles
                *last_error = Some(message.clone());
            } else {
                transcript.push_separated(Line::Err(message.clone()));
            }
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
            *live_tool = None;
            *last_user = None;
            *last_error = None;
            for m in messages {
                if m.digest || m.role == "digest" {
                    // faint compaction divider: history before this point
                    // lives in the summary, not the transcript
                    transcript.push_separated(Line::Info("⋯ digest ⋯".into()));
                    continue;
                }
                if m.role == "user" {
                    transcript.push_separated(Line::User(m.content.clone()));
                    continue;
                }
                // assistant: dim thought first, then one railed row per
                // tool call (consecutive calls merge into one block),
                // then the text block when present
                if let Some(t) = m.thinking.clone().filter(|t| !t.trim().is_empty()) {
                    transcript.push_separated(Line::Thought(t));
                }
                for c in &m.calls {
                    transcript.push_tool_call(ToolCall {
                        head: tool_header(&c.tool, &c.detail),
                        ok: !c.is_error,
                        note: tool_note(c.result.as_deref().unwrap_or("")),
                        excerpt: c.result.clone().unwrap_or_default(),
                        spill: None,
                        dur: None,
                        expanded: false,
                    });
                }
                if !m.content.trim().is_empty() {
                    transcript.push_separated(Line::Assistant(m.content.clone()));
                }
            }
        }
        Event::Note { message } => transcript.push_separated(Line::Info(message.clone())),
        Event::Inventory {
            tools,
            mcp,
            agents,
            skills,
            prompts,
        } => {
            let card = inventory_card(mcp, agents);
            if !card.is_empty() {
                transcript.push_separated(Line::Info(card));
            }
            sidebar.inventory = Inventory {
                tools: tools.clone(),
                mcp: mcp.clone(),
                agents: agents.clone(),
                skills: skills.clone(),
                prompts: prompts.clone(),
            };
        }
        Event::Title { title } => {
            if !title.is_empty() {
                sidebar.title = Some(title.clone());
            }
        }
        Event::Todos { items } => sidebar.todos = items.clone(),
        // the engine sends Idle after every non-turn command; without
        // clearing here, the optimistic busy set on a session switch
        // leaks into the next prompt ("steering this turn" on turn one)
        Event::Idle => {
            *busy = false;
            *busy_since = None;
        }
        Event::DigestStarted => {
            transcript.push_separated(Line::Info("⋯ digesting context…".to_string()))
        }
        Event::DigestFinished { .. } => {}
        // the run loop opens the /context modal from this event
        Event::ContextBreakdown { .. } => {}
        // the run loop opens the /tasks pager and /debug overlay from
        // these events (transcript fallback included)
        Event::TaskDetail { .. } | Event::DebugRoster { .. } => {}
    }
    finished
}

/// Compact bootstrap inventory card for [`Event::Inventory`]: one muted
/// transcript entry covering only what the sidebar does not own — the
/// mcp (`name ✓ n` / `name ✗`) and agents segments, one detail row each
/// under ~90 columns via `(+N)` overflow marks. With both empty no card
/// line lands at all (the transcript stays clean; the sidebar owns
/// tools and skills).
fn inventory_card(mcp: &[ka_protocol::McpSummary], agents: &[String]) -> String {
    const CAP: usize = 90;
    let mut rows = Vec::new();
    if !mcp.is_empty() {
        let items: Vec<String> = mcp
            .iter()
            .map(|s| {
                if s.ok {
                    format!("{} ✓ {}", s.name, s.tools)
                } else {
                    format!("{} ✗", s.name)
                }
            })
            .collect();
        rows.push(pack_row("mcp", &items, CAP));
    }
    if !agents.is_empty() {
        rows.push(pack_row("agents", agents, CAP));
    }
    rows.join("\n")
}

/// Split an inventory prompt row (`server/name (a, b)`) into its
/// coordinates and argument names.
fn parse_prompt_row(row: &str) -> (Option<(String, String)>, Vec<String>) {
    let (spec, args) = match row.find(" (") {
        Some(p) => (&row[..p], Some(&row[p + 2..])),
        None => (row, None),
    };
    let coords = spec
        .split_once('/')
        .map(|(server, name)| (server.to_string(), name.trim_end_matches(')').to_string()));
    let arg_names = args
        .map(|a| {
            a.trim_end_matches(')')
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    (coords, arg_names)
}

/// Parse `/prompt server/name k=v k2=v2` into a CallPrompt triple.
fn parse_prompt_invocation(
    rest: &str,
) -> Option<(String, String, std::collections::HashMap<String, String>)> {
    let mut parts = rest.split_whitespace();
    let spec = parts.next()?;
    let (server, name) = spec.split_once('/')?;
    let mut args = std::collections::HashMap::new();
    for pair in parts {
        let (k, v) = pair.split_once('=')?;
        args.insert(k.to_string(), v.to_string());
    }
    Some((server.to_string(), name.to_string(), args))
}

/// `label: a · b · c`, dropping tail items for a `(+N)` mark once the
/// row would pass `cap` columns (the first item always shows).
fn pack_row(label: &str, items: &[String], cap: usize) -> String {
    let mut row = format!("{label}: ");
    let mut shown = 0usize;
    for item in items {
        let candidate = if shown == 0 {
            format!("{row}{item}")
        } else {
            format!("{row} · {item}")
        };
        let remaining = items.len() - shown - 1;
        // room for the worst-case " (+NN)" suffix
        let suffix = if remaining > 0 { 6 } else { 0 };
        if shown > 0 && candidate.chars().count() + suffix > cap {
            break;
        }
        row = candidate;
        shown += 1;
    }
    let omitted = items.len() - shown;
    if omitted > 0 {
        row.push_str(&format!(" (+{omitted})"));
    }
    row
}

fn record_spill(spills: &mut Vec<String>, path: &str) {
    if spills.contains(&path.to_string()) {
        return;
    }
    spills.push(path.to_string());
    if spills.len() > 50 {
        spills.remove(0);
    }
}

/// Suspend the TUI (flush, leave raw mode and the alternate screen), run
/// `program` with inherited stdio, then restore raw mode + alternate
/// screen and force a full redraw. The child's exit status is ignored —
/// editors and pagers exit non-zero routinely. The mouse mode is
/// restored to whatever the session runs in (capture or native+1007) —
/// an unconditional capture re-arm would desync the flag and kill the
/// native wheel.
fn run_external(
    program: &str,
    args: &[&str],
    terminal: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    mouse_captured: bool,
) -> std::io::Result<()> {
    use std::io::Write as _;
    let _ = std::io::stdout().flush();
    let _ = crossterm::terminal::disable_raw_mode();
    let _ = crossterm::execute!(
        std::io::stdout(),
        crossterm::terminal::LeaveAlternateScreen,
        crossterm::event::DisableMouseCapture
    );
    let _ = std::io::stdout().write_all(b"\x1b[?1007l");
    let _ = std::io::stdout().flush();
    let res = std::process::Command::new(program)
        .args(args)
        .status()
        .map(|_| ());
    let _ = crossterm::execute!(std::io::stdout(), crossterm::terminal::EnterAlternateScreen);
    let _ = crossterm::terminal::enable_raw_mode();
    apply_mouse_mode(mouse_captured);
    let _ = terminal.clear();
    res
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
/// Detect image bytes by magic prefix (png/jpeg/gif/webp).
fn sniff_image_bytes(head: &[u8]) -> Option<&'static str> {
    if head.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        Some("image/png")
    } else if head.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if head.len() >= 12 && head.starts_with(b"RIFF") && &head[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// Detect an image file by magic bytes (png/jpeg/gif/webp).
fn sniff_image_type(path: &std::path::Path) -> Option<&'static str> {
    use std::io::Read;
    let mut head = [0u8; 12];
    let n = std::fs::File::open(path).ok()?.read(&mut head).ok()?;
    sniff_image_bytes(&head[..n])
}

/// Build an attachment part from raw image bytes: magic-byte sniff,
/// 5 MB cap, base64 payload.
fn image_part_from_bytes(data: &[u8]) -> Result<ka_protocol::ImagePart, String> {
    const MAX_BYTES: usize = 5 * 1024 * 1024;
    if data.len() > MAX_BYTES {
        return Err(format!(
            "clipboard image is {:.1} MB, over the 5 MB cap",
            data.len() as f64 / (1024.0 * 1024.0)
        ));
    }
    let media_type = sniff_image_bytes(data)
        .ok_or_else(|| "unsupported image type (png/jpg/webp/gif only)".to_string())?;
    Ok(ka_protocol::ImagePart {
        data: b64encode(data),
        media_type: media_type.to_string(),
    })
}

/// Load an image file into an attachment part: size precheck, then the
/// shared bytes path.
fn image_part_from_path(path: &std::path::Path) -> Result<ka_protocol::ImagePart, String> {
    const MAX_MB: u64 = 5;
    let meta = std::fs::metadata(path).map_err(|e| format!("image {}: {e}", path.display()))?;
    if meta.len() > MAX_MB * 1024 * 1024 {
        return Err(format!(
            "image {} is {:.1} MB, over the {MAX_MB} MB cap",
            path.display(),
            meta.len() as f64 / (1024.0 * 1024.0)
        ));
    }
    let bytes = std::fs::read(path).map_err(|e| format!("image {}: {e}", path.display()))?;
    image_part_from_bytes(&bytes).map_err(|e| format!("image {}: {e}", path.display()))
}

/// Attach `/image <path>`: validate, stage, and confirm in the
/// transcript. Returns `None` when the text is not an /image command.
fn handle_image_command(
    text: &str,
    pending_image: &mut Option<ka_protocol::ImagePart>,
) -> Option<Result<String, String>> {
    let rest = text.strip_prefix("/image ")?;
    let path = std::path::PathBuf::from(rest.trim());
    match image_part_from_path(&path) {
        Ok(part) => {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let kb = part.data.len() * 3 / 4 / 1024;
            *pending_image = Some(part);
            Some(Ok(format!("[img {name} · {} · {kb}KB]", "attached")))
        }
        Err(e) => Some(Err(e)),
    }
}

/// True under WSL (Windows Subsystem for Linux).
fn is_wsl() -> bool {
    std::fs::read_to_string("/proc/version")
        .map(|v| v.to_lowercase().contains("microsoft"))
        .unwrap_or(false)
}

/// Translate a Windows path (`C:\a\b`) to its WSL mount (`/mnt/c/a/b`).
fn win_to_wsl(path: &str) -> Option<std::path::PathBuf> {
    let path = path.trim().trim_matches('"');
    let (drive, rest) = path.split_once(':')?;
    let mut chars = drive.chars();
    let letter = chars.next()?;
    if chars.next().is_some() || !letter.is_ascii_alphabetic() {
        return None;
    }
    Some(std::path::PathBuf::from(format!(
        "/mnt/{}{}",
        letter.to_ascii_lowercase(),
        rest.replace('\\', "/")
    )))
}

/// One clipboard probe under the shared deadline: stdout bytes on
/// success, Err when the tool is missing, fails, or the budget ran out.
async fn clip_probe(
    program: &str,
    args: &[&str],
    deadline: tokio::time::Instant,
) -> Result<Vec<u8>, ()> {
    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
    if remaining.is_zero() {
        return Err(());
    }
    let out = tokio::time::timeout(remaining, async {
        tokio::process::Command::new(program)
            .args(args)
            .output()
            .await
    })
    .await
    .map_err(|_| ())?
    .map_err(|_| ())?;
    if out.status.success() && !out.stdout.is_empty() {
        Ok(out.stdout)
    } else {
        Err(())
    }
}

/// Save the Windows clipboard image to %TEMP%\ka-clip.png via
/// PowerShell and read it back across the /mnt/c boundary.
async fn clip_via_powershell(deadline: tokio::time::Instant) -> Result<Vec<u8>, ()> {
    const SCRIPT: &str = "$ErrorActionPreference='stop'; \
Add-Type -AssemblyName System.Windows.Forms; \
$i=[System.Windows.Forms.Clipboard]::GetImage(); \
if ($i) { $p=Join-Path $env:TEMP 'ka-clip.png'; \
$i.Save($p,[System.Drawing.Imaging.ImageFormat]::Png); Write-Output $p }";
    let out = clip_probe(
        "powershell.exe",
        &["-NoProfile", "-Command", SCRIPT],
        deadline,
    )
    .await?;
    // the printed path is ASCII; lossy decoding never panics
    let line = String::from_utf8_lossy(&out);
    let win_path = line.lines().next().ok_or(())?.trim();
    let path = win_to_wsl(win_path).ok_or(())?;
    std::fs::read(path).map_err(|_| ())
}

/// Read image bytes off the clipboard: wl-paste → xclip → WSL2
/// PowerShell bridge, all under one 4 s overall budget.
async fn clip_image_bytes() -> Result<Vec<u8>, String> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(4);
    for (program, args) in [
        ("wl-paste", vec!["--type", "image/png"]),
        (
            "xclip",
            vec!["-selection", "clipboard", "-t", "image/png", "-o"],
        ),
    ] {
        if let Ok(bytes) = clip_probe(program, &args, deadline).await {
            return Ok(bytes);
        }
    }
    if is_wsl() {
        if let Ok(bytes) = clip_via_powershell(deadline).await {
            return Ok(bytes);
        }
    }
    Err("no image on clipboard (tried wl-paste, xclip, powershell)".to_string())
}

pub fn available_slash_commands() -> Vec<(String, String)> {
    let mut out = builtin_slash_commands();
    // custom files come after builtins, named like any slash command so
    // the popup prefix-filter and Tab-accept work the same way
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    // safe mode: custom commands are customizations — built-ins only
    let custom = if ka_engine::conventions::bare_mode() {
        Vec::new()
    } else {
        scan_custom_commands(&cwd)
    };
    for c in custom {
        let hint = if c.argument_hint.is_empty() {
            String::new()
        } else {
            format!(" ({})", c.argument_hint)
        };
        out.push((
            format!("/{}", c.name),
            format!("custom command{hint} — {desc}", desc = c.description),
        ));
    }
    out
}

fn builtin_slash_commands() -> Vec<(String, String)> {
    let mut cmds = vec![
        ("/model".to_string(), "pick a model".to_string()),
        (
            "/mcp".to_string(),
            "refresh MCP tool lists (/mcp refresh)".to_string(),
        ),
        (
            "/tree".to_string(),
            "current session's fork tree".to_string(),
        ),
        (
            "/memory".to_string(),
            "show loaded MEMORY.md tiers".to_string(),
        ),
        (
            "/tasks".to_string(),
            "background tasks, DAP sessions — ⏎ pages a task result".to_string(),
        ),
        (
            "/debug".to_string(),
            "live debug sessions (breakpoints + output)".to_string(),
        ),
        ("/prompt".to_string(), "run an MCP prompt".to_string()),
        (
            "/provider".to_string(),
            "connect a provider (api key)".to_string(),
        ),
        ("/mode".to_string(), "pick a permission mode".to_string()),
        (
            "/plan".to_string(),
            "research the task, draft the plan file".to_string(),
        ),
        ("/build".to_string(), "implement the plan file".to_string()),
        (
            "/review".to_string(),
            "read-only review of current changes ([base])".to_string(),
        ),
        (
            "/approve".to_string(),
            "review the plan file, then build it".to_string(),
        ),
        (
            "/rewind".to_string(),
            "drop the last N exchanges".to_string(),
        ),
        (
            "/fork".to_string(),
            "fork this session into a copy [turns to drop]".to_string(),
        ),
        (
            "/checkpoint".to_string(),
            "snapshot the working tree (git)".to_string(),
        ),
        (
            "/restore".to_string(),
            "restore a checkpoint [id | list]".to_string(),
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
        ("/retry".to_string(), "resend the last prompt".to_string()),
        (
            "/copy".to_string(),
            "copy the last reply (OSC52)".to_string(),
        ),
        ("/clip".to_string(), "attach a clipboard image".to_string()),
        ("/image".to_string(), "attach an image [path]".to_string()),
        ("/agents".to_string(), "list available agents".to_string()),
        (
            "/find".to_string(),
            "search the transcript: /find <text>, bare repeats".to_string(),
        ),
        (
            "/spills".to_string(),
            "browse spilled tool output in $PAGER".to_string(),
        ),
        (
            "/export".to_string(),
            "save this session to markdown [path]".to_string(),
        ),
        ("/help".to_string(), "commands and key bindings".to_string()),
        (
            "/settings".to_string(),
            "settings & provider status".to_string(),
        ),
        (
            "/usage".to_string(),
            "usage & cost: session + recent".to_string(),
        ),
        (
            "/context".to_string(),
            "context usage breakdown".to_string(),
        ),
        (
            "/key".to_string(),
            "set an api key for the current model".to_string(),
        ),
        ("/quit".to_string(), "exit".to_string()),
    ];
    if let Ok(cwd) = std::env::current_dir() {
        let home = std::env::var("HOME").ok();
        // project scope anchors at the root project, not the launch dir
        let root = ka_engine::project_root(&cwd);
        let mut roots = vec![
            root.join(".ka/commands"),
            root.join(".agents/commands"),
            root.join(".claude/commands"),
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

/// Path-completion popup state (opened by Tab on a path-like or
/// @-mention token).
#[derive(Debug, Clone)]
pub struct PathPopup {
    /// Matching `(name, is_dir)` entries for the current directory.
    pub entries: Vec<(String, bool)>,
    /// Selected entry index.
    pub selected: usize,
    /// Char index where the completed token starts in the input.
    pub token_start: usize,
    /// The typed directory part incl. trailing `/` (e.g. `./src/`) for
    /// path completion, or the bare `@` sigil for file mentions; an
    /// accepted entry is inserted as `prefix + name`.
    pub prefix: String,
    /// False: filesystem completion against `list_matches`. True:
    /// @-mention mode over the recursive `walk_files` listing, where a
    /// file accept inserts a trailing space.
    pub mentions: bool,
}

/// Directories the @-mention walk never descends into (plus every
/// dotfile directory).
const WALK_SKIP_DIRS: &[&str] = &[".git", "node_modules", "target", ".venv", "dist"];

/// Entry cap for the @-mention walk.
const WALK_CAP: usize = 500;

/// Recursive walk of `root` collecting `(relative_path, is_dir)` for
/// every non-dotfile, skipping [`WALK_SKIP_DIRS`] and dotfile
/// directories. Sorted for determinism; capped at `cap` entries.
pub fn walk_files(root: &std::path::Path, cap: usize) -> Vec<(String, bool)> {
    let mut out: Vec<(String, bool)> = Vec::new();
    walk_into(root, "", &mut out);
    out.sort();
    out.truncate(cap);
    out
}

fn walk_into(dir: &std::path::Path, prefix: &str, out: &mut Vec<(String, bool)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir {
            if WALK_SKIP_DIRS.contains(&name.as_str()) {
                continue;
            }
            let rel = format!("{prefix}{name}");
            out.push((rel.clone(), true));
            walk_into(&dir.join(&name), &format!("{rel}/"), out);
        } else {
            out.push((format!("{prefix}{name}"), false));
        }
    }
}

/// The @-mention query carried by a completion token: Some(text after
/// the `@`) when the token starts with `@` and carries more than the
/// bare sigil.
pub fn mention_token(token: &str) -> Option<&str> {
    token.strip_prefix('@').filter(|q| !q.is_empty())
}

/// @-mention match rule: the text after the last `/` in the query is a
/// prefix of the entry path's last segment (case-sensitive; ordering is
/// the walk's deterministic sort).
pub fn mention_matches(entries: &[(String, bool)], query: &str) -> Vec<(String, bool)> {
    let base = query.rsplit('/').next().unwrap_or("");
    entries
        .iter()
        .filter(|(rel, _)| {
            rel.trim_end_matches('/')
                .rsplit('/')
                .next()
                .is_some_and(|seg| seg.starts_with(base))
        })
        .cloned()
        .collect()
}

/// The whitespace-delimited word at the caret (char index). None when
/// the caret sits between/inside whitespace or the text is empty.
pub fn path_token(text: &str, cursor: usize) -> Option<(usize, String)> {
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    let cursor = cursor.min(len);
    let ws = |c: char| matches!(c, ' ' | '\n' | '\t');
    if cursor < len && ws(chars[cursor]) {
        return None;
    }
    if cursor == len && (len == 0 || ws(chars[len - 1])) {
        return None;
    }
    let mut start = cursor;
    while start > 0 && !ws(chars[start - 1]) {
        start -= 1;
    }
    let mut end = cursor;
    while end < len && !ws(chars[end]) {
        end += 1;
    }
    let token: String = chars[start..end].iter().collect();
    if token.is_empty() {
        None
    } else {
        Some((start, token))
    }
}

/// Split a completion token at its last `/`: `(dir_part incl. '/', base, is_absolute)`.
pub fn split_path_token(token: &str) -> (String, String, bool) {
    match token.rfind('/') {
        Some(i) => (
            token[..=i].to_string(),
            token[i + 1..].to_string(),
            token.starts_with('/'),
        ),
        None => (String::new(), token.to_string(), false),
    }
}

/// The directory to list for `dir_part`: absolute as-is; `~/…` under
/// $HOME; relative joined to the cwd; empty = cwd. None when HOME is
/// needed but unset.
fn path_root(dir_part: &str) -> Option<std::path::PathBuf> {
    if dir_part.starts_with('/') {
        return Some(std::path::PathBuf::from(dir_part));
    }
    if dir_part == "~" {
        return std::env::var("HOME").ok().map(std::path::PathBuf::from);
    }
    if let Some(rest) = dir_part.strip_prefix("~/") {
        let home = std::env::var("HOME").ok()?;
        return Some(std::path::PathBuf::from(home).join(rest));
    }
    let cwd = std::env::current_dir().ok()?;
    Some(if dir_part.is_empty() {
        cwd
    } else {
        cwd.join(dir_part)
    })
}

/// Entries in `dir_part` whose names start with `base`. Dotfiles are
/// hidden unless `base` itself starts with `.`. Sorted; capped at 200.
pub fn list_matches(dir_part: &str, base: &str) -> Vec<(String, bool)> {
    let Some(root) = path_root(dir_part) else {
        return Vec::new();
    };
    let show_dots = base.starts_with('.');
    let mut out: Vec<(String, bool)> = Vec::new();
    let Ok(entries) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !show_dots && name.starts_with('.') {
            continue;
        }
        if !name.starts_with(base) {
            continue;
        }
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        out.push((name, is_dir));
        if out.len() >= 200 {
            break;
        }
    }
    out.sort();
    out
}

/// Replace the word at `[tok_start, tok_start + tok_len)` (chars) with
/// `insert`; returns the new text and the cursor after the insertion.
pub fn complete_token(
    text: &str,
    tok_start: usize,
    tok_len: usize,
    insert: &str,
) -> (String, usize) {
    let mut chars: Vec<char> = text.chars().collect();
    chars.splice(tok_start..(tok_start + tok_len), insert.chars());
    let out: String = chars.into_iter().collect();
    let cursor = tok_start + insert.chars().count();
    (out, cursor)
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
/// ka-engine's `config::user_config_path`).
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
pub struct Slash {
    pub event: Option<Command>,
    pub quit: bool,
    pub followup: Option<String>,
    /// Modal to open instead of sending an event.
    pub modal: Option<ModalKind>,
    /// Local transcript note (no engine roundtrip).
    pub note: Option<String>,
}

/// Modal a slash command opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModalKind {
    /// `/mode` permission picker (borrows the input box).
    Mode,
    /// Session picker.
    Session,
    /// Settings panel.
    Settings,
    /// Model picker.
    Model,
    /// Provider picker.
    Provider,
    /// API key prompt.
    Key,
    /// Spill-file viewer.
    Spills,
    /// MCP prompt picker.
    Prompts,
    /// Memory viewer.
    Memory,
    /// Usage dashboard.
    Usage,
    /// /tasks picker (rows arrive with `Event::Tasks`).
    Tasks,
    /// /debug overlay (rows arrive with `Event::DebugRoster`).
    Debug,
    /// Strand tree.
    Tree,
    /// Help overlay.
    Help,
}

/// Load a custom command body from `.ka/commands/<name>.md` (project) or
/// the user dir; `$ARGUMENTS` substituted with the rest of the line.
fn project_trusted_in(state_home: &std::path::Path, cwd: &std::path::Path) -> bool {
    // exact, canonicalized membership via the engine's helper: the old
    // raw-substring match missed differently-spelled launch dirs and
    // could false-positive on sibling path prefixes
    let file = state_home.join("ka/trust.json");
    ka_engine::trust::trusted_in(cwd, &ka_engine::trust::load_trust_at(&file))
}

/// One discovered custom slash command.
pub struct CustomCommand {
    /// Command name (without `/`).
    pub name: String,
    /// Frontmatter description (or first body line).
    pub description: String,
    /// Frontmatter argument hint.
    pub argument_hint: String,
}

/// Project command directories, scanned for the popup.
const PROJECT_COMMAND_DIRS: &[&str] = &[".ka/commands", ".agents/commands", ".claude/commands"];

/// Scan custom slash commands: project dirs (trust-gated like skills)
/// plus the user dir (ungated). Project wins on name collisions.
pub fn scan_custom_commands(cwd: &std::path::Path) -> Vec<CustomCommand> {
    let state_home = std::env::var("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|_| {
            std::env::var("HOME").map(|h| std::path::PathBuf::from(h).join(".local/state"))
        })
        .unwrap_or_else(|_| std::env::temp_dir());
    scan_custom_commands_in(cwd, &state_home)
}

pub fn scan_custom_commands_in(
    cwd: &std::path::Path,
    state_home: &std::path::Path,
) -> Vec<CustomCommand> {
    let mut out: Vec<CustomCommand> = Vec::new();
    let mut dirs: Vec<std::path::PathBuf> = Vec::new();
    // project scope anchors at the root project, matching the engine's
    // trust store entries (one approval covers every launch dir)
    let root = ka_engine::project_root(cwd);
    if project_trusted_in(state_home, &root) {
        dirs.extend(PROJECT_COMMAND_DIRS.iter().map(|d| root.join(d)));
    }
    if let Ok(home) = std::env::var("HOME") {
        dirs.push(std::path::PathBuf::from(home).join(".config/ka/commands"));
    }
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "md") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let (description, argument_hint, _body) = parse_command_md(&text);
            let name = path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            if out.iter().any(|c: &CustomCommand| c.name == name) {
                continue;
            }
            out.push(CustomCommand {
                name,
                description,
                argument_hint,
            });
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Split frontmatter (`description`, `argument-hint`) off a command
/// markdown body.
fn parse_command_md(text: &str) -> (String, String, String) {
    let mut description = String::new();
    let mut argument_hint = String::new();
    let mut body = text.to_string();
    if let Some(rest) = text.strip_prefix("---") {
        if let Some(end) = rest.find("\n---") {
            for line in rest[..end].lines() {
                if let Some((key, value)) = line.split_once(':') {
                    match key.trim() {
                        "description" => description = value.trim().to_string(),
                        "argument-hint" | "argument_hint" => {
                            argument_hint = value.trim().to_string();
                        }
                        _ => {}
                    }
                }
            }
            body = rest[end + 4..].trim_start_matches('\n').to_string();
        }
    }
    if description.is_empty() {
        description = body
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("custom command")
            .chars()
            .take(60)
            .collect();
    }
    (description, argument_hint, body)
}

fn custom_command(head: &str, rest: Option<&str>) -> Option<String> {
    // safe mode: custom commands are customizations — typed commands
    // must not load their bodies from disk (built-ins only)
    if ka_engine::conventions::bare_mode() {
        return None;
    }
    let cwd = std::env::current_dir().ok()?;
    let state_home = std::env::var("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|_| {
            std::env::var("HOME").map(|h| std::path::PathBuf::from(h).join(".local/state"))
        })
        .unwrap_or_else(|_| std::env::temp_dir());
    custom_command_in(&cwd, &state_home, head, rest)
}

fn custom_command_in(
    cwd: &std::path::Path,
    state_home: &std::path::Path,
    head: &str,
    rest: Option<&str>,
) -> Option<String> {
    let name = head.strip_prefix('/')?;
    // project scope anchors at the root project, not the launch dir
    let root = ka_engine::project_root(cwd);
    let mut candidates = vec![
        root.join(format!(".ka/commands/{name}.md")),
        root.join(format!(".agents/commands/{name}.md")),
        root.join(format!(".claude/commands/{name}.md")),
    ];
    if let Ok(home) = std::env::var("HOME") {
        candidates
            .push(std::path::PathBuf::from(home).join(format!(".config/ka/commands/{name}.md")));
    }
    let home = std::env::var("HOME").unwrap_or_default();
    for path in candidates {
        // user-dir commands are ungated; project commands pay the trust
        // gate (like skills)
        let is_user_file = !home.is_empty() && path.starts_with(&home);
        if !is_user_file && !project_trusted_in(state_home, &root) {
            continue;
        }
        if let Ok(body) = std::fs::read_to_string(&path) {
            let args = rest.unwrap_or("");
            let (_, _, clean) = parse_command_md(&body);
            return Some(clean.replace("$ARGUMENTS", args));
        }
    }
    None
}

pub fn slash_command(text: &str) -> Option<Slash> {
    let mut parts = text.splitn(2, ' ');
    let head = parts.next()?.trim();
    let rest = parts.next().map(str::trim).filter(|s| !s.is_empty());
    // custom commands load from files; builtins win over files
    if !matches!(
        head,
        "/quit"
            | "/exit"
            | "/model"
            | "/provider"
            | "/mode"
            | "/compact"
            | "/session"
            | "/resume"
            | "/new"
            | "/settings"
            | "/key"
            | "/export"
            | "/spills"
            | "/fork"
            | "/checkpoint"
            | "/restore"
            | "/mcp"
            | "/prompt"
            | "/tree"
            | "/memory"
            | "/tasks"
            | "/debug"
    ) {
        if let Some(body) = custom_command(head, rest) {
            return Some(Slash {
                note: None,
                event: Some(Command::Prompt {
                    text: body,
                    schema: None,
                    images: Vec::new(),
                }),
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
        "/provider" => Some(Slash {
            note: None,
            event: None,
            quit: false,
            followup: None,
            modal: Some(ModalKind::Provider),
        }),
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
                    "Plan this task. Research the codebase with read/glob/grep/pathfinder, \
then write a concrete numbered implementation plan to {}. Task: {task}",
                    plan_file_path().display()
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
            followup: Some(build_followup()),
        }),
        "/review" => {
            // read-only review preset (codex /review shape): plan-mode
            // locking + a strict reviewer prompt; the report lands in
            // chat where /copy and /export can reach it
            let base = rest
                .map(str::trim)
                .filter(|r| !r.is_empty())
                .unwrap_or("the repository's default branch — main or master, whichever exists; if neither, HEAD");
            Some(Slash {
                note: None,
                event: Some(Command::SetMode {
                    mode: ka_protocol::Mode::Plan,
                }),
                quit: false,
                modal: None,
                followup: Some(format!(
                    "Review the current changes as a strict senior engineer — READ-ONLY, \\
do not modify any files. Compare the working tree against {base} (git diff plus \\
untracked files, via the bash tool; read files for full context). Produce: \\
(1) a one-line verdict (ship / fix first / blocked), (2) findings ordered \\
blocker > major > minor > nit, each as `file:line — issue — concrete fix`, \\
(3) what is missing (tests, docs, error handling)."
                )),
            })
        }
        "/approve" => Some(Slash {
            note: None,
            event: Some(Command::SetMode {
                mode: ka_protocol::Mode::Guarded,
            }),
            quit: false,
            modal: None,
            followup: Some(build_followup()),
        }),
        "/rewind" => {
            // bare /rewind drops the last exchange; an unparsable count
            // is refused rather than silently defaulted (a wrong rewind
            // cut loses turns)
            let turns = match rest {
                None => Some(1),
                Some(r) => r.trim().parse::<u32>().ok(),
            };
            Some(match turns {
                Some(turns) => Slash {
                    note: None,
                    event: Some(Command::Rewind { turns }),
                    quit: false,
                    followup: None,
                    modal: None,
                },
                None => Slash {
                    note: Some("usage: /rewind [n]".to_string()),
                    event: None,
                    quit: false,
                    followup: None,
                    modal: None,
                },
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
        "/fork" => {
            // bare /fork copies the session as-is; an unparsable count is
            // refused rather than defaulted — a wrong fork cut loses turns
            let turns = match rest {
                None => Some(0),
                Some(r) => r.trim().parse::<u32>().ok(),
            };
            Some(match turns {
                Some(turns) => Slash {
                    note: None,
                    event: Some(Command::ForkStrand { turns }),
                    quit: false,
                    followup: None,
                    modal: None,
                },
                None => Slash {
                    note: Some("usage: /fork [turns]".to_string()),
                    event: None,
                    quit: false,
                    followup: None,
                    modal: None,
                },
            })
        }
        "/checkpoint" => Some(Slash {
            note: None,
            event: Some(Command::Checkpoint),
            quit: false,
            followup: None,
            modal: None,
        }),
        "/restore" => Some(Slash {
            note: None,
            event: Some(Command::RestoreCheckpoint {
                id: rest
                    .map(str::to_string)
                    .unwrap_or_else(|| "list".to_string()),
            }),
            quit: false,
            followup: None,
            modal: None,
        }),
        "/export" => {
            // `/export [--html] [path]` — html renders a self-contained
            // offline page via the shared ka-strand renderer
            let rest = rest.unwrap_or_default().trim();
            let html = rest.starts_with("--html");
            let path = rest.trim_start_matches("--html").trim();
            Some(Slash {
                note: None,
                event: Some(Command::ExportMarkdown {
                    out: (!path.is_empty()).then(|| std::path::PathBuf::from(path)),
                    html,
                }),
                quit: false,
                followup: None,
                modal: None,
            })
        }
        "/spills" => Some(Slash {
            note: None,
            event: None,
            quit: false,
            followup: None,
            modal: Some(ModalKind::Spills),
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
        "/tasks" => Some(Slash {
            note: None,
            event: Some(Command::ListTasks),
            quit: false,
            followup: None,
            modal: None,
        }),
        "/debug" => Some(Slash {
            note: None,
            event: Some(Command::DebugRoster),
            quit: false,
            followup: None,
            modal: None,
        }),
        "/memory" => Some(Slash {
            note: None,
            event: None,
            quit: false,
            followup: None,
            modal: Some(ModalKind::Memory),
        }),
        "/tree" => Some(Slash {
            note: None,
            event: None,
            quit: false,
            followup: None,
            modal: Some(ModalKind::Tree),
        }),
        "/mcp" => match rest {
            Some("refresh") => Some(Slash {
                note: None,
                event: Some(Command::RefreshMcp),
                quit: false,
                followup: None,
                modal: None,
            }),
            _ => Some(Slash {
                note: Some("usage: /mcp refresh — re-list tools on every MCP server".into()),
                event: None,
                quit: false,
                followup: None,
                modal: None,
            }),
        },
        "/prompt" => {
            let parsed = rest.and_then(parse_prompt_invocation);
            match parsed {
                Some((server, name, args)) => Some(Slash {
                    note: None,
                    event: Some(Command::CallPrompt { server, name, args }),
                    quit: false,
                    followup: None,
                    modal: None,
                }),
                None => Some(Slash {
                    note: None,
                    event: None,
                    quit: false,
                    followup: None,
                    modal: Some(ModalKind::Prompts),
                }),
            }
        }
        "/settings" => Some(Slash {
            note: None,
            event: None,
            quit: false,
            followup: None,
            modal: Some(ModalKind::Settings),
        }),
        "/usage" => Some(Slash {
            note: None,
            event: None,
            quit: false,
            followup: None,
            modal: Some(ModalKind::Usage),
        }),
        "/context" => Some(Slash {
            note: None,
            event: None, // dispatched by the busy gate in the app loop
            quit: false,
            followup: None,
            modal: None,
        }),
        "/key" => Some(Slash {
            note: None,
            event: None,
            quit: false,
            followup: None,
            modal: Some(ModalKind::Key),
        }),
        "/mode" => match rest {
            None => Some(Slash {
                note: None,
                event: None,
                quit: false,
                followup: None,
                modal: Some(ModalKind::Mode),
            }),
            Some(name) => {
                let mode = match name {
                    "guarded" | "needs-approval" | "needs_approval" => ka_protocol::Mode::Guarded,
                    "accept-edits" | "accept_edits" => ka_protocol::Mode::AcceptEdits,
                    "free" | "full-access" | "full_access" => ka_protocol::Mode::Free,
                    "plan" => ka_protocol::Mode::Plan,
                    other => {
                        return Some(Slash {
                            note: Some(format!("unknown mode {other:?} — pick one below")),
                            event: None,
                            quit: false,
                            followup: None,
                            modal: Some(ModalKind::Mode),
                        });
                    }
                };
                Some(Slash {
                    note: None,
                    event: Some(Command::SetMode { mode }),
                    quit: false,
                    followup: None,
                    modal: None,
                })
            }
        },
        _ => None,
    }
}

/// Everything the live region renders below the cached transcript rows:
/// the in-flight thought tail, the running tool block, streaming markdown.
#[derive(Debug, Clone)]
struct LiveBlock {
    thought: String,
    /// Running tool header (`→ tool · detail`); empty while no call is
    /// in flight. The band folds from this + the live preview at the
    /// RENDER width (prebuilt rows would survive a resize stale).
    tool_header: String,
    /// The in-flight call's rolling output preview.
    live_tool: Option<LiveTool>,
    md: Vec<ratatui::text::Line<'static>>,
}

/// Most recent preview lines shown under the running tool header.
const PREVIEW_WINDOW: usize = 3;

/// Flush in-progress thought and assistant text into the transcript, in
/// turn order. Used at call boundaries (chronology) and at turn end.
fn flush_live_text(transcript: &mut Transcript, thought: &mut String, assistant: &mut String) {
    if !thought.trim().is_empty() {
        transcript.push_separated(Line::Thought(std::mem::take(thought)));
    }
    if !assistant.trim().is_empty() {
        transcript.push_separated(Line::Assistant(std::mem::take(assistant)));
    }
}

/// Fold a preview excerpt into the rolling window: every line kept, at
/// most [`PREVIEW_WINDOW`] of the most recent survive.
fn observe_preview(window: &mut Vec<String>, excerpt: &str) {
    for line in excerpt.lines() {
        window.push(line.to_string());
    }
    if window.len() > PREVIEW_WINDOW {
        window.drain(..window.len() - PREVIEW_WINDOW);
    }
}

/// Width-truncate one preview row to `width` chars (chars, not bytes),
/// marking the cut with an ellipsis.
fn preview_row(line: &str, width: usize) -> String {
    trunc_cols(line, width.saturating_sub(1))
}

/// Live tool block rows while a call is running: the `→ {tool}` header
/// plus up to [`PREVIEW_WINDOW`] dim preview lines. The violet band
/// hugs its content — a quiet rail, then content with one column of air
/// each side; the cached railed rows that replace it sit on the canvas.
fn tool_live_rows(
    header: &str,
    live: Option<&LiveTool>,
    width: usize,
) -> Vec<ratatui::text::Line<'static>> {
    use ratatui::text::Line as TuiLine;
    let Some(lt) = live else {
        return Vec::new();
    };
    // the bg must live on a SPAN — Paragraph ignores line-level styles
    let band_span = |content: String, fg: ratatui::style::Color| {
        TuiLine::from(vec![
            ratatui::text::Span::styled(" │ ", crate::palette::BORDER_QUIET_STYLE),
            ratatui::text::Span::styled(
                content,
                ratatui::style::Style::new()
                    .fg(fg)
                    .bg(crate::palette::BG_TOOL),
            ),
        ])
    };
    let head = trunc_cols(header, width.saturating_sub(5));
    let mut rows = vec![band_span(format!(" {head} "), crate::palette::TOOL)];
    let start = lt.preview.len().saturating_sub(PREVIEW_WINDOW);
    for line in &lt.preview[start..] {
        rows.push(band_span(
            format!("  {}", preview_row(line, width.saturating_sub(6))),
            crate::palette::FAINT,
        ));
    }
    rows
}

/// The one selection identity of the whole TUI: a full-row pink bar
/// carrying near-black text (pad_to_width fills the row with SEL_BG).
fn selection_style() -> ratatui::style::Style {
    use ratatui::style::Modifier;
    ratatui::style::Style::new()
        .fg(crate::palette::SEL_FG)
        .bg(crate::palette::SEL_BG)
        .add_modifier(Modifier::BOLD)
}

/// Pad a row to `width` display columns (unicode-width aware) so an
/// inverse selection reads as a bar across the row, not a word.
fn pad_to_width(s: String, width: usize) -> String {
    use unicode_width::UnicodeWidthStr;
    let used = s.width();
    if used >= width {
        return s;
    }
    s + &" ".repeat(width - used)
}

/// Status-bar key hints: each entry is a bold key plus a dim action,
/// separated by dim ` · `.
/// Memory tier files for /memory: project MEMORY.md, then the
/// user-level one. Mirrors the engine's system-prompt fold.
/// Rows for the /memory modal: loaded MEMORY.md tiers.
pub fn memory_modal_rows(cwd: &std::path::Path) -> Vec<String> {
    let files = discover_memory_files(cwd);
    let mut rows = Vec::new();
    if files.is_empty() {
        rows.push("(no memory files)".to_string());
        rows.push("create MEMORY.md at the project root or ~/.config/ka/MEMORY.md".to_string());
    }
    for (path, content) in files {
        rows.push(format!("▸ {}", path.display()));
        for line in content.lines() {
            rows.push(line.to_string());
        }
    }
    rows
}

/// The staged-memory inbox file path. Anchors at the root project
/// (nearest `.git` ancestor, else cwd) — the same file the engine's
/// `remember` hand stages into, whatever directory the session runs in.
fn memory_inbox_path(cwd: &std::path::Path) -> std::path::PathBuf {
    ka_engine::project_root(cwd).join(".ka/memory/inbox.md")
}

/// The project MEMORY.md tier: root-anchored, mirroring the engine's
/// [`ka_engine::conventions`] discovery.
fn project_memory_path(cwd: &std::path::Path) -> std::path::PathBuf {
    ka_engine::project_root(cwd).join("MEMORY.md")
}

/// Staged memory proposals, oldest first.
pub fn read_memory_inbox(cwd: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(memory_inbox_path(cwd))
        .map(|text| {
            text.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Persist the inbox after a review action (removed when empty).
pub fn write_memory_inbox(cwd: &std::path::Path, inbox: &[String]) {
    let path = memory_inbox_path(cwd);
    if inbox.is_empty() {
        let _ = std::fs::remove_file(path);
        return;
    }
    let mut body = inbox.join(
        "
",
    );
    body.push('\n');
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, body);
}

/// Append one staged note to a MEMORY.md tier (project or user).
/// Strips the `- [stamp] ` prefix before writing.
pub fn accept_memory_note(cwd: &std::path::Path, staged: &str, user: bool) -> std::io::Result<()> {
    let note = staged
        .split_once("] ")
        .map(|(_, note)| note)
        .unwrap_or(staged)
        .trim()
        .trim_start_matches("- ")
        .to_string();
    if note.is_empty() {
        return Ok(());
    }
    let target = if user {
        std::env::var("HOME")
            .map(|h| std::path::PathBuf::from(h).join(".config/ka/MEMORY.md"))
            .unwrap_or_else(|_| project_memory_path(cwd))
    } else {
        project_memory_path(cwd)
    };
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut body = std::fs::read_to_string(&target).unwrap_or_default();
    if !body.is_empty() && !body.ends_with('\n') {
        body.push('\n');
    }
    body.push_str(&note);
    body.push('\n');
    std::fs::write(&target, body)
}

fn discover_memory_files(cwd: &std::path::Path) -> Vec<(std::path::PathBuf, String)> {
    let mut out = Vec::new();
    let project = project_memory_path(cwd);
    if let Ok(content) = std::fs::read_to_string(&project) {
        if !content.trim().is_empty() {
            out.push((project, content));
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        let user = std::path::PathBuf::from(home).join(".config/ka/MEMORY.md");
        if let Ok(content) = std::fs::read_to_string(&user) {
            if !content.trim().is_empty() {
                out.push((user, content));
            }
        }
    }
    out
}

fn hint_spans(pairs: &[(&str, &str)]) -> Vec<ratatui::text::Span<'static>> {
    use ratatui::style::{Modifier, Style};
    use ratatui::text::Span;
    let key = Style::new().add_modifier(Modifier::BOLD);
    let mut out: Vec<Span<'static>> = Vec::new();
    for (i, (k, v)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push(Span::styled(" · ".to_string(), crate::palette::META));
        }
        out.push(Span::styled((*k).to_string(), key));
        out.push(Span::styled(format!(" {v}"), crate::palette::META));
    }
    out
}

/// Status-bar right side: `{model} · {mode} · ctx {gauge} {pct}% ·
/// ${cost}` — facts joined only when known, cost always on.
fn status_right(meters: &Meters) -> Vec<ratatui::text::Span<'static>> {
    let (used, window) = meters.context;
    let mut segs: Vec<ratatui::text::Span<'static>> = Vec::new();
    let push = |s: String, segs: &mut Vec<ratatui::text::Span<'static>>| {
        if !segs.is_empty() {
            segs.push(ratatui::text::Span::styled(" · ", crate::palette::META));
        }
        segs.push(ratatui::text::Span::styled(s, crate::palette::META));
    };
    if !meters.model.is_empty() {
        push(meters.model.clone(), &mut segs);
    }
    if !meters.mode.is_empty() {
        push(meters.mode.clone(), &mut segs);
    }
    if window > 0 {
        let pct = (used as f64 / window as f64 * 100.0) as u64;
        // eight cells, one step per 12.5%; the bar warms as the window fills
        let filled = ((used as f64 / window as f64) * 8.0).round() as usize;
        let filled = filled.clamp(0, 8);
        let gauge_color = if pct >= 95 {
            crate::palette::ERR
        } else if pct >= 80 {
            crate::palette::WARN
        } else {
            crate::palette::OK
        };
        push("ctx".to_string(), &mut segs);
        segs.push(ratatui::text::Span::styled(
            format!(" {}{}", "█".repeat(filled), "·".repeat(8 - filled)),
            ratatui::style::Style::new().fg(gauge_color),
        ));
        segs.push(ratatui::text::Span::styled(
            format!(" {pct}%"),
            crate::palette::META,
        ));
    } else if used > 0 {
        push(format!("~{} tok", fmt_tok(used)), &mut segs);
    }
    push(format!("${:.4}", meters.cost), &mut segs);
    segs
}

/// The fresh-conversation welcome: quiet rows centered in the
/// transcript pane. Rendered only while the transcript is empty and no
/// turn is in flight — the first exchange (or a replayed older chat)
/// replaces it.
fn welcome_rows(width: usize, glyph: &str, meters: &Meters) -> Vec<ratatui::text::Line<'static>> {
    use unicode_width::UnicodeWidthStr;
    let center = |text: &str, style: ratatui::style::Style| -> ratatui::text::Line<'static> {
        let pad = width.saturating_sub(text.width()) / 2;
        ratatui::text::Line::from(vec![
            ratatui::text::Span::raw(" ".repeat(pad)),
            ratatui::text::Span::styled(text.to_string(), style),
        ])
    };
    let glyph = if glyph.is_empty() { "◆" } else { glyph };
    // model + mode ride from the meters; empty segments stay hidden
    let config = [meters.model.as_str(), meters.mode.as_str()]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" · ");
    let faint = ratatui::style::Style::new().fg(crate::palette::FAINT);
    let mut rows = vec![
        center(&format!("· {glyph} ·"), crate::palette::ACCENT_STYLE),
        center("new conversation", crate::palette::THOUGHT),
    ];
    if !config.is_empty() {
        rows.push(center(&config, faint));
    }
    rows.push(center("/help keys · /session past chats", faint));
    rows
}

#[allow(clippy::too_many_arguments)]
fn render(
    frame: &mut ratatui::Frame,
    transcript: &mut Transcript,
    scroll: Option<usize>,
    input: &str,
    cursor: usize,
    busy: bool,
    busy_since: Option<Instant>,
    now: Instant,
    queued: usize,
    ask: Option<&PendingAsk>,
    live: Option<&LiveBlock>,
    popup: Option<&SlashPopup>,
    path: Option<&PathPopup>,
    // live reverse-search query line; Some while Ctrl+R search is active
    rsearch: Option<&str>,
    modal: Option<&Modal>,
    picker: Option<&ModePicker>,
    meters: &Meters,
    sidebar: &SidebarState,
    fresh: bool,
    // mouse mode: the idle hint bar teaches the selection/scroll story
    // for whichever mode is live
    mouse_captured: bool,
    header_glyph: &str,
    strip_zone: &std::cell::Cell<Option<StripZones>>,
    // transient action feedback, auto-expiring; None most of the time
    toast: Option<&str>,
    title_arrows: &std::cell::Cell<Option<TitleArrows>>,
    // content rows + first visible row: click-to-collapse hit mapping
    tx_content: &std::cell::Cell<Option<(ratatui::layout::Rect, usize)>>,
    // the transcript width this frame adopts, published back to the
    // tick so the markdown cache keys on the real render width
    tx_width: &std::cell::Cell<u16>,
) {
    use ratatui::layout::Constraint::{Length, Min};
    use ratatui::style::{Modifier, Style};
    use ratatui::text::{Line as TuiLine, Span};
    use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Wrap};
    // ── canvas: paint the whole frame before any widget so the app sits
    // on the indigo ground with lavender prose, whatever the terminal
    // theme paints behind it ──
    frame.render_widget(Block::new().style(crate::palette::CANVAS), frame.area());
    // one column of air on each side of the frame; rows stay edge to
    // edge (they are scarce) — except the single empty row above the
    // transcript's top border, which gives the window a top margin.
    // Every width below derives from these chunks, never from the raw
    // frame area.
    let outer = frame.area().inner(ratatui::layout::Margin::new(1, 0));
    let chunks = ratatui::layout::Layout::vertical([
        Length(1),
        Min(3),
        Length(input_height(input_area_rows(
            picker,
            input,
            input_inner_w(frame.area().width),
        ))),
        Length(1), // bottom strip: popup buttons + cwd:branch
        Length(1),
    ])
    .split(outer);
    let tx_area = chunks[1];
    let tx_inner = tx_area.width.saturating_sub(2);
    // single width authority: the frame's own layout decides the cache
    // and band width — never a tick-side terminal.size() guess
    tx_width.set(tx_inner);
    transcript.set_width(tx_inner);
    // modals center within the transcript pane, one col clear of its
    // edges: they never touch the input box or the status bar
    let modal_area = tx_area.inner(ratatui::layout::Margin::new(1, 0));

    // ── transcript: cached rows + live region under a scroll window ──
    let mut live_rows: Vec<TuiLine> = Vec::new();
    if let Some(lb) = live {
        if !lb.thought.trim().is_empty() {
            let tail: Vec<&str> = lb
                .thought
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
        live_rows.extend(tool_live_rows(
            lb.tool_header.as_str(),
            lb.live_tool.as_ref(),
            tx_inner as usize,
        ));
        // cached markdown rows (clone-on-append; the cache stays cursor-free)
        // live markdown rows get the same surface as final output; the
        // cursor is appended first so the surface pads around it, and
        // blank rows above and below give the live card the same inner
        // air the cached rows carry
        if !lb.md.is_empty() {
            let mut md_live: Vec<TuiLine> = lb.md.to_vec();
            if let Some(last) = md_live.last_mut() {
                last.spans
                    .push(Span::styled("▌", crate::palette::ACCENT_STYLE));
            }
            live_rows.push(surface_blank(tx_inner, crate::palette::BG_OUTPUT));
            apply_output_surface(&mut md_live, tx_inner, crate::palette::BG_OUTPUT);
            live_rows.extend(md_live);
            live_rows.push(surface_blank(tx_inner, crate::palette::BG_OUTPUT));
        }
        // transient tail of the live region: the working indicator
        // rides below the last live row and is never cached; blank
        // rows above and below give it a little air so it never
        // crowds the live card or the input box
        live_rows.push(TuiLine::default());
        live_rows.push(working_row(ask, busy_since, now));
        live_rows.push(TuiLine::default());
    }

    let cached = transcript.total_rows();
    let total = cached + live_rows.len();
    let visible = chunks[1].height.saturating_sub(1) as usize;
    debug_assert_eq!(
        visible,
        visible_rows(frame.area().height, chunks[2].height),
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
    // an empty, idle, fresh transcript opens on a quiet welcome — the
    // first exchange (or a replayed older chat) replaces it
    if total == 0 && !busy && fresh {
        window = welcome_rows(tx_inner as usize, header_glyph, meters);
    }
    // the title row: the session glyph, plus the auto-generated session
    // title once the engine names it; scrolled viewports show how much
    // history sits above
    let session_title = sidebar
        .title
        .as_deref()
        .filter(|t| !t.is_empty())
        .map(|t| trunc_cols(t, 48));
    let title = match (&session_title, pinned) {
        (Some(t), true) => format!("{header_glyph} {t}"),
        (Some(t), false) => format!("{header_glyph} {t} · ↑{} above (pgdn/esc)", start),
        (None, true) => header_glyph.to_string(),
        (None, false) => format!("{header_glyph} · ↑{} above (pgdn/esc)", start),
    };
    // cells the padded title occupies (one space of air each side)
    let title_cells = title.chars().count() + 2;
    let widget = Paragraph::new(window).block(
        Block::default()
            .borders(Borders::TOP)
            .title(padded_title(title))
            .border_style(crate::palette::BORDER_QUIET_STYLE)
            .padding(ratatui::widgets::Padding::horizontal(1)),
    );
    frame.render_widget(widget, tx_area);

    // ── ▲▼ user-message jump arrows at the end of the title row ──
    // Only in captured-mouse mode (the zones are click targets — in
    // native mode they would be dead pixels; Ctrl+↑/↓ are the keyboard
    // twin and always work), once the user has sent a message, and only
    // when the pane is wide enough that the arrows never collide with
    // the title text. ▲ steps to the previous user message, ▼ to the
    // next.
    title_arrows.set(None);
    // content rows (below the title) + the first visible transcript row:
    // click-to-collapse maps screen rows back to entries through this
    let content_area = ratatui::layout::Rect {
        x: tx_area.x,
        y: tx_area.y + 1,
        width: tx_area.width,
        height: tx_area.height.saturating_sub(1),
    };
    tx_content.set(Some((content_area, start)));
    let has_user = transcript
        .entries()
        .iter()
        .any(|l| matches!(l, Line::User(_)));
    if has_user && tx_area.width >= 12 && mouse_captured {
        let need = title_cells + 7;
        if tx_area.width as usize >= need {
            let y = tx_area.y;
            let x0 = tx_area.x + tx_area.width - 4; // ' ', '▲', ' ', '▼'
            let buf = frame.buffer_mut();
            for (i, sym) in [' ', '▲', ' ', '▼'].iter().enumerate() {
                if let Some(c) = buf.cell_mut((x0 + i as u16, y)) {
                    c.set_symbol(&sym.to_string());
                    if *sym != ' ' {
                        c.set_style(crate::palette::ACCENT_STYLE);
                    }
                }
            }
            title_arrows.set(Some(TitleArrows {
                up: ratatui::layout::Rect {
                    x: x0,
                    y,
                    width: 2,
                    height: 1,
                },
                down: ratatui::layout::Rect {
                    x: x0 + 2,
                    y,
                    width: 2,
                    height: 1,
                },
            }));
        }
    }

    // ── scrollbar rail: painted into the transcript's always-blank
    // right padding column, so no width math ever learns about it ──
    if total > visible {
        let (thumb_pos, thumb_len) = rail_thumb(visible, total, visible, start);
        // span-level styling: Paragraph paints span styles, not line styles
        let rail: Vec<TuiLine> = (0..visible)
            .map(|i| {
                if (thumb_pos..thumb_pos + thumb_len).contains(&i) {
                    TuiLine::from(vec![Span::styled("█", crate::palette::META_STYLE)])
                } else {
                    TuiLine::from(vec![Span::styled("│", crate::palette::BORDER_QUIET_STYLE)])
                }
            })
            .collect();
        frame.render_widget(
            Paragraph::new(rail),
            ratatui::layout::Rect {
                x: tx_area.x + tx_area.width.saturating_sub(1),
                y: tx_area.y + 1,
                width: 1,
                height: visible as u16,
            },
        );
    }

    // ── input ─────────────────────────────────────────────────────
    // permission asks render as centered modals; only the /mode
    // picker borrows the box now. Titles carry only structural/draft
    // state — the action hints live in the status bar
    let title = if picker.is_some() {
        "mode".to_string()
    } else if let Some(q) = rsearch {
        format!("input · {q}")
    } else {
        busy_input_title(queued)
    };
    let input_border = if modal.is_some() || popup.is_some() || path.is_some() || picker.is_some() {
        crate::palette::META_STYLE
    } else if busy {
        ratatui::style::Style::new().fg(crate::palette::WARN)
    } else {
        crate::palette::BORDER_STYLE
    };
    // textarea wrap: display rows fold at the box width; the visible
    // window ends at the cursor's visual row and the terminal cursor
    // rides its offset inside that window
    let (draft_window, cursor_vis): (Option<Vec<TuiLine>>, Option<(usize, usize)>) =
        if picker.is_none() {
            let inner_w = chunks[2].width.saturating_sub(4) as usize; // borders + padding
            let rows = wrap_rows(input, inner_w);
            let cur_row = visual_cursor_row(&rows, cursor);
            let vis = rows.len().min(6);
            let start = cur_row.saturating_sub(vis - 1).min(rows.len() - vis);
            let window: Vec<TuiLine> = rows[start..start + vis]
                .iter()
                .map(|r| TuiLine::from(r.text.clone()))
                .collect();
            let col: usize = rows[cur_row]
                .text
                .chars()
                .take(cursor - rows[cur_row].start)
                .map(|c| unicode_width::UnicodeWidthChar::width(c).unwrap_or(0))
                .sum();
            (Some(window), Some((cur_row - start, col)))
        } else {
            (None, None)
        };
    let body: Vec<TuiLine> = if let Some(pk) = picker {
        // one row per tier: the selected row is a full-width inverse
        // bar, the label column stays padded so descriptions align, and
        // unselected descriptions ride along in DIM
        let inner_w = chunks[2].width.saturating_sub(4) as usize; // borders + padding
        let mut rows = Vec::with_capacity(MODE_CHOICES.len());
        for (i, (_, label, desc)) in MODE_CHOICES.iter().enumerate() {
            if i == pk.selected {
                rows.push(TuiLine::styled(
                    pad_to_width(format!("  {label:<15} {desc}"), inner_w),
                    selection_style(),
                ));
            } else {
                rows.push(TuiLine::from(vec![
                    Span::raw(format!("  {label:<15}")),
                    Span::styled(*desc, crate::palette::FAINT),
                ]));
            }
        }
        rows
    } else {
        draft_window.clone().unwrap_or_default()
    };
    let input_widget = Paragraph::new(body)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .title(padded_title(title))
                .border_style(input_border)
                .padding(ratatui::widgets::Padding::horizontal(1)),
        )
        .style(ratatui::style::Style::new().bg(crate::palette::BG_PANEL));
    frame.render_widget(input_widget, chunks[2]);
    // the hardware cursor only rides the draft when it can actually
    // follow keystrokes (an open ask captures input into its dialog)
    if ask.is_none() {
        if let (Some(_), Some((row_off, col))) = (&draft_window, cursor_vis) {
            let area = chunks[2];
            frame.set_cursor_position((
                area.x + 2 + col as u16,
                area.y + 1 + row_off.min(area.height.saturating_sub(2) as usize) as u16,
            ));
        }
    }

    // ── bottom strip: popup buttons left, cwd:branch right ──────────
    // capture mode makes the buttons clickable; native mode uses the
    // keyboard shortcuts printed on them (terminals only deliver clicks
    // under mouse reporting)
    {
        // one span per button: bright label + faint key hint
        let btn = |label: &str, key: &str| {
            (
                Span::styled(
                    format!(" {label}"),
                    ratatui::style::Style::new()
                        .fg(crate::palette::FG_STRONG)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!(" {key} "), crate::palette::FAINT),
            )
        };
        let (t, s, i) = (
            btn("todos", "alt+o"),
            btn("skills", "^t"),
            btn("info", "alt+i"),
        );
        let sep = Span::styled(" │ ", crate::palette::BORDER_QUIET_STYLE);
        let span_w = |s: &Span| unicode_width::UnicodeWidthStr::width(s.content.as_ref()) as u16;
        let (tw, sw, iw) = (
            span_w(&t.0) + span_w(&t.1),
            span_w(&s.0) + span_w(&s.1),
            span_w(&i.0) + span_w(&i.1),
        );
        let sep_w: u16 = 3;
        let x0 = chunks[3].x + 1;
        let x1 = x0 + tw + sep_w;
        let x2 = x1 + sw + sep_w;
        let right = match &sidebar.branch {
            Some(b) => format!("{}:{b}", sidebar.cwd),
            None => sidebar.cwd.clone(),
        };
        let w = unicode_width::UnicodeWidthStr::width;
        let used = 1 + (tw + sep_w + sw + sep_w + iw) as usize;
        let gap = (chunks[3].width as usize)
            .saturating_sub(used + w(right.as_str()) + 1)
            .max(1);
        let spans = vec![
            Span::raw(" "),
            t.0,
            t.1,
            sep.clone(),
            s.0,
            s.1,
            sep,
            i.0,
            i.1,
            Span::raw(" ".repeat(gap)),
            Span::styled(right, crate::palette::FAINT),
        ];
        strip_zone.set(Some(StripZones {
            todos: ratatui::layout::Rect {
                x: x0,
                y: chunks[3].y,
                width: tw,
                height: 1,
            },
            skills: ratatui::layout::Rect {
                x: x1,
                y: chunks[3].y,
                width: sw,
                height: 1,
            },
            info: ratatui::layout::Rect {
                x: x2,
                y: chunks[3].y,
                width: iw,
                height: 1,
            },
        }));
        frame.render_widget(
            Paragraph::new(ratatui::text::Line::from(spans))
                .style(ratatui::style::Style::new().bg(crate::palette::BG_PANEL)),
            chunks[3],
        );
    }

    // ── status bar: contextual key hints left, meters right ───────
    let hints: Vec<Span<'static>> = if ask.is_some() {
        hint_spans(&[
            (" ↑↓", "select"),
            (" 1-9", "pick"),
            (" ⏎", "confirm"),
            (" esc", "deny"),
        ])
    } else if picker.is_some() {
        hint_spans(&[
            (" ↑↓", "select"),
            (" 1-4", "jump"),
            (" ⏎", "apply"),
            (" esc", "close"),
        ])
    } else if let Some(open) = modal {
        match open {
            Modal::Session(_) => {
                hint_spans(&[(" type", "filter"), (" ⏎", "switch"), (" esc", "close")])
            }
            Modal::Model(_) => {
                hint_spans(&[(" type", "filter"), (" ⏎", "select"), (" esc", "back")])
            }
            Modal::Provider(_) => {
                hint_spans(&[(" type", "filter"), (" ⏎", "connect"), (" esc", "close")])
            }
            Modal::Settings(p) if p.edit.is_some() => hint_spans(&[
                (" type", "edit selector"),
                (" ⏎", "apply"),
                (" esc", "cancel"),
            ]),
            Modal::Settings(_) => {
                hint_spans(&[(" ⏎", "edit/cycle"), (" s", "save"), (" esc", "close")])
            }
            Modal::Spills { .. } => {
                hint_spans(&[(" ↑↓", "choose"), (" ⏎", "open"), (" esc", "close")])
            }
            Modal::Prompts { .. } => hint_spans(&[
                (" ↑↓", "choose"),
                (" ⏎", "run / fill args"),
                (" esc", "close"),
            ]),
            Modal::Tree { .. } => {
                hint_spans(&[(" ↑↓", "choose"), (" ⏎", "attach"), (" esc", "close")])
            }
            Modal::Rewind { .. } => hint_spans(&[
                (" ↑↓", "choose"),
                (" ⏎", "rewind"),
                (" e", "edit"),
                (" esc", "close"),
            ]),
            Modal::Memory { inbox, .. } => {
                if inbox.is_empty() {
                    hint_spans(&[(" esc", "close")])
                } else {
                    hint_spans(&[
                        (" ↑↓", "choose"),
                        (" ⏎", "project"),
                        (" u", "user"),
                        (" d", "discard"),
                        (" esc", "close"),
                    ])
                }
            }
            Modal::Usage { .. } => hint_spans(&[(" any", "close")]),
            Modal::Todos { .. } | Modal::Skills { .. } | Modal::Info { .. } => {
                hint_spans(&[(" any", "close")])
            }
            Modal::Context { .. } => hint_spans(&[(" esc", "close")]),
            Modal::Tasks { .. } => {
                hint_spans(&[(" ↑↓", "choose"), (" ⏎", "page result"), (" esc", "close")])
            }
            Modal::TaskDetail { .. } | Modal::Debug { .. } => hint_spans(&[
                (" pgup/pgdn", "scroll"),
                (" home/end", "top/tail"),
                (" esc", "close"),
            ]),
            Modal::Key(_) => hint_spans(&[(" type", "value"), (" ⏎", "save"), (" esc", "cancel")]),
            Modal::Help => hint_spans(&[(" ⏎", "close")]),
        }
    } else if popup.is_some() {
        hint_spans(&[(" ↑↓", "select"), (" tab", "complete"), (" esc", "close")])
    } else if path.is_some() {
        hint_spans(&[(" ↑↓", "choose"), (" tab", "accept"), (" esc", "close")])
    } else if rsearch.is_some() {
        hint_spans(&[(" ctrl+r", "next"), (" ⏎", "accept"), (" esc", "cancel")])
    } else if busy {
        hint_spans(&[(" enter", "interject"), (" +", "defer"), (" esc", "abort")])
    } else if mouse_captured {
        hint_spans(&[
            (" enter", "send"),
            (" /", "commands"),
            (" ⇧drag", "select"),
            (" ^p/^n", "history"),
        ])
    } else {
        hint_spans(&[
            (" enter", "send"),
            (" /", "commands"),
            (" drag", "select · ↑↓ scroll"),
            (" ^p/^n", "history"),
        ])
    };
    let right = status_right(meters);
    let w = unicode_width::UnicodeWidthStr::width;
    let left_cols: usize = hints.iter().map(|s| w(s.content.as_ref())).sum();
    let right_cols: usize = right.iter().map(|s| w(s.content.as_ref())).sum();
    // one leading + one trailing col of air: the left zone starts a col
    // in, the right zone ends a col before the chunk edge
    let pad = chunks[4]
        .width
        .saturating_sub(2)
        .saturating_sub((left_cols + right_cols) as u16)
        .max(1) as usize;
    let mut bar: Vec<Span<'static>> = vec![Span::raw(" ")];
    bar.extend(hints);
    bar.push(Span::raw(" ".repeat(pad)));
    bar.extend(right);
    bar.push(Span::raw(" "));
    frame.render_widget(Paragraph::new(TuiLine::from(bar)), chunks[4]);

    // ── slash autocomplete popup (above input) ────────────────────
    if let Some(popup) = popup {
        // border(2) + up to 7 items (hints live in the status bar)
        let rows = popup.items.len().min(7) as u16 + 2;
        let rect = ratatui::layout::Rect {
            x: chunks[2].x,
            y: chunks[2].y.saturating_sub(rows),
            width: (chunks[2].width).min(56),
            height: rows,
        };
        // wipe the covered cells: the paragraph paints only its text
        frame.render_widget(ratatui::widgets::Clear, rect);
        let inner_w = rect.width.saturating_sub(4) as usize; // borders + padding
        let mut text = Vec::new();
        // window the list so the selection always stays visible: the
        // filter can match far more commands than the 7 shown rows
        const CAP: usize = 7;
        let offset = popup
            .selected
            .saturating_sub(CAP - 1)
            .min(popup.items.len().saturating_sub(CAP));
        for (i, (name, desc)) in popup.items.iter().skip(offset).take(CAP).enumerate() {
            let desc_trim: String = desc.chars().take(32).collect();
            let row = format!("{name:<12} {desc_trim}");
            if i + offset == popup.selected {
                text.push(TuiLine::styled(
                    pad_to_width(row, inner_w),
                    selection_style(),
                ));
            } else {
                text.push(TuiLine::raw(row));
            }
        }
        let widget = Paragraph::new(text)
            .block(modal_frame("commands").padding(ratatui::widgets::Padding::horizontal(1)))
            .style(ratatui::style::Style::new().bg(crate::palette::BG_SURFACE))
            .wrap(Wrap { trim: false });
        frame.render_widget(widget, rect);
    }

    // ── path completion popup (above input) ────────────────────────
    if let Some(path) = path {
        // border(2) + up to 7 entries (hints live in the status bar)
        let rows = path.entries.len().min(7) as u16 + 2;
        let rect = ratatui::layout::Rect {
            x: chunks[2].x,
            y: chunks[2].y.saturating_sub(rows),
            width: (chunks[2].width).min(56),
            height: rows,
        };
        // wipe the covered cells: the paragraph paints only its text
        frame.render_widget(ratatui::widgets::Clear, rect);
        let inner_w = rect.width.saturating_sub(4) as usize; // borders + padding
        let mut text = Vec::new();
        // window the list so the selection always stays visible
        const CAP: usize = 7;
        let offset = path
            .selected
            .saturating_sub(CAP - 1)
            .min(path.entries.len().saturating_sub(CAP));
        for (i, (name, is_dir)) in path.entries.iter().skip(offset).take(CAP).enumerate() {
            let slash = if *is_dir { "/" } else { "" };
            if i + offset == path.selected {
                text.push(TuiLine::styled(
                    pad_to_width(format!("{name}{slash}"), inner_w),
                    selection_style(),
                ));
            } else if *is_dir {
                text.push(TuiLine::from(vec![
                    Span::raw(name.clone()),
                    Span::styled("/", crate::palette::META),
                ]));
            } else {
                text.push(TuiLine::raw(name.clone()));
            }
        }
        let widget = Paragraph::new(text)
            .block(
                modal_frame(if path.mentions { "files" } else { "path" })
                    .padding(ratatui::widgets::Padding::horizontal(1)),
            )
            .style(ratatui::style::Style::new().bg(crate::palette::BG_SURFACE))
            .wrap(Wrap { trim: false });
        frame.render_widget(widget, rect);
    }

    // ── modals (session picker / settings) ────────────────────────
    if let Some(open) = modal {
        match open {
            Modal::Session(picker) => {
                let rows = picker.rows();
                // border(2) + vertical padding(2) + filter row(1)
                let height = (rows.len() as u16 + 5).min(22);
                let width = 68.min(frame.area().width);
                let rect = centered(width, height, modal_area);
                // wipe the covered cells first: the paragraph only paints
                // its own text, and the transcript would bleed through
                frame.render_widget(ratatui::widgets::Clear, rect);
                let mut text = vec![TuiLine::from(vec![
                    Span::styled("filter: ", ratatui::style::Style::default()),
                    Span::styled(picker.filter.clone(), crate::palette::ACCENT_STYLE),
                ])];
                let inner_w = width.saturating_sub(4) as usize; // borders + padding
                let cap = (height as usize).saturating_sub(5);
                let offset = picker
                    .selected
                    .saturating_sub(cap.saturating_sub(1))
                    .min(rows.len().saturating_sub(cap));
                for (i, (label, detail)) in rows.iter().skip(offset).take(cap).enumerate() {
                    let row = pad_to_width(format!("{label}  —  {detail}"), inner_w);
                    if i + offset == picker.selected {
                        text.push(TuiLine::styled(row, selection_style()));
                    } else {
                        text.push(TuiLine::raw(row));
                    }
                }
                let widget = Paragraph::new(text)
                    .block(modal_frame("sessions"))
                    .style(ratatui::style::Style::new().bg(crate::palette::BG_SURFACE))
                    .wrap(Wrap { trim: false });
                frame.render_widget(widget, rect);
            }
            Modal::Key(prompt) => {
                // full-frame repaint: switching here from the model picker
                // must leave no stale pixels outside the prompt rect; the
                // canvas (not a bare wipe) keeps the warm ground intact
                frame.render_widget(Block::new().style(crate::palette::CANVAS), frame.area());
                let height = 10u16.min(frame.area().height.saturating_sub(2));
                let width = 68.min(frame.area().width);
                let rect = centered(width, height, modal_area);
                // wipe the covered cells first: the paragraph only paints
                // its own text, and the transcript would bleed through
                frame.render_widget(ratatui::widgets::Clear, rect);
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
                    // "get one   " (10 cols) + doc must fit the padded inner width
                    let doc: String = prompt
                        .doc_url
                        .chars()
                        .take(width.saturating_sub(14) as usize)
                        .collect();
                    text.push(TuiLine::from(vec![
                        Span::styled("get one   ", crate::palette::META),
                        Span::styled(doc, crate::palette::TOOL),
                    ]));
                }
                text.push(TuiLine::default());
                let masked = "•".repeat(prompt.input.chars().count());
                text.push(TuiLine::from(vec![
                    Span::styled("value     ", crate::palette::META),
                    Span::styled(masked, Style::default()),
                    Span::styled("▌", crate::palette::ACCENT_STYLE),
                ]));
                let widget = Paragraph::new(text)
                    .block(modal_frame("api key"))
                    .style(ratatui::style::Style::new().bg(crate::palette::BG_SURFACE))
                    .wrap(Wrap { trim: false });
                frame.render_widget(widget, rect);
            }
            Modal::Help => {
                let height = 34u16.min(frame.area().height.saturating_sub(2));
                let width = 80.min(frame.area().width);
                let rect = centered(width, height, modal_area);
                // wipe the covered cells first: the paragraph only paints
                // its own text, and the transcript would bleed through
                frame.render_widget(ratatui::widgets::Clear, rect);
                let text = help_modal_rows();
                let widget = Paragraph::new(text)
                    .block(modal_frame("help"))
                    .style(ratatui::style::Style::new().bg(crate::palette::BG_SURFACE))
                    .wrap(Wrap { trim: false });
                frame.render_widget(widget, rect);
            }
            Modal::Model(picker) => {
                let rows = picker.rows();
                // border(2) + vertical padding(2) + filter row(1)
                let height = (rows.len() as u16 + 5).min(20);
                let width = 68.min(frame.area().width);
                let rect = centered(width, height, modal_area);
                // wipe the covered cells first: the paragraph only paints
                // its own text, and the transcript would bleed through
                frame.render_widget(ratatui::widgets::Clear, rect);
                let mut text = vec![TuiLine::from(vec![
                    Span::styled("filter: ", ratatui::style::Style::default()),
                    Span::styled(picker.filter.clone(), crate::palette::ACCENT_STYLE),
                ])];
                let cap = (height as usize).saturating_sub(5);
                let inner_w = width.saturating_sub(4) as usize; // borders + padding
                // the unified list splits configured from unconfigured
                // models with one dim separator row (the same predicate
                // rows() partitions on)
                let unified = picker.unified();
                let split = if unified {
                    rows.iter()
                        .take_while(|m| ModelPicker::configured(m))
                        .count()
                } else {
                    rows.len()
                };
                let mut shown = 0usize;
                for (i, m) in rows.iter().enumerate() {
                    if shown >= cap {
                        break;
                    }
                    if unified && i == split && i > 0 {
                        text.push(TuiLine::styled("─ not configured ─", crate::palette::META));
                        shown += 1;
                        if shown >= cap {
                            break;
                        }
                    }
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
                    text.push(TuiLine::styled(
                        pad_to_width(model_row(m, &ctx, &key), inner_w),
                        if i == picker.selected {
                            selection_style()
                        } else {
                            ratatui::style::Style::default()
                        },
                    ));
                    shown += 1;
                }
                if rows.is_empty() {
                    if picker.configured_only && picker.filter.trim().is_empty() {
                        text.push(TuiLine::styled(
                            "no configured providers — /provider to connect",
                            crate::palette::WARN,
                        ));
                    } else if picker.filter.trim().is_empty() {
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
                let widget = Paragraph::new(text)
                    .block(modal_frame(
                        picker
                            .vendor
                            .as_ref()
                            .map_or("model".to_string(), |v| format!("model · {v}")),
                    ))
                    .style(ratatui::style::Style::new().bg(crate::palette::BG_SURFACE))
                    .wrap(Wrap { trim: false });
                frame.render_widget(widget, rect);
            }
            Modal::Provider(picker) => {
                let rows = picker.rows();
                // border(2) + vertical padding(2) + filter row(1)
                let height = (rows.len() as u16 + 5).min(20);
                let width = 68.min(frame.area().width);
                let rect = centered(width, height, modal_area);
                // wipe the covered cells first: the paragraph only paints
                // its own text, and the transcript would bleed through
                frame.render_widget(ratatui::widgets::Clear, rect);
                let inner_w = width.saturating_sub(4) as usize; // borders + padding
                let mut text = vec![TuiLine::from(vec![
                    Span::styled("filter: ", ratatui::style::Style::default()),
                    Span::styled(picker.filter.clone(), crate::palette::ACCENT_STYLE),
                ])];
                let cap = (height as usize).saturating_sub(5);
                for (i, (p, detail)) in rows.iter().take(cap).enumerate() {
                    let name: String = p.name.chars().take(16).collect();
                    let detail: String = detail.chars().take(44).collect();
                    let key_style = if p.key_set || p.env_var.is_empty() {
                        crate::palette::OK
                    } else {
                        crate::palette::ERR
                    };
                    if i == picker.selected {
                        text.push(TuiLine::from(vec![Span::styled(
                            pad_to_width(format!("{name:<16} {detail}"), inner_w),
                            selection_style(),
                        )]));
                    } else {
                        text.push(TuiLine::from(vec![
                            Span::raw(format!("{name:<16} ")),
                            Span::styled(detail, key_style),
                        ]));
                    }
                }
                if rows.is_empty() {
                    text.push(TuiLine::styled(
                        "(no providers match)",
                        crate::palette::META,
                    ));
                }
                let widget = Paragraph::new(text)
                    .block(modal_frame("providers"))
                    .style(ratatui::style::Style::new().bg(crate::palette::BG_SURFACE))
                    .wrap(Wrap { trim: false });
                frame.render_widget(widget, rect);
            }
            Modal::Settings(panel) => {
                // border(2) + vertical padding(2) + the ROWS/provider rows
                let height = (SettingsPanel::ROWS + panel.providers.len() + 7) as u16;
                let height = height.min(frame.area().height.saturating_sub(2));
                let width = 80.min(frame.area().width);
                let rect = centered(width, height, modal_area);
                // wipe the covered cells first: the paragraph only paints
                // its own text, and the transcript would bleed through
                frame.render_widget(ratatui::widgets::Clear, rect);
                let dim = ratatui::style::Style::new().fg(crate::palette::META);
                let inner_w = width.saturating_sub(4) as usize; // borders + padding
                let mode_str = mode_label(panel.mode);
                let effort_str = panel
                    .effort
                    .as_ref()
                    .map(|e| format!("{e:?}").to_lowercase())
                    .unwrap_or_else(|| "(default)".to_string());
                let editing = panel.edit.is_some();
                // while editing, the row shows a cursor block so the mode
                // change is unmistakable (nothing else on screen moves)
                let model_label = match &panel.edit {
                    Some(buf) => format!("model ✎  {buf}▌"),
                    None => format!("model    {}", panel.model),
                };
                let model_style = if editing {
                    ratatui::style::Style::new()
                        .fg(crate::palette::WARN)
                        .add_modifier(Modifier::BOLD)
                } else if panel.selected == 0 {
                    selection_style()
                } else {
                    dim
                };
                let mut text = vec![
                    TuiLine::styled(pad_to_width(model_label, inner_w), model_style),
                    TuiLine::styled(
                        pad_to_width(format!("mode     {mode_str}"), inner_w),
                        if panel.selected == 1 {
                            selection_style()
                        } else {
                            dim
                        },
                    ),
                    TuiLine::styled(
                        pad_to_width(format!("effort   {effort_str}"), inner_w),
                        if panel.selected == 2 {
                            selection_style()
                        } else {
                            dim
                        },
                    ),
                    TuiLine::styled(
                        format!("config: {}", panel.config_path),
                        crate::palette::META,
                    ),
                ];
                text.push(TuiLine::styled(
                    "providers:",
                    ratatui::style::Style::default(),
                ));
                let url_room = (width.saturating_sub(40)) as usize;
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
                        format!("… +{} more", rest.len()),
                        crate::palette::META,
                    ));
                }
                let widget = Paragraph::new(text)
                    .block(modal_frame("settings"))
                    .style(ratatui::style::Style::new().bg(crate::palette::BG_SURFACE))
                    .wrap(Wrap { trim: false });
                frame.render_widget(widget, rect);
            }
            Modal::Spills { items, selected } => {
                // border(2) + vertical padding(2) + list rows
                let height = (items.len() as u16 + 5).clamp(7, 21);
                let width = 68.min(frame.area().width);
                let rect = centered(width, height, modal_area);
                // wipe the covered cells first: the paragraph only paints
                // its own text, and the transcript would bleed through
                frame.render_widget(ratatui::widgets::Clear, rect);
                let inner_w = width.saturating_sub(4) as usize; // borders + padding
                let mut text = Vec::new();
                if items.is_empty() {
                    text.push(TuiLine::styled(
                        "(no spill files yet — full tool output lands here)",
                        crate::palette::META,
                    ));
                }
                let cap = (height as usize).saturating_sub(5);
                let offset = (*selected)
                    .saturating_sub(cap.saturating_sub(1))
                    .min(items.len().saturating_sub(cap));
                for (i, path) in items.iter().skip(offset).take(cap).enumerate() {
                    if i + offset == *selected {
                        text.push(TuiLine::styled(
                            pad_to_width(path.clone(), inner_w),
                            selection_style(),
                        ));
                    } else {
                        text.push(TuiLine::raw(path.clone()));
                    }
                }
                let widget = Paragraph::new(text)
                    .block(modal_frame("spills"))
                    .style(ratatui::style::Style::new().bg(crate::palette::BG_SURFACE))
                    .wrap(Wrap { trim: false });
                frame.render_widget(widget, rect);
            }
            Modal::Prompts { items, selected } => {
                let height = (items.len() as u16 + 5).clamp(7, 21);
                let width = 68.min(frame.area().width);
                let rect = centered(width, height, modal_area);
                // wipe the covered cells first: the paragraph only paints
                // its own text, and the transcript would bleed through
                frame.render_widget(ratatui::widgets::Clear, rect);
                let inner_w = width.saturating_sub(4) as usize;
                let mut text = Vec::new();
                if items.is_empty() {
                    text.push(TuiLine::styled(
                        "(no MCP prompts advertised)",
                        crate::palette::META,
                    ));
                }
                let cap = (height as usize).saturating_sub(5);
                let offset = (*selected)
                    .saturating_sub(cap.saturating_sub(1))
                    .min(items.len().saturating_sub(cap));
                for (i, row) in items.iter().skip(offset).take(cap).enumerate() {
                    if i + offset == *selected {
                        text.push(TuiLine::styled(
                            pad_to_width(row.clone(), inner_w),
                            selection_style(),
                        ));
                    } else {
                        text.push(TuiLine::raw(row.clone()));
                    }
                }
                let widget = Paragraph::new(text)
                    .block(modal_frame("prompts"))
                    .style(ratatui::style::Style::new().bg(crate::palette::BG_SURFACE))
                    .wrap(Wrap { trim: false });
                frame.render_widget(widget, rect);
            }
            Modal::Tasks { entries, selected } => {
                let height = (entries.len() as u16 + 5).clamp(7, 21);
                let width = 90.min(frame.area().width);
                let rect = centered(width, height, modal_area);
                frame.render_widget(ratatui::widgets::Clear, rect);
                let inner_w = width.saturating_sub(4) as usize;
                let mut text = Vec::new();
                if entries.is_empty() {
                    text.push(TuiLine::styled(
                        "(no background tasks or jobs)".to_string(),
                        crate::palette::META,
                    ));
                }
                let cap = (height as usize).saturating_sub(5);
                let offset = (*selected)
                    .saturating_sub(cap.saturating_sub(1))
                    .min(entries.len().saturating_sub(cap));
                for (i, (_, row)) in entries.iter().enumerate().skip(offset).take(cap) {
                    if i == *selected {
                        text.push(TuiLine::styled(
                            pad_to_width(row.clone(), inner_w),
                            selection_style(),
                        ));
                    } else {
                        text.push(TuiLine::raw(row.clone()));
                    }
                }
                let widget = Paragraph::new(text)
                    .block(modal_frame("tasks — ⏎ pages a task result"))
                    .style(ratatui::style::Style::new().bg(crate::palette::BG_SURFACE))
                    .wrap(Wrap { trim: false });
                frame.render_widget(widget, rect);
            }
            Modal::TaskDetail { id, text, scroll } => {
                let lines: Vec<&str> = text.lines().collect();
                // clamp to the transcript pane: a taller box would bleed
                // over the input box / status bar
                let height = (lines.len() as u16 + 5).clamp(7, 30).min(modal_area.height);
                let width = 90.min(frame.area().width);
                let rect = centered(width, height, modal_area);
                frame.render_widget(ratatui::widgets::Clear, rect);
                let visible = (height as usize).saturating_sub(5);
                let (start, _) = window_range(lines.len(), visible, *scroll);
                let mut body = Vec::new();
                for line in &lines[start..(start + visible).min(lines.len())] {
                    body.push(TuiLine::raw((*line).to_string()));
                }
                let widget = Paragraph::new(body)
                    .block(modal_frame(format!("task t-{id}")))
                    .style(ratatui::style::Style::new().bg(crate::palette::BG_SURFACE))
                    .wrap(Wrap { trim: false });
                frame.render_widget(widget, rect);
            }
            Modal::Debug { rows, scroll } => {
                // clamp to the transcript pane: a taller box would bleed
                // over the input box / status bar
                let height = (rows.len() as u16 + 5).clamp(7, 30).min(modal_area.height);
                let width = 90.min(frame.area().width);
                let rect = centered(width, height, modal_area);
                frame.render_widget(ratatui::widgets::Clear, rect);
                let visible = (height as usize).saturating_sub(5);
                let (start, _) = window_range(rows.len(), visible, *scroll);
                let mut body = Vec::new();
                for row in &rows[start..(start + visible).min(rows.len())] {
                    body.push(TuiLine::styled(row.clone(), crate::palette::META));
                }
                let widget = Paragraph::new(body)
                    .block(modal_frame("debug sessions"))
                    .style(ratatui::style::Style::new().bg(crate::palette::BG_SURFACE))
                    .wrap(Wrap { trim: false });
                frame.render_widget(widget, rect);
            }
            Modal::Rewind { items, selected } => {
                let height = (items.len() as u16 + 5).clamp(6, 22);
                let width = 76.min(frame.area().width);
                let rect = centered(width, height, modal_area);
                frame.render_widget(ratatui::widgets::Clear, rect);
                let inner_w = width.saturating_sub(4) as usize;
                let mut text = vec![TuiLine::styled(
                    pad_to_width(
                        "pick a message — ⏎/r rewind here · e edit & resend · esc close"
                            .to_string(),
                        inner_w,
                    ),
                    crate::palette::META,
                )];
                let cap = (height as usize).saturating_sub(5);
                for (i, (turns, prompt)) in items.iter().enumerate().take(cap.max(1)) {
                    let mut row = format!("[−{turns}] {prompt}");
                    if row.chars().count() > inner_w {
                        row = row
                            .chars()
                            .take(inner_w.saturating_sub(1))
                            .collect::<String>()
                            + "…";
                    }
                    // the one selection idiom: the full-row pink bar
                    let style = if i == *selected {
                        selection_style()
                    } else {
                        ratatui::style::Style::new().fg(crate::palette::META)
                    };
                    text.push(TuiLine::styled(pad_to_width(row, inner_w), style));
                }
                let widget = Paragraph::new(text)
                    .block(modal_frame("rewind"))
                    .style(ratatui::style::Style::new().bg(crate::palette::BG_SURFACE))
                    .wrap(Wrap { trim: false });
                frame.render_widget(widget, rect);
            }
            Modal::Memory {
                rows,
                inbox,
                selected,
            } => {
                let extra = if inbox.is_empty() { 0 } else { inbox.len() + 2 };
                let height = (rows.len() as u16 + extra as u16 + 4).clamp(6, 24);
                let width = 80.min(frame.area().width);
                let rect = centered(width, height, modal_area);
                // wipe the covered cells first: the paragraph only paints
                // its own text, and the transcript would bleed through
                frame.render_widget(ratatui::widgets::Clear, rect);
                let inner_w = width.saturating_sub(4) as usize;
                let mut text = Vec::new();
                let cap = (height as usize).saturating_sub(4);
                let mut budget = cap;
                for row in rows.iter() {
                    if budget == 0 {
                        break;
                    }
                    budget -= 1;
                    if row.starts_with('▸') {
                        text.push(TuiLine::styled(
                            pad_to_width(row.clone(), inner_w),
                            crate::palette::META,
                        ));
                    } else {
                        text.push(TuiLine::raw(row.clone()));
                    }
                }
                if !inbox.is_empty() && budget >= 2 {
                    budget -= 2;
                    text.push(TuiLine::raw(String::new()));
                    text.push(TuiLine::styled(
                        "staged memories — ⏎ accept→MEMORY.md · u→user · d discard".to_string(),
                        crate::palette::META,
                    ));
                    for (i, line) in inbox.iter().enumerate() {
                        if budget == 0 {
                            break;
                        }
                        budget -= 1;
                        let style = if i == *selected {
                            selection_style()
                        } else {
                            ratatui::style::Style::new().fg(crate::palette::META)
                        };
                        text.push(TuiLine::styled(pad_to_width(line.clone(), inner_w), style));
                    }
                }
                let widget = Paragraph::new(text)
                    .block(modal_frame("memory"))
                    .style(ratatui::style::Style::new().bg(crate::palette::BG_SURFACE))
                    .wrap(Wrap { trim: false });
                frame.render_widget(widget, rect);
            }
            Modal::Todos { rows } | Modal::Skills { rows } | Modal::Info { rows } => {
                let (title, rows) = match open {
                    Modal::Todos { rows } => ("todos", rows),
                    Modal::Skills { rows } => ("skills · agents · mcp", rows),
                    _ => ("info", rows),
                };
                let height = (rows.len() as u16 + 4).clamp(6, 24);
                let width = 80.min(frame.area().width);
                let rect = centered(width, height, modal_area);
                frame.render_widget(ratatui::widgets::Clear, rect);
                let inner_w = width.saturating_sub(4) as usize;
                let mut text = Vec::new();
                let cap = (height as usize).saturating_sub(4);
                for row in rows.iter().take(cap) {
                    let plain: String = row.spans.iter().map(|s| s.content.clone()).collect();
                    // section headers keep their accent; everything else
                    // renders plain
                    if plain == "skills" || plain == "agents" || plain == "mcp" {
                        text.push(TuiLine::styled(
                            pad_to_width(plain, inner_w),
                            crate::palette::META,
                        ));
                    } else {
                        text.push(TuiLine::from(
                            row.spans
                                .iter()
                                .map(|s| Span::styled(s.content.clone(), s.style))
                                .collect::<Vec<_>>(),
                        ));
                    }
                }
                let more = rows.len().saturating_sub(cap);
                if more > 0 {
                    text.push(TuiLine::styled(
                        format!("… +{more} more"),
                        crate::palette::META,
                    ));
                }
                let widget = Paragraph::new(text)
                    .block(modal_frame(title))
                    .style(ratatui::style::Style::new().bg(crate::palette::BG_SURFACE))
                    .wrap(Wrap { trim: false });
                frame.render_widget(widget, rect);
            }
            Modal::Usage { rows } => {
                let height = (rows.len() as u16 + 4).clamp(6, 24);
                let width = 80.min(frame.area().width);
                let rect = centered(width, height, modal_area);
                // wipe the covered cells first: the paragraph only paints
                // its own text, and the transcript would bleed through
                frame.render_widget(ratatui::widgets::Clear, rect);
                let inner_w = width.saturating_sub(4) as usize;
                let mut text = Vec::new();
                let cap = (height as usize).saturating_sub(4);
                for row in rows.iter().take(cap) {
                    if row.starts_with('▸') {
                        text.push(TuiLine::styled(
                            pad_to_width(row.clone(), inner_w),
                            crate::palette::META,
                        ));
                    } else {
                        text.push(TuiLine::raw(row.clone()));
                    }
                }
                let widget = Paragraph::new(text)
                    .block(modal_frame("usage"))
                    .style(ratatui::style::Style::new().bg(crate::palette::BG_SURFACE))
                    .wrap(Wrap { trim: false });
                frame.render_widget(widget, rect);
            }
            Modal::Context { rows } => {
                let height = (rows.len() as u16 + 4).clamp(6, 24);
                let width = 80.min(frame.area().width);
                let rect = centered(width, height, modal_area);
                // wipe the covered cells first: the paragraph only paints
                // its own text, and the transcript would bleed through
                frame.render_widget(ratatui::widgets::Clear, rect);
                let inner_w = width.saturating_sub(4) as usize;
                let mut text = Vec::new();
                let cap = (height as usize).saturating_sub(4);
                for row in rows.iter().take(cap) {
                    if row.starts_with('▸') {
                        text.push(TuiLine::styled(
                            pad_to_width(row.clone(), inner_w),
                            crate::palette::META,
                        ));
                    } else {
                        text.push(TuiLine::raw(row.clone()));
                    }
                }
                let widget = Paragraph::new(text)
                    .block(modal_frame("context"))
                    .style(ratatui::style::Style::new().bg(crate::palette::BG_SURFACE))
                    .wrap(Wrap { trim: false });
                frame.render_widget(widget, rect);
            }
            Modal::Tree {
                items,
                targets: _,
                selected,
            } => {
                let height = (items.len() as u16 + 5).clamp(7, 21);
                let width = 68.min(frame.area().width);
                let rect = centered(width, height, modal_area);
                // wipe the covered cells first: the paragraph only paints
                // its own text, and the transcript would bleed through
                frame.render_widget(ratatui::widgets::Clear, rect);
                let inner_w = width.saturating_sub(4) as usize;
                let mut text = Vec::new();
                if items.is_empty() {
                    text.push(TuiLine::styled(
                        "(no related sessions)",
                        crate::palette::META,
                    ));
                }
                let cap = (height as usize).saturating_sub(5);
                // window the list so the selection always stays visible:
                // long trees scroll instead of hiding the selected row
                let offset = (*selected)
                    .saturating_sub(cap.saturating_sub(1))
                    .min(items.len().saturating_sub(cap));
                for (i, row) in items.iter().skip(offset).take(cap).enumerate() {
                    // truncate at the ACTUAL inner width: a narrow
                    // terminal renders a smaller box than the 64-col
                    // build assumption, and wrapping must not come back
                    let row = pad_to_width(trunc_cols(row, inner_w), inner_w);
                    if i + offset == *selected {
                        text.push(TuiLine::styled(row, selection_style()));
                    } else {
                        text.push(TuiLine::raw(row));
                    }
                }
                let widget = Paragraph::new(text)
                    .block(modal_frame("tree"))
                    .style(ratatui::style::Style::new().bg(crate::palette::BG_SURFACE))
                    .wrap(Wrap { trim: false });
                frame.render_widget(widget, rect);
            }
        }
    }

    // ── permission ask: a real dialog, layered above everything ────
    // the safety-critical surface gets the modal treatment: warned
    // border, bold question, budgeted detail, numbered options on the
    // pink selection bar. The input box renders normally underneath.
    if let Some(ask) = ask {
        let width = 80.min(frame.area().width);
        let inner_w = width.saturating_sub(4) as usize;
        let body = ask_modal_body(ask, inner_w);
        let height = (body.len() as u16 + 4).clamp(5, modal_area.height.max(5));
        let rect = centered(width, height, modal_area);
        // wipe the covered cells first: the paragraph only paints
        // its own text, and the transcript would bleed through
        frame.render_widget(ratatui::widgets::Clear, rect);
        let widget = Paragraph::new(body)
            .block(
                modal_frame("permission")
                    .border_style(ratatui::style::Style::new().fg(crate::palette::WARN)),
            )
            .style(ratatui::style::Style::new().bg(crate::palette::BG_SURFACE))
            .wrap(Wrap { trim: false });
        frame.render_widget(widget, rect);
    }

    // ── toast: transient action feedback over the transcript tail ──
    if let Some(msg) = toast {
        use unicode_width::UnicodeWidthStr;
        let w = (msg.width() as u16 + 4)
            .min(tx_area.width.saturating_sub(4))
            .max(1);
        let text = if msg.width() as u16 > w.saturating_sub(4) {
            format!(" {}", trunc_cols(msg, w.saturating_sub(5) as usize))
        } else {
            format!(" {msg}")
        };
        let rect = ratatui::layout::Rect {
            x: tx_area.x + (tx_area.width.saturating_sub(w)) / 2,
            y: tx_area.y + tx_area.height.saturating_sub(1),
            width: w,
            height: 1,
        };
        frame.render_widget(ratatui::widgets::Clear, rect);
        let bar = TuiLine::from(vec![Span::styled(
            pad_to_width(text, w.saturating_sub(1) as usize),
            ratatui::style::Style::new()
                .fg(crate::palette::ACCENT)
                .bg(crate::palette::BG_PANEL),
        )]);
        frame.render_widget(Paragraph::new(bar), rect);
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

/// Horizontal inset (columns) between a message block's background
/// edges and its text — the band reads as a block, characters never
/// touch its borders.
const CARD_MARGIN: usize = 2;

/// Inset surface-filled rows by [`CARD_MARGIN`] columns on both sides:
/// prepends and appends margin spans carrying the band background.
/// Rows must already be surface-filled to exactly `width` columns
/// (empty rows become full-width blanks).
fn inset_rows(rows: &mut [ratatui::text::Line<'static>], width: u16, bg: ratatui::style::Color) {
    let m = CARD_MARGIN.min(width as usize / 4);
    if m == 0 {
        return;
    }
    let style = ratatui::style::Style::new().bg(bg);
    for row in rows.iter_mut() {
        if row.spans.is_empty() {
            *row = surface_blank(width, bg);
            continue;
        }
        row.spans
            .insert(0, ratatui::text::Span::styled(" ".repeat(m), style));
        row.spans
            .push(ratatui::text::Span::styled(" ".repeat(m), style));
    }
}
/// Full-width band for user messages — the only edge-to-edge role
/// (OMP userMsgBg: warm dark, amber prompt glyph, bold text).
fn push_block(out: &mut Vec<ratatui::text::Line<'static>>, text: &str, width: u16) {
    use ratatui::text::Line as TuiLine;
    use ratatui::text::Span;
    // one blank band row above the text: the user card reads as a pad
    // above AND below (the trailing blank below closes the card)
    out.push(surface_blank(width, crate::palette::BG_USER));
    let lead_style = ratatui::style::Style::new()
        .fg(crate::palette::ACCENT)
        .bg(crate::palette::BG_USER);
    let body_style = ratatui::style::Style::new()
        .bg(crate::palette::BG_USER)
        .add_modifier(ratatui::style::Modifier::BOLD);
    // horizontal margin: the band reads as a block, the text never
    // touches its edges
    let m = CARD_MARGIN.min(width as usize / 4);
    let margin_span = Span::styled(" ".repeat(m), body_style);
    let usable = width as usize - 2 * m;
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
                margin_span.clone(),
                Span::styled(lead.to_string(), lead_style),
                Span::styled(segment, body_style),
            ];
            if pad > 0 {
                spans.push(Span::styled(" ".repeat(pad), body_style));
            }
            spans.push(margin_span.clone());
            out.push(TuiLine::from(spans));
            if end >= chars.len() {
                break;
            }
            start = end;
        }
    }
    out.push(surface_blank(width, crate::palette::BG_USER)); // spacing after each block
}

/// Gutter-prefixed rows for ambient roles (thought/note/report): no
/// background, no full-width padding, and no built-in spacing — air
/// between blocks is [`separate_before`]'s job alone.
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
}

/// `/tree` modal rows: the current session's whole family (itself plus
/// every descendant, found by walking `parent` links level by level).
/// Rows come out parent-before-child, indented one step per depth, the
/// current session marked `▸`, each truncated to `width` columns.
/// Returns (rows, strand ids) in the same order.
fn tree_modal_rows(
    all: &[ka_strand::StrandSummary],
    current: Option<&str>,
    width: usize,
) -> (Vec<String>, Vec<String>) {
    let mut ids: Vec<String> = current.iter().map(|c| c.to_string()).collect();
    let mut depths: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    if let Some(c) = current {
        depths.insert(c.to_string(), 0);
    }
    let mut level = ids.clone();
    let mut depth = 0;
    while !level.is_empty() {
        let mut next = Vec::new();
        for s in all {
            if let Some(p) = &s.parent {
                if level.contains(p) && !ids.contains(&s.id) {
                    ids.push(s.id.clone());
                    depths.insert(s.id.clone(), depth + 1);
                    next.push(s.id.clone());
                }
            }
        }
        level = next;
        depth += 1;
    }
    let mut items = Vec::with_capacity(ids.len());
    let mut targets = Vec::with_capacity(ids.len());
    for id in &ids {
        let Some(s) = all.iter().find(|s| &s.id == id) else {
            continue;
        };
        let date = s.ts.get(..10).unwrap_or(&s.ts).to_string();
        let indent = "  ".repeat(*depths.get(id).unwrap_or(&0));
        let marker = if Some(id.as_str()) == current {
            "▸ "
        } else {
            ""
        };
        let row = format!("{indent}{marker}{} · {date} · {} msgs", s.title, s.messages);
        items.push(trunc_cols(&row, width));
        targets.push(id.clone());
    }
    (items, targets)
}

/// The one modal frame: rounded borders on the surface background with
/// the muted padded title — every overlay speaks this dialect, so a
/// corner never changes shape between popups.
fn modal_frame(title: impl Into<String>) -> ratatui::widgets::Block<'static> {
    ratatui::widgets::Block::default()
        .borders(ratatui::widgets::Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .title(padded_title(title))
        .border_style(crate::palette::BORDER_STYLE)
        .padding(ratatui::widgets::Padding::new(1, 1, 1, 1))
        .style(ratatui::style::Style::new().bg(crate::palette::BG_SURFACE))
}

/// Rows of the /help overlay: the full key table, then commands grouped
/// by what they touch. `{key or /name:<14} {desc}` per row.
fn help_modal_rows() -> Vec<ratatui::text::Line<'static>> {
    use ratatui::text::{Line as TuiLine, Span};
    let key_style = crate::palette::ACCENT_STYLE;
    let dim = ratatui::style::Style::default().fg(crate::palette::META);
    let mut text = Vec::new();
    fn section(text: &mut Vec<TuiLine<'static>>, name: &str) {
        text.push(TuiLine::styled(
            format!("▸ {name}"),
            crate::palette::ACCENT_BOLD,
        ));
    }
    fn rows(
        text: &mut Vec<TuiLine<'static>>,
        pairs: &[(&str, &str)],
        key_style: ratatui::style::Style,
    ) {
        for (k, v) in pairs {
            let desc: String = v.chars().take(44).collect();
            text.push(TuiLine::from(vec![
                Span::styled(format!("{k:<14} "), key_style),
                Span::styled(desc.to_string(), ratatui::style::Style::default()),
            ]));
        }
    }
    section(&mut text, "keys");
    let keys: &[(&str, &str)] = &[
        ("enter", "send · interject mid-turn"),
        ("⇧⏎ / ctrl+j", "newline in the draft"),
        ("+text", "defer until the turn ends"),
        ("!cmd", "run a shell command directly"),
        ("esc", "close popups & modals · unpin scroll"),
        ("esc (busy)", "abort the running turn"),
        ("esc esc", "rewind menu (empty input)"),
        ("ctrl+c", "quit · clear draft · twice to exit"),
        ("↑ ↓", "prompt history · navigate pickers"),
        ("ctrl+p / ctrl+n", "prompt history"),
        ("ctrl+q", "recall the last queued item"),
        ("ctrl+r", "search the transcript"),
        ("ctrl+o", "expand/collapse the last tool call"),
        ("ctrl+m", "toggle mouse capture ⇄ native"),
        ("ctrl+t / alt+o / alt+i", "popups: skills · todos · info"),
        ("alt+t", "fold/unfold thinking"),
        ("alt+e", "edit the draft in $EDITOR"),
        ("ctrl+↑ / ctrl+↓", "jump between your messages"),
        ("pgup / pgdn", "scroll the transcript"),
        ("tab", "complete slash command or path"),
        ("1-9", "pick a row in pickers & asks"),
        ("ctrl+l", "clear screen"),
        ("ctrl+u/k/w · y · z", "kill to start/end/word · yank · undo"),
    ];
    rows(&mut text, keys, key_style);
    text.push(TuiLine::default());
    section(&mut text, "session");
    rows(
        &mut text,
        &[
            ("/session", "pick a session to resume"),
            ("/resume", "alias for /session"),
            ("/new", "start a fresh session"),
            ("/fork", "fork into a copy [turns to drop]"),
            ("/tree", "current session's fork tree"),
            ("/rewind", "drop the last N exchanges"),
            ("/undo", "restore the last edited file"),
            ("/checkpoint", "snapshot the working tree (git)"),
            ("/restore", "restore a checkpoint [id | list]"),
            ("/export", "save this session to markdown [path]"),
            ("/compact", "digest the context now"),
            ("/retry", "resend the last prompt"),
        ],
        key_style,
    );
    text.push(TuiLine::default());
    section(&mut text, "model & providers");
    rows(
        &mut text,
        &[
            ("/model", "pick a model"),
            ("/mode", "pick a permission mode"),
            ("/key", "set an api key for the current model"),
            ("/provider", "connect a provider (api key)"),
            ("/settings", "settings & provider status"),
        ],
        key_style,
    );
    text.push(TuiLine::default());
    section(&mut text, "tools & context");
    rows(
        &mut text,
        &[
            ("/tasks", "background tasks — ⏎ pages a result"),
            ("/debug", "live debug sessions"),
            ("/mcp", "refresh MCP tool lists (/mcp refresh)"),
            ("/prompt", "run an MCP prompt"),
            ("/spills", "browse spilled tool output"),
            ("/context", "context usage breakdown"),
            ("/usage", "usage & cost: session + recent"),
            ("/find", "search the transcript: /find <text>"),
            ("/copy", "copy the last reply (OSC52)"),
            ("/clip", "attach a clipboard image"),
            ("/image", "attach an image [path]"),
        ],
        key_style,
    );
    text.push(TuiLine::default());
    section(&mut text, "safety & plans");
    rows(
        &mut text,
        &[
            ("/plan", "research the task, draft the plan"),
            ("/build", "implement the plan file"),
            ("/review", "read-only review of changes [base]"),
            ("/approve", "review the plan file, then build"),
            ("/memory", "show loaded MEMORY.md tiers"),
        ],
        key_style,
    );
    text.push(TuiLine::default());
    section(&mut text, "meta");
    rows(
        &mut text,
        &[
            ("/help", "commands and key bindings"),
            ("/agents", "list available agents"),
            ("/quit", "exit"),
        ],
        key_style,
    );
    text.push(TuiLine::default());
    text.push(TuiLine::from(Span::styled(
        "custom commands: .ka/commands/*.md (project, trust-gated) or \
~/.config/ka/commands/*.md; body supports $ARGUMENTS",
        dim,
    )));
    text
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

/// A block title with one column of air on each side, so titles never
/// kiss the border glyphs or the frame edge.
fn padded_title(title: impl Into<String>) -> ratatui::text::Line<'static> {
    ratatui::text::Line::from(format!(" {} ", title.into())).style(crate::palette::META)
}

/// Jump the viewport anchor to the previous (up) or next (down) user
/// message relative to the window top. Up with nothing above is a
/// no-op; down past the last user message lands on the live tail
/// (scroll = None) unless the view is already pinned there.
fn jump_to_user_message(
    scroll: &mut Option<usize>,
    user_rows: &[usize],
    total: usize,
    visible: usize,
    up: bool,
) {
    if user_rows.is_empty() || visible == 0 {
        return;
    }
    let (start, pinned) = window_range(total, visible, *scroll);
    if up {
        if let Some(&r) = user_rows.iter().rev().find(|&&r| r < start) {
            *scroll = Some(r);
        }
    } else if let Some(&r) = user_rows.iter().find(|&&r| r > start) {
        *scroll = Some(r);
    } else if !pinned {
        *scroll = None;
    }
}

/// Read the system clipboard as text: wayland → X11 → WSL2/Windows.
async fn clipboard_text() -> String {
    // Every probe is capped (a hanging xclip against an unresponsive
    // selection owner must not freeze the event loop).
    const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
    let run = |program: &str, args: &[&str]| {
        tokio::time::timeout(
            TIMEOUT,
            tokio::process::Command::new(program).args(args).output(),
        )
    };
    // WSL2 first: the Windows clipboard lives behind powershell.exe.
    if let Ok(Ok(out)) = run(
        "powershell.exe",
        &["-NoProfile", "-Command", "Get-Clipboard"],
    )
    .await
    {
        if out.status.success() && !out.stdout.is_empty() {
            return String::from_utf8_lossy(&out.stdout)
                .trim_end_matches(['\r', '\n'])
                .to_string();
        }
    }
    for (program, args) in [
        ("wl-paste", vec!["--no-newline"]),
        ("xclip", vec!["-selection", "clipboard", "-o"]),
    ] {
        if let Ok(Ok(out)) = run(program, &args).await {
            if out.status.success() && !out.stdout.is_empty() {
                return String::from_utf8_lossy(&out.stdout)
                    .trim_end_matches(['\r', '\n'])
                    .to_string();
            }
        }
    }
    String::new()
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
    fn safe_mode_blocks_custom_command_execution() {
        // isolated here: ka-term's test binary is its own process, so
        // flipping the bare-mode atomic cannot disturb ka-engine's tests
        let dir = std::env::temp_dir().join(format!("ka-tui-safemode-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".ka/commands")).unwrap();
        std::fs::write(dir.join(".ka/commands/ship.md"), "ship body $ARGUMENTS\n").unwrap();
        let state_home = dir.join("state");
        // trusted so the normal path would load the body
        std::fs::create_dir_all(&state_home).unwrap();
        let trust_file = state_home.join("ka/trust.json");
        std::fs::create_dir_all(trust_file.parent().unwrap()).unwrap();
        ka_engine::trust::save_trust_at(&trust_file, std::slice::from_ref(&dir));
        // sanity: without safe mode the command resolves (explicit
        // paths — no process-global cwd mutation in a parallel test
        // binary)
        let body = custom_command_in(&dir, &state_home, "/ship", Some("v1"));
        assert!(body.is_some(), "trusted project loads the command body");
        // safe mode: execution path refuses even though the popup gate
        // (available_slash_commands) also hides it
        ka_engine::conventions::set_bare_mode(true);
        assert!(
            custom_command("/ship", Some("v1")).is_none(),
            "safe mode must not load custom command bodies"
        );
        assert!(
            !available_slash_commands()
                .iter()
                .any(|(name, _)| name == "cmd:ship"),
            "safe mode must not advertise custom commands"
        );
        ka_engine::conventions::set_bare_mode(false);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn model_picker_filters_picks_and_falls_back_to_custom() {
        let picker = ModelPicker {
            models: vec![
                model("ollama/qwen3-32b", 131_072),
                model("openai/gpt-5.1", 400_000),
                model("anthropic/claude-sonnet-5", 200_000),
            ],
            vendor: None,
            configured_only: false,
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

        // the unified list partitions: configured models lead, the rest
        // follow; order is stable within both groups
        let mut mixed = picker.clone();
        mixed.models[1].key_set = true; // openai connected
        let ids: Vec<&str> = mixed.rows().iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "openai/gpt-5.1",
                "ollama/qwen3-32b",
                "anthropic/claude-sonnet-5"
            ],
            "configured vendor's models lead"
        );
        // the render's separator splits at the same predicate
        assert_eq!(
            mixed
                .rows()
                .iter()
                .take_while(|m| ModelPicker::configured(m))
                .count(),
            1
        );
    }

    #[test]
    fn model_picker_unified_partitions_configured_first() {
        let mut models = vec![
            model("zai/glm-5.3", 200_000),      // keyed, no key → unconfigured
            model("ollama/qwen3-32b", 131_072), // keyless → configured
            model("openai/gpt-5.1", 400_000),   // keyed + key set → configured
            model("anthropic/claude-sonnet-5", 200_000), // keyed, no key
        ];
        models[0].key_env = "ZAI_API_KEY".to_string();
        models[1].key_env = String::new();
        models[2].key_set = true;
        let picker = ModelPicker {
            models,
            vendor: None,
            configured_only: false,
            selected: 0,
            filter: String::new(),
        };
        let ids: Vec<&str> = picker.rows().iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "ollama/qwen3-32b",
                "openai/gpt-5.1",
                "zai/glm-5.3",
                "anthropic/claude-sonnet-5",
            ],
            "configured models lead (stable), unconfigured follow (stable)"
        );
        // the filter spans the whole pool, unconfigured group included
        let mut f = picker.clone();
        f.filter = "glm".to_string();
        assert_eq!(f.pick().as_deref(), Some("zai/glm-5.3"));

        // the vendor-locked drill list never partitions
        let mut locked = picker.clone();
        locked.vendor = Some("zai".to_string());
        let ids: Vec<&str> = locked.rows().iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["zai/glm-5.3"]);
    }

    fn provider(name: &str, env_var: &str, key_set: bool) -> ProviderInfo {
        ProviderInfo {
            name: name.to_string(),
            env_var: env_var.to_string(),
            base_url: String::new(),
            key_set,
        }
    }

    #[test]
    fn provider_picker_counts_filters_and_picks() {
        let models = vec![
            model("anthropic/claude-sonnet-5", 200_000),
            model("anthropic/claude-opus-4", 200_000),
            model("ollama/qwen3-32b", 131_072),
        ];
        let mut picker = ProviderPicker::new(
            vec![
                provider("anthropic", "ANTHROPIC_API_KEY", false),
                provider("ollama", "", false),
            ],
            &models,
        );
        let rows = picker.rows();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].1, "ANTHROPIC_API_KEY ✗ · 2 models");
        assert_eq!(rows[1].1, "keyless · local · 1 model");
        assert_eq!(picker.pick().unwrap().name, "anthropic");

        // the filter matches the env var, not just the name
        picker.filter = "ANTHROPIC_API_KEY".to_string();
        assert_eq!(picker.rows().len(), 1);
        assert_eq!(picker.pick().unwrap().name, "anthropic");

        picker.filter = "OLLAMA".to_string();
        assert_eq!(picker.rows().len(), 1);
        assert_eq!(picker.pick().unwrap().name, "ollama");

        // no match: nothing to connect
        picker.filter = "midscene".to_string();
        assert!(picker.rows().is_empty());
        assert!(picker.pick().is_none());
    }

    #[test]
    fn model_picker_vendor_lock_narrows_rows() {
        let models = vec![
            model("anthropic/claude-sonnet-5", 200_000),
            model("anthropic/claude-opus-4", 200_000),
            model("ollama/qwen3-32b", 131_072),
        ];
        let mut picker = ModelPicker {
            models,
            vendor: Some("anthropic".to_string()),
            configured_only: false,
            selected: 0,
            filter: String::new(),
        };
        assert_eq!(picker.rows().len(), 2);
        assert_eq!(picker.pick().as_deref(), Some("anthropic/claude-sonnet-5"));

        // the filter still applies within the vendor
        picker.filter = "opus".to_string();
        assert_eq!(picker.rows().len(), 1);
        assert_eq!(picker.pick().as_deref(), Some("anthropic/claude-opus-4"));

        // an unmatched filter still falls back to a custom selector
        picker.filter = "anthropic/claude-haiku".to_string();
        assert!(picker.rows().is_empty());
        assert_eq!(picker.pick().as_deref(), Some("anthropic/claude-haiku"));

        // no lock: every model shows (the plain picker's behavior)
        let mut open = picker.clone();
        open.vendor = None;
        open.filter = String::new();
        assert_eq!(open.rows().len(), 3);
    }

    #[test]
    fn model_picker_configured_only_lists_connected_vendors() {
        let mut models = vec![
            model("openai/gpt-5.1", 400_000),
            model("anthropic/claude-sonnet-5", 200_000),
            model("ollama/qwen3-32b", 131_072),
        ];
        models[0].key_env = "OPENAI_API_KEY".to_string(); // keyed, no key
        models[1].key_env = "ANTHROPIC_API_KEY".to_string();
        models[1].key_set = true; // keyed and connected
        models[2].key_env = String::new(); // keyless local

        let mut picker = ModelPicker {
            models,
            vendor: None,
            configured_only: true,
            selected: 0,
            filter: String::new(),
        };
        let ids: Vec<&str> = picker.rows().iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["anthropic/claude-sonnet-5", "ollama/qwen3-32b"],
            "keyed-unset hidden; keyed-set and keyless shown"
        );

        // composes with the vendor lock: openai is unconfigured, so its
        // locked list is empty and Enter has nothing to apply
        picker.vendor = Some("openai".to_string());
        assert!(picker.rows().is_empty());
        assert_eq!(picker.pick(), None);

        // the filter still applies within the configured set
        picker.vendor = None;
        picker.filter = "qwen".to_string();
        assert_eq!(picker.pick().as_deref(), Some("ollama/qwen3-32b"));
    }

    #[test]
    fn mark_key_set_flips_only_matching_entries() {
        let mut models = vec![model("openai/gpt-5.1", 400_000)];
        models[0].key_env = "OPENAI_API_KEY".to_string();
        let mut providers = vec![
            provider("openai", "OPENAI_API_KEY", false),
            provider("zai", "ZAI_API_KEY", false),
        ];
        mark_key_set(&mut models, &mut providers, "OPENAI_API_KEY");
        assert!(models[0].key_set);
        assert!(providers[0].key_set);
        assert!(!providers[1].key_set, "other providers stay as they were");
    }

    #[test]
    fn model_rows_stay_single_line() {
        let long = model(
            "ollama/some-extremely-long-model-name:with-tag-and-more",
            1_000_000,
        );
        let row = model_row(&long, "1000k", " ✗");
        assert!(
            row.chars().count() <= 66,
            "row too wide: {} {}",
            row.chars().count(),
            row
        );
        let short = model("ollama/qwen3.5:9b", 262_144);
        let row = model_row(&short, "262k", "");
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
        assert!(matches!(
            slash_command("/provider"),
            Some(Slash {
                note: None,
                modal: Some(ModalKind::Provider),
                event: None,
                ..
            })
        ));
    }

    #[test]
    fn word_moves_cross_whitespace_runs() {
        // "foo bar  baz"
        let mut b = InputBuffer {
            text: "foo bar  baz".into(),
            cursor: 0,
            ..Default::default()
        };
        b.word_left();
        assert_eq!(b.cursor, 0, "at start: no-op");
        b.word_right();
        assert_eq!(b.cursor, 3, "end of first word");
        b.word_right();
        assert_eq!(b.cursor, 7, "end of second word");
        b.word_right();
        assert_eq!(b.cursor, 12, "end of last word = text end");
        b.word_right();
        assert_eq!(b.cursor, 12, "clamped at end");

        // cursor inside a word: word_left to its start, word_right to its end
        let mut mid = InputBuffer {
            text: "foo bar  baz".into(),
            cursor: 10,
            ..Default::default()
        };
        mid.word_left();
        assert_eq!(mid.cursor, 9, "start of 'baz'");
        mid.word_left();
        assert_eq!(mid.cursor, 4, "start of 'bar'");
        mid.cursor = 5;
        mid.word_right();
        assert_eq!(mid.cursor, 7, "end of 'bar'");
        mid.cursor = 8;
        mid.word_left();
        assert_eq!(mid.cursor, 4, "double space skipped back to 'bar'");
        mid.cursor = 8;
        mid.word_right();
        assert_eq!(mid.cursor, 12, "double space skipped forward past 'baz'");
    }

    #[test]
    fn delete_word_backward_kills_only_the_word_prefix() {
        let mut b = InputBuffer {
            text: "foo bar".into(),
            cursor: 5,
            ..Default::default()
        }; // inside 'bar'
        assert_eq!(b.delete_word_backward().as_deref(), Some("b"));
        assert_eq!(b.text, "foo ar");
        assert_eq!(b.cursor, 4);
        b.cursor = 0;
        assert_eq!(b.delete_word_backward(), None, "at text start: no-op");
        assert_eq!(b.text, "foo ar");
        b.cursor = 6;
        assert_eq!(b.delete_word_backward().as_deref(), Some("ar"));
        assert_eq!(b.text, "foo ");
        assert_eq!(b.cursor, 4);
    }

    #[test]
    fn delete_word_forward_kills_to_next_word_start() {
        let mut b = InputBuffer {
            text: "foo bar  baz".into(),
            cursor: 4,
            ..Default::default()
        }; // start of 'bar'
        assert_eq!(b.delete_word_forward().as_deref(), Some("bar"));
        assert_eq!(b.text, "foo   baz");
        assert_eq!(b.cursor, 4, "cursor unmoved by forward delete");
        assert_eq!(b.delete_word_forward().as_deref(), Some("  baz"));
        assert_eq!(b.text, "foo ");
        assert_eq!(b.cursor, 4);
        assert_eq!(b.delete_word_forward(), None, "at end: no-op");
        assert_eq!(b.text, "foo ");
    }

    #[test]
    fn line_kills_respect_the_current_line_only() {
        // "line one\ntwo three\nfour"
        let mut b = InputBuffer {
            text: "line one\ntwo three\nfour".into(),
            cursor: 15,
            ..Default::default()
        }; // inside 'three'
        assert_eq!(b.delete_to_line_start().as_deref(), Some("two th"));
        assert_eq!(b.text, "line one\nree\nfour");
        assert_eq!(b.cursor, 9, "cursor at line start");
        assert_eq!(b.delete_to_line_start(), None, "already at line start");
        assert_eq!(b.delete_to_line_end().as_deref(), Some("ree"));
        assert_eq!(
            b.text, "line one\n\nfour",
            "line end kill stops at, never eats, \\n"
        );

        let mut tail = InputBuffer {
            text: "aa\nbb".into(),
            cursor: 5,
            ..Default::default()
        }; // end of text
        assert_eq!(tail.delete_to_line_end(), None, "at text end: no-op");
        assert_eq!(tail.text, "aa\nbb");
        tail.cursor = 2; // at the newline: end of line one
        assert_eq!(
            tail.delete_to_line_end(),
            None,
            "at line end before \n: no-op"
        );
    }

    #[test]
    fn line_kill_at_start_deletes_whole_line_content() {
        let mut b = InputBuffer {
            text: "hello".into(),
            cursor: 5,
            ..Default::default()
        };
        assert_eq!(b.delete_to_line_start().as_deref(), Some("hello"));
        assert!(b.text.is_empty());
        assert_eq!(b.cursor, 0);
        b.undo();
        assert_eq!(b.text, "hello");
        assert_eq!(b.cursor, 5);

        let mut fwd = InputBuffer {
            text: "hello world".into(),
            cursor: 0,
            ..Default::default()
        };
        assert_eq!(fwd.delete_to_line_end().as_deref(), Some("hello world"));
        assert!(fwd.text.is_empty());
        fwd.undo();
        assert_eq!(fwd.text, "hello world");
    }

    #[test]
    fn kill_replace_and_yank_roundtrip_multiline() {
        let mut b = InputBuffer::default();
        b.insert_str("aa\nbb\ncc");
        b.cursor = 3; // start of 'bb'
        let del = b.delete_to_line_end();
        b.kill_push(del); // kill "bb"
        assert_eq!(b.text, "aa\n\ncc");
        assert_eq!(b.kill.clone().as_deref(), Some("bb"));
        b.yank();
        assert_eq!(b.text, "aa\nbb\ncc", "yank restores multiline text");
        assert_eq!(b.cursor, 5, "cursor after the yanked text");

        // kills REPLACE the slot (no readline append)
        // kills REPLACE the slot (no readline append)
        let mut r = InputBuffer::default();
        r.insert_str("one two");
        r.cursor = 4;
        let del = r.delete_word_forward();
        r.kill_push(del); // kills "two"
        assert_eq!(r.text, "one ");
        assert_eq!(r.kill.as_deref(), Some("two"));
        r.cursor = 3;
        let del = r.delete_word_backward();
        r.kill_push(del); // kills "one"
        assert_eq!(r.text, " ");
        assert_eq!(r.kill.as_deref(), Some("one"), "new kill replaces the old");
        // empty kill never clobbers
        r.cursor = 0;
        let del = r.delete_to_line_start();
        r.kill_push(del); // already at line start
        assert_eq!(r.kill.as_deref(), Some("one"), "no-op kill keeps the slot");

        let mut empty = InputBuffer::default();
        empty.yank(); // kill = None: silent no-op
        assert!(empty.text.is_empty());
    }

    #[test]
    fn clear_draft_is_undoable_and_yankable() {
        let mut b = InputBuffer::default();
        b.insert_str("long draft");
        b.clear_draft();
        assert!(b.text.is_empty(), "draft cleared");
        b.undo();
        assert_eq!(b.text, "long draft", "ctrl+z restores the cleared draft");
        assert_eq!(b.cursor, 10);
        // the cleared text also landed in the kill ring (ctrl+y yanks)
        b.clear_draft();
        b.yank();
        assert_eq!(b.text, "long draft", "ctrl+y yanks the killed draft");
    }

    #[test]
    fn undo_restores_pre_op_snapshots_across_ops() {
        let mut b = InputBuffer::default();
        b.insert('a');
        b.insert('b');
        b.newline();
        b.insert_str("cd");
        assert_eq!(b.text, "ab\ncd");
        b.undo();
        assert_eq!(b.text, "ab\n", "undo insert_str");
        b.undo();
        assert_eq!(b.text, "ab", "undo newline");
        b.undo();
        assert_eq!(b.text, "a", "undo insert b");
        b.undo();
        assert_eq!(b.text, "", "undo insert a");
        b.undo();
        assert_eq!(b.text, "", "empty undo stack: no-op");

        // backspace + delete_forward
        let mut c = InputBuffer::default();
        c.insert_str("abc");
        c.backspace();
        assert_eq!(c.text, "ab");
        c.undo();
        assert_eq!(c.text, "abc");
        c.cursor = 1;
        c.delete_forward();
        assert_eq!(c.text, "ac");
        c.undo();
        assert_eq!(c.text, "abc");

        // word kill + line kill
        let mut d = InputBuffer::default();
        d.insert_str("foo bar baz");
        d.cursor = 7;
        assert!(d.delete_word_backward().is_some());
        assert_eq!(d.text, "foo  baz");
        d.undo();
        assert_eq!(d.text, "foo bar baz", "undo word kill");
        assert_eq!(d.cursor, 7, "undo restores the cursor too");
        let del = d.delete_to_line_start();
        d.kill_push(del);
        d.undo();
        assert_eq!(d.text, "foo bar baz");
        d.cursor = 10;
        let del = d.delete_to_line_end();
        d.kill_push(del); // kills the trailing 'z'
        assert_eq!(d.text, "foo bar ba");
        d.undo();
        assert_eq!(d.text, "foo bar baz");
    }

    #[test]
    fn undo_stack_is_bounded_at_100() {
        let mut b = InputBuffer::default();
        for _ in 0..120 {
            b.insert('a');
        }
        assert_eq!(b.text, "a".repeat(120));
        for _ in 0..100 {
            b.undo();
        }
        assert_eq!(b.text, "a".repeat(20), "oldest 20 snapshots were dropped");
        b.undo();
        assert_eq!(b.text, "a".repeat(20), "stack exhausted");
    }

    #[test]
    fn unicode_word_ops_and_undo_keep_char_indices() {
        // "héllo 世界": h é l l o ' ' 世 界 = 8 chars (13 bytes != chars)
        let mut b = InputBuffer::default();
        b.insert_str("héllo 世界");
        assert_eq!(b.cursor, 8);
        b.word_left();
        assert_eq!(b.cursor, 6, "back to the start of 世界 (char index)");
        b.insert('!'); // multibyte prefix, insertion at char 6
        assert_eq!(b.text, "héllo !世界");
        assert_eq!(b.cursor, 7);
        b.undo();
        assert_eq!(b.text, "héllo 世界");
        assert_eq!(b.cursor, 6, "undo restores the multi-byte cursor position");

        let mut c = InputBuffer::default();
        c.insert_str("héllo 世界");
        assert_eq!(c.cursor, 8);
        assert_eq!(c.delete_word_backward().as_deref(), Some("世界"));
        assert_eq!(c.text, "héllo ");
        c.undo();
        assert_eq!(c.text, "héllo 世界");
        assert_eq!(c.cursor, 8, "undo restores the end cursor");

        let mut d = InputBuffer::default();
        d.insert_str("世界");
        d.backspace();
        assert_eq!(d.text, "世");
        d.undo();
        assert_eq!(d.text, "世界");

        // word_right over multibyte
        let mut e = InputBuffer {
            text: "héllo 世界".into(),
            cursor: 0,
            ..Default::default()
        };
        e.word_right();
        assert_eq!(e.cursor, 5, "end of 'héllo' (char 5, bytes differ)");
        e.word_right();
        assert_eq!(e.cursor, 8, "end of text");
        assert_eq!(e.char_to_byte(8), e.text.len(), "cursor at byte end");
        // delete_word_forward across the multibyte boundary
        let mut f = InputBuffer {
            text: "héllo 世界".into(),
            cursor: 6,
            ..Default::default()
        };
        assert_eq!(f.delete_word_forward().as_deref(), Some("世界"));
        assert_eq!(f.text, "héllo ");
    }

    #[test]
    fn transcript_renders_once_per_entry_and_resizes() {
        let mut t = Transcript::default();
        t.set_width(40);
        t.push(Line::User("hello".into()));
        t.push(Line::Info("note".into()));
        t.push_tool_call(ToolCall {
            head: "→ read".into(),
            ..no_note_call()
        });
        // push_tool_call lands via push_separated: one air row + the row
        assert_eq!(t.render_passes(), 4, "one pass per pushed entry");
        t.set_width(40); // same width: no rebuild
        assert_eq!(t.render_passes(), 4, "same width must not re-render");
        t.set_width(72); // resize: one pass per entry again
        assert_eq!(t.render_passes(), 8, "resize rebuilds the cache");
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
    fn user_entry_rows_tracks_user_message_positions() {
        let mut t = Transcript::default();
        t.set_width(60);
        t.push(Line::User("hello".into()));
        t.push(Line::Assistant("world".into()));
        t.push(Line::User("again".into()));
        let rows = t.user_entry_rows();
        assert_eq!(rows.len(), 2, "two user entries");
        assert!(rows[0] == 0, "first entry is a user message");
        assert!(
            rows[1] > rows[0] && rows[1] < t.total_rows(),
            "second user entry starts after the first, before the tail"
        );
        // every reported row really is the start of a user band
        for &r in &rows {
            let row = t.row(r).unwrap();
            let bg = row.spans.first().and_then(|s| s.style.bg);
            assert_eq!(bg, Some(crate::palette::BG_USER), "user band at row {r}");
        }
        // resizing keeps the mapping stable
        t.set_width(20);
        assert_eq!(t.user_entry_rows(), rows);
    }
    #[test]
    fn rail_thumb_sizes_and_positions_proportionally() {
        // no rail when everything fits or the track is empty
        assert_eq!(rail_thumb(10, 10, 10, 0), (0, 0));
        assert_eq!(rail_thumb(0, 100, 0, 0), (0, 0));
        // the thumb never collapses to zero cells
        assert_eq!(rail_thumb(10, 1000, 10, 0), (0, 1));
        // half the content visible: half-size thumb, tail-pinned bottom
        assert_eq!(rail_thumb(10, 20, 10, 10), (5, 5));
        // mid-scroll anchor rides proportionally
        assert_eq!(rail_thumb(10, 100, 20, 40), (4, 2));
        // an out-of-range anchor clamps inside the track
        assert_eq!(rail_thumb(10, 20, 10, 999), (5, 5));
    }
    #[test]
    fn visible_rows_carves_out_chrome() {
        // top margin + input area + strip + footer + transcript top border
        assert_eq!(visible_rows(24, 3), 17);
        assert_eq!(visible_rows(5, 3), 0, "never underflows");
        assert_eq!(visible_rows(0, 0), 0);
        // growth of the input eats the viewport one row at a time
        assert_eq!(visible_rows(24, 8), 12);
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
    fn resume_hint_needs_a_session_and_activity() {
        let (hint, cmd) = resume_hint("s19a4f2e1b0-3f9c2a81d4b7", 1, false, false).unwrap();
        assert!(hint.contains("ka -c"), "{hint}");
        assert!(hint.contains("ka --session 3f9c2a81"), "{hint}");
        assert_eq!(
            cmd, "ka --session 3f9c2a81",
            "the command lands in $HISTFILE"
        );
        // quitting mid-turn (turns == 0, still busy) still resumes
        assert!(resume_hint("s19a4f2e1b0-3f9c2a81d4b7", 0, true, false).is_some());
        // an older chat replayed at startup, closed with no new turn
        assert!(resume_hint("s19a4f2e1b0-3f9c2a81d4b7", 0, false, true).is_some());
        // a fresh session with no turns, idle: not worth resuming
        assert!(resume_hint("s19a4f2e1b0-3f9c2a81d4b7", 0, false, false).is_none());
        // no session id (engine died at bootstrap): nothing to print
        assert!(resume_hint("", 3, false, true).is_none());
    }

    #[test]
    fn history_line_sniffs_zsh_extended_format() {
        // zsh-style tail → matching extended shape
        assert_eq!(
            history_line(
                Some(": 1700000000:5;cargo run"),
                "ka --session abc",
                1700000123
            ),
            ":1700000123:0;ka --session abc"
        );
        // plain tail (bash) → plain command
        assert_eq!(
            history_line(Some("cargo run"), "ka --session abc", 7),
            "ka --session abc"
        );
        // empty file → plain
        assert_eq!(
            history_line(None, "ka --session abc", 7),
            "ka --session abc"
        );
        // non-numeric timestamps are not the zsh shape
        assert_eq!(history_line(Some(": start:5;ls"), "cmd", 9), "cmd");
    }

    #[test]
    fn append_history_to_appends_and_never_creates() {
        let dir = std::env::temp_dir().join(format!("ka-hist-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let hist = dir.join("hist");
        // missing file: skipped, never created
        append_history_to(&hist, "ka --session deadbeef", 1);
        assert!(!hist.exists());
        // zsh-style file: the extended line appends
        std::fs::write(&hist, ": 1700000000:5;old cmd").unwrap(); // no trailing newline
        append_history_to(&hist, "ka --session deadbeef", 1700000123);
        let out = std::fs::read_to_string(&hist).unwrap();
        assert!(
            out.ends_with(":1700000123:0;ka --session deadbeef\n"),
            "{out:?}"
        );
        // plain file: the bare command appends
        std::fs::write(&hist, "old cmd\n").unwrap();
        append_history_to(&hist, "ka --session deadbeef", 2);
        assert!(
            std::fs::read_to_string(&hist)
                .unwrap()
                .ends_with("ka --session deadbeef\n")
        );
        let _ = std::fs::remove_dir_all(&dir);
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
                    cost: 0.0,
                    tokens: 0,
                    parent: None,
                },
                ka_strand::StrandSummary {
                    path: std::path::PathBuf::from("/tmp/b.jsonl"),
                    id: "s1-bbbb22221111".to_string(),
                    ts: "2026-01-01T00:00:00Z".to_string(),
                    title: "active session".to_string(),
                    messages: 5,
                    cost: 0.0,
                    tokens: 0,
                    parent: None,
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
                cost: 0.0,
                tokens: 0,
                parent: None,
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
        for want in [
            "/session",
            "/resume",
            "/new",
            "/settings",
            "/export",
            "/retry",
            "/copy",
            "/usage",
            // dispatchable all along, now cataloged too
            "/image",
            "/agents",
        ] {
            assert!(names.contains(&want.to_string()), "missing {want}");
        }
        // customs are named like any slash command, so the popup
        // prefix-filter matches and Tab inserts a usable command
        assert!(
            names.iter().all(|n| n.starts_with('/')),
            "every catalog entry is a /command: {names:?}"
        );
        // popup filter serves /he → /help (prefix match over the names)
        let popup = update_suggestions("/he").expect("popup for /he");
        assert!(popup.items.iter().any(|(n, _)| n == "/help"), "{popup:?}");
    }

    #[test]
    fn rewind_bad_arg_warns_instead_of_defaulting() {
        // unparsable count: usage warning, no command sent
        let cmd = slash_command("/rewind abc").unwrap();
        assert_eq!(cmd.note.as_deref(), Some("usage: /rewind [n]"));
        assert!(cmd.event.is_none());
        // bare /rewind keeps its documented default of one exchange
        let cmd = slash_command("/rewind").unwrap();
        assert!(matches!(cmd.event, Some(Command::Rewind { turns: 1 })));
        // an explicit count passes through
        let cmd = slash_command("/rewind 3").unwrap();
        assert!(matches!(cmd.event, Some(Command::Rewind { turns: 3 })));
    }

    #[test]
    fn help_lists_all_keys_and_groups() {
        let text: String = help_modal_rows()
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        // the previously undocumented keys are taught
        for key in [
            "ctrl+r",
            "ctrl+p / ctrl+n",
            "ctrl+q",
            "ctrl+o",
            "ctrl+m",
            "ctrl+t / alt+o / alt+i",
            "alt+t",
            "alt+e",
            "ctrl+↑ / ctrl+↓",
            "esc esc",
        ] {
            assert!(text.contains(key), "help lacks {key}");
        }
        // command groups in order
        for group in [
            "keys",
            "session",
            "model & providers",
            "tools & context",
            "safety & plans",
            "meta",
        ] {
            assert!(
                text.contains(&format!("▸ {group}")),
                "help lacks group {group}"
            );
        }
        // rows keep the `{name:<14} {desc}` shape: 15 cols before the text
        let rows = help_modal_rows();
        let line = rows
            .iter()
            .find(|l| {
                let t: String = l.spans.iter().map(|s| s.content.to_string()).collect();
                t.starts_with("/session")
            })
            .expect("/session row");
        assert_eq!(line.spans[0].content.len(), 15, "{line:?}");

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
    fn usage_rows_render_session_totals_and_recent() {
        let meters = Meters {
            turns: 3,
            tokens_in: 1500,
            tokens_out: 300,
            cache_read: 900,
            cost: 0.0123,
            elapsed: 95.0,
            ..Default::default()
        };
        let sessions = vec![
            ka_strand::StrandSummary {
                path: std::path::PathBuf::from("/tmp/a.jsonl"),
                id: "s1-aaaa1111".into(),
                ts: "2026-09-01T10:00:00Z".into(),
                title: "fix the parser".into(),
                messages: 12,
                cost: 0.5,
                parent: None,
                tokens: 1234,
            },
            ka_strand::StrandSummary {
                path: std::path::PathBuf::from("/tmp/b.jsonl"),
                id: "s1-bbbb2222".into(),
                ts: "2026-09-02T10:00:00Z".into(),
                title: "tidy docs".into(),
                messages: 4,
                cost: 0.003,
                parent: None,
                tokens: 0,
            },
        ];
        let rows = usage_rows(&meters, &sessions);
        let joined = rows.join("\n");
        assert_eq!(rows[0], "▸ session", "{joined}");
        assert!(joined.contains("3 turns"), "{joined}");
        assert!(joined.contains("1.5k in / 300 out"), "{joined}");
        assert!(joined.contains("cache 900"), "{joined}");
        assert!(joined.contains("hit 60%"), "{joined}");
        assert!(joined.contains("$0.0123"), "{joined}");
        assert!(joined.contains("1:35 busy"), "{joined}");
        // recent sessions: title, age, tokens, cost
        assert!(joined.contains("fix the parser"), "{joined}");
        assert!(joined.contains("1.2k tok · $0.50"), "{joined}");
        // total row sums the listed sessions
        assert!(
            joined.contains("total · 2 sessions · 1.2k tok · $0.50"),
            "{joined}"
        );
        // empty history degrades to a placeholder, not a bare section
        let empty = usage_rows(&Meters::default(), &[]);
        assert!(empty.join("\n").contains("(no recorded sessions)"));
    }

    #[test]
    fn approve_matches_build_commands() {
        let build = slash_command("/build").unwrap();
        let approve = slash_command("/approve").unwrap();
        let guard = |sl: &Slash| {
            matches!(
                sl.event,
                Some(Command::SetMode {
                    mode: ka_protocol::Mode::Guarded,
                })
            )
        };
        assert!(guard(&build) && guard(&approve), "both flip to Guarded");
        assert_eq!(build.followup, approve.followup, "shared followup");
        assert!(
            approve
                .followup
                .as_deref()
                .is_some_and(|f| f.contains(".ka/plans/plan.md")),
            "{:?}",
            approve.followup
        );
    }

    #[test]
    fn plan_drafted_needs_a_file_fresh_after_start() {
        let dir = std::env::temp_dir().join(format!("ka-plan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let plan = dir.join("plan.md");
        let started = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        // no file yet → not drafted
        assert!(!plan_drafted(Some(started), &plan));
        std::fs::write(&plan, "# plan\n").unwrap();
        // file written after start → drafted
        assert!(plan_drafted(Some(started), &plan));
        // start in the future (file older) → not drafted
        assert!(!plan_drafted(
            Some(std::time::SystemTime::now() + std::time::Duration::from_secs(60)),
            &plan
        ));
        // never started planning → not drafted
        assert!(!plan_drafted(None, &plan));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn context_rows_renders_bars_and_footer() {
        let parts = vec![
            ka_protocol::ContextPart {
                name: "system".into(),
                tokens: 800,
            },
            ka_protocol::ContextPart {
                name: "user".into(),
                tokens: 200,
            },
        ];
        let rows = context_rows(&parts, 10_000);
        let joined = rows.join("\n");
        assert!(joined.contains("system"), "{joined}");
        assert!(joined.contains("█"), "{joined}");
        assert!(joined.contains("200 (20%)"), "{joined}");
        assert!(
            joined.contains("used 1k of 10k (10%) · free 9k"),
            "{joined}"
        );
        let rows = context_rows(&parts, 0);
        assert!(rows.join("\n").contains("free 0"), "{rows:?}");
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
    fn working_row_spins_then_waits_for_approval() {
        let t0 = Instant::now();
        let now = t0 + Duration::from_millis(1500);
        // streaming: a spinner glyph leads, elapsed seconds trail
        let row = working_row(None, Some(t0), now);
        let text: String = row.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.ends_with("working · 1.5s"), "{text}");
        assert!(
            "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏".contains(text.chars().next().unwrap()),
            "spinner glyph leads: {text}"
        );
        assert_eq!(row.spans[0].style.fg, Some(crate::palette::META));
        // permission ask up: static WARN line, no spinner glyph
        let ask = PendingAsk {
            id: ka_protocol::AskId("a".into()),
            question: "allow?".into(),
            options: vec!["yes".into()],
            detail: None,
            selected: 0,
        };
        let row = working_row(Some(&ask), Some(t0), now);
        let text: String = row.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "… waiting for approval · 1.5s");
        assert_eq!(row.spans[0].style.fg, Some(crate::palette::WARN));
        // a missing clock degrades to zero elapsed, never panics
        let row = working_row(None, None, now);
        let text: String = row.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.ends_with("working · 0.0s"), "{text}");
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
    fn status_right_joins_known_facts_and_always_shows_cost() {
        let m = Meters {
            model: "ollama/qwen3.5:9b".into(),
            mode: "guarded".into(),
            context: (10_000, 100_000),
            cost: 0.0123,
            ..Default::default()
        };
        let text = |spans: &[ratatui::text::Span<'static>]| {
            spans.iter().map(|s| s.content.as_ref()).collect::<String>()
        };
        // 10% of the window: one green cell of the eight-cell gauge
        assert_eq!(
            text(&status_right(&m)),
            "ollama/qwen3.5:9b · guarded · ctx █······· 10% · $0.0123"
        );
        assert_eq!(status_right(&m)[5].style.fg, Some(crate::palette::OK));
        // fresh session: unknown model/mode/window collapse away
        assert_eq!(text(&status_right(&Meters::default())), "$0.0000");
    }

    #[test]
    fn ctx_gauge_cells_and_thresholds() {
        let gauge = |used, window| {
            let m = Meters {
                context: (used, window),
                ..Default::default()
            };
            let spans = status_right(&m);
            spans
                .iter()
                .find(|s| s.content.contains('█'))
                .map(|s| (s.content.trim_start().to_string(), s.style.fg))
                .unwrap()
        };
        // 12.5% per cell, thresholds at 80% (WARN) and 95% (ERR)
        assert_eq!(gauge(25_000, 100_000).0, "██······");
        assert_eq!(gauge(50_000, 100_000).0, "████····");
        assert_eq!(gauge(79_000, 100_000).1, Some(crate::palette::OK));
        assert_eq!(gauge(80_000, 100_000).1, Some(crate::palette::WARN));
        assert_eq!(gauge(94_000, 100_000).1, Some(crate::palette::WARN));
        assert_eq!(gauge(95_000, 100_000).1, Some(crate::palette::ERR));
        assert_eq!(gauge(100_000, 100_000).0, "████████");
        // rounding at cell edges: 6.25% = half a cell rounds to one
        assert_eq!(gauge(6_250, 100_000).0, "█·······");
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
        push_gutter(
            &mut out,
            "short text",
            40,
            "⚙ ",
            crate::palette::META.into(),
        );
        assert_eq!(out.len(), 1, "text rows only: air is the separator's job");
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
        // blank rows above and below carry the same fill, giving the
        // card inner top/bottom air
        let out = super::render_line(&Line::Assistant("**hi** there".into()), 40, false);
        assert!(out.len() >= 3, "blank + content + blank, got {}", out.len());
        let surface = crate::palette::BG_OUTPUT;
        let row_text = |l: &ratatui::text::Line<'static>| {
            l.spans
                .iter()
                .map(|s| s.content.to_string())
                .collect::<String>()
        };
        let cols = |l: &ratatui::text::Line<'static>| {
            l.spans
                .iter()
                .map(|s| s.content.chars().count())
                .sum::<usize>()
        };
        // the card opens and closes on a full-width surface blank
        for edge in [out.first().unwrap(), out.last().unwrap()] {
            assert!(
                edge.spans.iter().all(|s| s.style.bg == Some(surface)),
                "edge row filled at span level: {edge:?}"
            );
            assert_eq!(cols(edge), 40, "edge blank fills the width");
            assert!(row_text(edge).trim().is_empty(), "edge row is blank");
        }
        // the first content row keeps the surface fill
        let content = &out[1];
        assert_eq!(row_text(content).trim(), "hi there");
        assert_eq!(cols(content), 40, "surface fills the width");
        let slab = format!("{content:?}");
        assert!(slab.contains(&format!("{surface:?}")), "output bg: {slab}");
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
        // the card opens and closes on a BG_USER pad row
        let user_bg = crate::palette::BG_USER;
        for pad in [out.first().unwrap(), out.last().unwrap()] {
            assert!(
                pad.spans.iter().all(|s| s.style.bg == Some(user_bg)),
                "pad rows carry BG_USER: {pad:?}"
            );
            assert!(text(pad).trim().is_empty(), "pad row is blank");
        }
        assert!(text(&out[1]).starts_with("  ❯ alpha"));
        assert!(
            text(&out[2]).starts_with("    beta"),
            "second source line indents"
        );
        assert_eq!(out.len(), 4, "pad + two rows + pad");
    }

    #[test]
    fn multiline_gutter_prefixes_only_first_row() {
        use super::push_gutter;
        use ratatui::text::Line as TuiLine;

        let mut out: Vec<TuiLine> = Vec::new();
        push_gutter(&mut out, "one\ntwo", 40, "⚙ ", crate::palette::META.into());
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
        let state = std::env::temp_dir().join(format!("ka-cmd-state-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&state);
        let cmds = dir.join(".ka/commands");
        std::fs::create_dir_all(&cmds).unwrap();
        std::fs::write(cmds.join("review.md"), "Review this diff: $ARGUMENTS").unwrap();
        // untrusted project: the command must NOT dispatch
        assert!(
            custom_command_in(&dir, &state, "/review", Some("src/main.rs")).is_none(),
            "untrusted project commands stay gated"
        );
        // trust the project: dispatch substitutes $ARGUMENTS
        std::fs::create_dir_all(state.join("ka")).unwrap();
        ka_engine::trust::save_trust_at(&state.join("ka/trust.json"), std::slice::from_ref(&dir));
        let body = custom_command_in(&dir, &state, "/review", Some("src/main.rs")).unwrap();
        assert_eq!(body, "Review this diff: src/main.rs");

        // frontmatter is stripped from the body and surfaces in the scan
        std::fs::write(
            cmds.join("ship.md"),
            "---\ndescription: ship it\nargument-hint: branch\n---\nShip {branch}: $ARGUMENTS",
        )
        .unwrap();
        let scanned = scan_custom_commands_in(&dir, &state);
        let ship = scanned.iter().find(|c| c.name == "ship").unwrap();
        assert_eq!(ship.description, "ship it");
        assert_eq!(ship.argument_hint, "branch");
        let body = custom_command_in(&dir, &state, "/ship", Some("main")).unwrap();
        assert_eq!(body, "Ship {branch}: main");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&state);
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
        let with_path = slash_command("/export o.md").unwrap();
        assert!(matches!(
            &with_path.event,
            Some(Command::ExportMarkdown {
                out: Some(p),
                html: false
            }) if p == &std::path::PathBuf::from("o.md")
        ));
        assert!(matches!(
            &slash_command("/export").unwrap().event,
            Some(Command::ExportMarkdown {
                out: None,
                html: false
            })
        ));
        // --html flips the renderer and still accepts a path
        assert!(matches!(
            &slash_command("/export --html page.html").unwrap().event,
            Some(Command::ExportMarkdown {
                out: Some(p),
                html: true
            }) if p == &std::path::PathBuf::from("page.html")
        ));
    }

    #[test]
    fn slash_tasks_and_debug_parse() {
        assert!(matches!(
            slash_command("/tasks").unwrap().event,
            Some(Command::ListTasks)
        ));
        assert!(matches!(
            slash_command("/debug").unwrap().event,
            Some(Command::DebugRoster)
        ));
        // both appear in the help listing
        let names: Vec<String> = builtin_slash_commands()
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert_eq!(
            names.iter().filter(|n| n.as_str() == "/tasks").count(),
            1,
            "one /tasks entry: {names:?}"
        );
        assert!(names.contains(&"/debug".to_string()), "{names:?}");
    }

    #[test]
    fn pager_visible_matches_render_clamp() {
        // short pane: the window shrinks with the pane, so the top of a
        // long result stays reachable (render: height = min(n+5, 30,
        // pane) and visible = height - 5)
        assert_eq!(pager_visible(24), 19);
        assert_eq!(pager_visible(10), 5);
        // degenerate pane: at least one row
        assert_eq!(pager_visible(4), 1);
        // tall pane: capped at the box's own cap
        assert_eq!(pager_visible(80), 25);
    }

    #[test]
    fn task_id_of_row_parses_roster_shapes() {
        assert_eq!(task_id_of_row("t-3  running  12s  coder — audit"), Some(3));
        assert_eq!(task_id_of_row("t-42  done    1m03s  x"), Some(42));
        assert_eq!(task_id_of_row("  t-7  running"), Some(7), "leading space");
        assert_eq!(task_id_of_row("t-x  running"), None, "non-numeric id");
        assert_eq!(
            task_id_of_row("job-1`  exit 0  cargo test"),
            None,
            "job rows carry no task id"
        );
        assert_eq!(
            task_id_of_row("dap d1  live     breakpoints in 2 file(s)"),
            None,
            "dap rows carry no task id"
        );
        assert_eq!(task_id_of_row("no background tasks or jobs"), None);
    }

    #[test]
    fn apply_event_collects_tool_and_text_lines() {
        let mut lines = Transcript::default();
        let mut busy = true;
        let mut busy_since = None;
        let mut meters = Meters::default();
        let mut pending = None;
        let mut turn_produced = false;
        let mut usage = None;
        let mut a = String::new();
        let mut t = String::new();
        let mut tool = String::new();
        let mut live_tool: Option<LiveTool> = None;
        let mut last_user: Option<String> = None;
        let mut last_error: Option<String> = None;
        let mut spills: Vec<String> = Vec::new();
        let mut sidebar = SidebarState::default();

        let _ = apply_event(
            &Event::Delta {
                kind: ka_protocol::DeltaKind::Text("hi ".into()),
            },
            &mut lines,
            &mut busy,
            &mut busy_since,
            &mut meters,
            &mut pending,
            &mut turn_produced,
            &mut usage,
            &mut a,
            &mut t,
            &mut tool,
            &mut live_tool,
            &mut last_user,
            &mut last_error,
            &mut spills,
            &mut sidebar,
        );
        let _ = apply_event(
            &Event::Delta {
                kind: ka_protocol::DeltaKind::Text("there".into()),
            },
            &mut lines,
            &mut busy,
            &mut busy_since,
            &mut meters,
            &mut pending,
            &mut turn_produced,
            &mut usage,
            &mut a,
            &mut t,
            &mut tool,
            &mut live_tool,
            &mut last_user,
            &mut last_error,
            &mut spills,
            &mut sidebar,
        );
        let _ = apply_event(
            &Event::CallStarted {
                tool: "read".into(),
                id: "c1".into(),
                detail: "main.rs".into(),
            },
            &mut lines,
            &mut busy,
            &mut busy_since,
            &mut meters,
            &mut pending,
            &mut turn_produced,
            &mut usage,
            &mut a,
            &mut t,
            &mut tool,
            &mut live_tool,
            &mut last_user,
            &mut last_error,
            &mut spills,
            &mut sidebar,
        );
        let _ = apply_event(
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
            &mut turn_produced,
            &mut usage,
            &mut a,
            &mut t,
            &mut tool,
            &mut live_tool,
            &mut last_user,
            &mut last_error,
            &mut spills,
            &mut sidebar,
        );
        let _ = apply_event(
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
            &mut turn_produced,
            &mut usage,
            &mut a,
            &mut t,
            &mut tool,
            &mut live_tool,
            &mut last_user,
            &mut last_error,
            &mut spills,
            &mut sidebar,
        );

        assert!(!busy);
        let texts: Vec<&str> = lines
            .entries()
            .iter()
            .filter_map(|l| match l {
                Line::Assistant(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert!(texts.contains(&"hi there"), "{texts:?}");
        // the tool call lands as a block with a railed row: head from
        // the CallStarted detail, verdict from CallFinished
        let calls: Vec<&ToolCall> = lines
            .entries()
            .iter()
            .filter_map(|l| match l {
                Line::ToolBlock(v) => v.first(),
                _ => None,
            })
            .collect();
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert!(calls[0].head.contains("read"), "{calls:?}");
        assert!(calls[0].ok, "{calls:?}");
    }

    #[test]
    fn call_boundary_flushes_text_before_the_tool_row() {
        let mut lines = Transcript::default();
        let mut busy = false;
        let mut busy_since = None;
        let mut meters = Meters::default();
        let mut pending = None;
        let mut turn_produced = false;
        let mut usage = None;
        let mut a = String::new();
        let mut t = String::new();
        let mut tool = String::new();
        let mut live_tool: Option<LiveTool> = None;
        let mut last_user: Option<String> = None;
        let mut last_error: Option<String> = None;
        let mut spills: Vec<String> = Vec::new();
        let mut sidebar = SidebarState::default();

        let _ = apply_event(
            &Event::Delta {
                kind: ka_protocol::DeltaKind::Thought("pondering".into()),
            },
            &mut lines,
            &mut busy,
            &mut busy_since,
            &mut meters,
            &mut pending,
            &mut turn_produced,
            &mut usage,
            &mut a,
            &mut t,
            &mut tool,
            &mut live_tool,
            &mut last_user,
            &mut last_error,
            &mut spills,
            &mut sidebar,
        );
        let _ = apply_event(
            &Event::Delta {
                kind: ka_protocol::DeltaKind::Text("before ".into()),
            },
            &mut lines,
            &mut busy,
            &mut busy_since,
            &mut meters,
            &mut pending,
            &mut turn_produced,
            &mut usage,
            &mut a,
            &mut t,
            &mut tool,
            &mut live_tool,
            &mut last_user,
            &mut last_error,
            &mut spills,
            &mut sidebar,
        );
        let _ = apply_event(
            &Event::Delta {
                kind: ka_protocol::DeltaKind::Call {
                    tool: "bash".into(),
                    id: "c1".into(),
                },
            },
            &mut lines,
            &mut busy,
            &mut busy_since,
            &mut meters,
            &mut pending,
            &mut turn_produced,
            &mut usage,
            &mut a,
            &mut t,
            &mut tool,
            &mut live_tool,
            &mut last_user,
            &mut last_error,
            &mut spills,
            &mut sidebar,
        );
        // mid-call: text must already be flushed above the tool block, and
        // partial outputs must refresh the preview, not the compact row
        assert!(
            a.is_empty() && t.is_empty(),
            "buffers taken at the boundary"
        );
        let _ = apply_event(
            &Event::CallOutput {
                tool: "bash".into(),
                id: "c1".into(),
                excerpt: "line-1\nline-2".into(),
                is_error: false,
                spill: None,
            },
            &mut lines,
            &mut busy,
            &mut busy_since,
            &mut meters,
            &mut pending,
            &mut turn_produced,
            &mut usage,
            &mut a,
            &mut t,
            &mut tool,
            &mut live_tool,
            &mut last_user,
            &mut last_error,
            &mut spills,
            &mut sidebar,
        );
        let _ = apply_event(
            &Event::CallOutput {
                tool: "bash".into(),
                id: "c1".into(),
                excerpt: "line-3".into(),
                is_error: false,
                spill: None,
            },
            &mut lines,
            &mut busy,
            &mut busy_since,
            &mut meters,
            &mut pending,
            &mut turn_produced,
            &mut usage,
            &mut a,
            &mut t,
            &mut tool,
            &mut live_tool,
            &mut last_user,
            &mut last_error,
            &mut spills,
            &mut sidebar,
        );
        let lt = live_tool.as_ref().unwrap();
        assert_eq!(lt.preview, vec!["line-1", "line-2", "line-3"]);
        assert_eq!(tool, "→ bash", "partials must not append the note");
        let _ = apply_event(
            &Event::CallFinished {
                tool: "bash".into(),
                id: "c1".into(),
                ok: true,
            },
            &mut lines,
            &mut busy,
            &mut busy_since,
            &mut meters,
            &mut pending,
            &mut turn_produced,
            &mut usage,
            &mut a,
            &mut t,
            &mut tool,
            &mut live_tool,
            &mut last_user,
            &mut last_error,
            &mut spills,
            &mut sidebar,
        );
        assert!(live_tool.is_none(), "block collapses on finish");
        let _ = apply_event(
            &Event::Delta {
                kind: ka_protocol::DeltaKind::Text("after".into()),
            },
            &mut lines,
            &mut busy,
            &mut busy_since,
            &mut meters,
            &mut pending,
            &mut turn_produced,
            &mut usage,
            &mut a,
            &mut t,
            &mut tool,
            &mut live_tool,
            &mut last_user,
            &mut last_error,
            &mut spills,
            &mut sidebar,
        );
        let _ = apply_event(
            &Event::CallStarted {
                tool: "bash".into(),
                id: "c2".into(),
                detail: String::new(),
            },
            &mut lines,
            &mut busy,
            &mut busy_since,
            &mut meters,
            &mut pending,
            &mut turn_produced,
            &mut usage,
            &mut a,
            &mut t,
            &mut tool,
            &mut live_tool,
            &mut last_user,
            &mut last_error,
            &mut spills,
            &mut sidebar,
        );
        let _ = apply_event(
            &Event::CallOutput {
                tool: "bash".into(),
                id: "c2".into(),
                excerpt: "boom".into(),
                is_error: true,
                spill: None,
            },
            &mut lines,
            &mut busy,
            &mut busy_since,
            &mut meters,
            &mut pending,
            &mut turn_produced,
            &mut usage,
            &mut a,
            &mut t,
            &mut tool,
            &mut live_tool,
            &mut last_user,
            &mut last_error,
            &mut spills,
            &mut sidebar,
        );
        let _ = apply_event(
            &Event::CallFinished {
                tool: "bash".into(),
                id: "c2".into(),
                ok: false,
            },
            &mut lines,
            &mut busy,
            &mut busy_since,
            &mut meters,
            &mut pending,
            &mut turn_produced,
            &mut usage,
            &mut a,
            &mut t,
            &mut tool,
            &mut live_tool,
            &mut last_user,
            &mut last_error,
            &mut spills,
            &mut sidebar,
        );
        let _ = apply_event(
            &Event::TurnFinished {
                stop: ka_protocol::Stop::Done,
                usage: ka_protocol::Usage::default(),
            },
            &mut lines,
            &mut busy,
            &mut busy_since,
            &mut meters,
            &mut pending,
            &mut turn_produced,
            &mut usage,
            &mut a,
            &mut t,
            &mut tool,
            &mut live_tool,
            &mut last_user,
            &mut last_error,
            &mut spills,
            &mut sidebar,
        );

        let entries = lines.entries();
        let shapes: Vec<String> = entries
            .iter()
            .map(|l| match l {
                Line::Thought(s) => format!("T:{s}"),
                Line::Assistant(s) => format!("A:{s}"),
                Line::ToolBlock(v) => format!(
                    "T:{}",
                    v.iter()
                        .map(|c| c.head.as_str())
                        .collect::<Vec<_>>()
                        .join("|")
                ),
                Line::Report(s) => format!("R:{s}"),
                Line::User(s) => format!("U:{s}"),
                _ => "?".to_string(),
            })
            .collect();
        assert_eq!(
            shapes,
            vec![
                "T:pondering",
                "R:",
                "A:before ",
                "R:",
                "T:→ bash",
                "R:",
                "A:after",
                "R:",
                "T:→ bash",
            ],
            "final order must interleave text and tools: {shapes:?}"
        );
        // verdict + note live on the call, not the head: the first call
        // closed ok, the second failed
        let blocks: Vec<&Vec<ToolCall>> = lines
            .entries()
            .iter()
            .filter_map(|l| match l {
                Line::ToolBlock(v) => Some(v),
                _ => None,
            })
            .collect();
        assert_eq!(blocks.len(), 2, "text between calls splits the blocks");
        assert!(blocks[0][0].ok);
        assert_eq!(blocks[0][0].note, "line-3");
        assert!(!blocks[1][0].ok);
        assert_eq!(blocks[1][0].note, "boom");
    }

    #[test]
    fn flush_live_text_orders_thought_then_assistant() {
        let mut transcript = Transcript::default();
        let mut thought = "why\n".to_string();
        let mut assistant = String::new();
        flush_live_text(&mut transcript, &mut thought, &mut assistant);
        assert!(thought.is_empty() && assistant.is_empty());
        assert_eq!(transcript.entries().len(), 1, "empty text flushes nothing");
        assistant.push_str("answer");
        thought.push_str("more");
        flush_live_text(&mut transcript, &mut thought, &mut assistant);
        let shapes: Vec<String> = transcript
            .entries()
            .iter()
            .map(|l| match l {
                Line::Thought(s) => format!("T:{s}"),
                Line::Assistant(s) => format!("A:{s}"),
                Line::Report(s) => format!("R:{s}"),
                _ => "?".to_string(),
            })
            .collect();
        // the assistant card is a different family: air opens between it
        // and the thought above
        assert_eq!(shapes, vec!["T:why\n", "T:more", "R:", "A:answer"]);
    }

    #[test]
    fn preview_helpers_roll_truncate_and_stay_transient() {
        let mut window: Vec<String> = Vec::new();
        for i in 1..=5 {
            observe_preview(&mut window, &format!("line-{i}"));
        }
        assert_eq!(window, vec!["line-3", "line-4", "line-5"], "rolling 3");
        // width truncation is display-width aware, ellipsis-marked
        let wide = "é".repeat(20);
        let row = preview_row(&wide, 10);
        assert!(row.width() <= 10, "{row:?}");
        assert!(row.ends_with('…'));
        assert_eq!(preview_row("short", 10), "short");
        // the block renders header + at most the 3 newest dim lines
        let lt = LiveTool {
            id: "c1".into(),
            preview: window.clone(),
            last: Some(("line-5".into(), false)),
            started: Instant::now(),
        };
        let rows = tool_live_rows("→ bash", Some(&lt), 40);
        assert_eq!(rows.len(), 1 + PREVIEW_WINDOW);
        let texts: Vec<String> = rows
            .iter()
            .map(|r| {
                r.spans
                    .iter()
                    .map(|s| s.content.to_string())
                    .collect::<String>()
            })
            .collect();
        // quiet rail, then band content with one col of air each side —
        // the band hugs its content, no full-width fill
        assert_eq!(texts[0], " │  → bash ");
        assert_eq!(
            texts[1..].iter().map(|t| t.trim_end()).collect::<Vec<_>>(),
            [" │   line-3", " │   line-4", " │   line-5"]
        );
        for row in &rows {
            // the rail stays canvas-clean; the band content rides BG_TOOL
            assert!(
                &row.spans[1..]
                    .iter()
                    .all(|s| s.style.bg == Some(crate::palette::BG_TOOL)),
                "live band content rides BG_TOOL: {row:?}"
            );
            assert_eq!(row.spans[0].style.bg, None, "rail has no fill");
        }
        assert_eq!(tool_live_rows("→ bash", None, 40).len(), 0);
    }

    #[test]
    fn turn_error_is_buffered_and_surfaced_on_the_report_row() {
        let mut lines = Transcript::default();
        let mut busy = false;
        let mut busy_since = None;
        let mut meters = Meters::default();
        let mut pending = None;
        let mut turn_produced = false;
        let mut usage = None;
        let mut a = String::new();
        let mut t = String::new();
        let mut tool = String::new();
        let mut live_tool: Option<LiveTool> = None;
        let mut last_user: Option<String> = None;
        let mut last_error: Option<String> = None;
        let mut spills: Vec<String> = Vec::new();
        let mut sidebar = SidebarState::default();

        let _ = apply_event(
            &Event::TurnStarted {
                context: ka_protocol::ContextMeter {
                    used: 100,
                    window: 200,
                },
            },
            &mut lines,
            &mut busy,
            &mut busy_since,
            &mut meters,
            &mut pending,
            &mut turn_produced,
            &mut usage,
            &mut a,
            &mut t,
            &mut tool,
            &mut live_tool,
            &mut last_user,
            &mut last_error,
            &mut spills,
            &mut sidebar,
        );
        let _ = apply_event(
            &Event::Delta {
                kind: ka_protocol::DeltaKind::Text("hi".into()),
            },
            &mut lines,
            &mut busy,
            &mut busy_since,
            &mut meters,
            &mut pending,
            &mut turn_produced,
            &mut usage,
            &mut a,
            &mut t,
            &mut tool,
            &mut live_tool,
            &mut last_user,
            &mut last_error,
            &mut spills,
            &mut sidebar,
        );
        let _ = apply_event(
            &Event::Error {
                class: ka_protocol::ErrorClass::RateLimit,
                retryable: true,
                message: "429 over 5h".into(),
            },
            &mut lines,
            &mut busy,
            &mut busy_since,
            &mut meters,
            &mut pending,
            &mut turn_produced,
            &mut usage,
            &mut a,
            &mut t,
            &mut tool,
            &mut live_tool,
            &mut last_user,
            &mut last_error,
            &mut spills,
            &mut sidebar,
        );
        assert!(
            lines.entries().is_empty(),
            "busy in-turn error must not push a standalone row"
        );
        let _ = apply_event(
            &Event::TurnFinished {
                stop: ka_protocol::Stop::Error,
                usage: ka_protocol::Usage {
                    input: 100,
                    output: 5,
                    cost: 0.0002,
                    ..Default::default()
                },
            },
            &mut lines,
            &mut busy,
            &mut busy_since,
            &mut meters,
            &mut pending,
            &mut turn_produced,
            &mut usage,
            &mut a,
            &mut t,
            &mut tool,
            &mut live_tool,
            &mut last_user,
            &mut last_error,
            &mut spills,
            &mut sidebar,
        );
        let entries = lines.entries();
        assert_eq!(
            entries.len(),
            3,
            "assistant, air, one bold failure summary — no second meta row: {entries:?}"
        );
        assert!(matches!(entries[0], Line::Assistant(_)));
        // air opens between the card and the failure summary (family change)
        assert_eq!(&entries[1], &Line::Report(String::new()));
        let Line::Summary { glyph, tone, text } = &entries[2] else {
            panic!("third entry must be a failure Summary: {entries:?}");
        };
        assert_eq!(*glyph, '✗');
        assert_eq!(*tone, SummaryTone::Err);
        for want in ["failed", "429", "/retry", "in", "out", "$0.0002"] {
            assert!(text.contains(want), "summary {text:?} lacks {want}");
        }
        assert!(
            last_error.is_none(),
            "buffered error consumed by the report"
        );
    }

    #[test]
    fn done_with_zero_usage_pushes_no_report_row() {
        let mut lines = Transcript::default();
        let mut busy = false;
        let mut busy_since = None;
        let mut meters = Meters::default();
        let mut pending = None;
        let mut turn_produced = false;
        let mut usage = None;
        let mut a = String::new();
        let mut t = String::new();
        let mut tool = String::new();
        let mut live_tool: Option<LiveTool> = None;
        let mut last_user: Option<String> = None;
        let mut last_error: Option<String> = None;
        let mut spills: Vec<String> = Vec::new();
        let mut sidebar = SidebarState::default();

        let _ = apply_event(
            &Event::TurnStarted {
                context: ka_protocol::ContextMeter::default(),
            },
            &mut lines,
            &mut busy,
            &mut busy_since,
            &mut meters,
            &mut pending,
            &mut turn_produced,
            &mut usage,
            &mut a,
            &mut t,
            &mut tool,
            &mut live_tool,
            &mut last_user,
            &mut last_error,
            &mut spills,
            &mut sidebar,
        );
        let _ = apply_event(
            &Event::TurnFinished {
                stop: ka_protocol::Stop::Done,
                usage: ka_protocol::Usage::default(),
            },
            &mut lines,
            &mut busy,
            &mut busy_since,
            &mut meters,
            &mut pending,
            &mut turn_produced,
            &mut usage,
            &mut a,
            &mut t,
            &mut tool,
            &mut live_tool,
            &mut last_user,
            &mut last_error,
            &mut spills,
            &mut sidebar,
        );
        assert!(
            lines.entries().is_empty(),
            "canned/no-model turns must stay silent"
        );
    }

    #[test]
    fn fmt_dur_and_fmt_tok_hit_the_boundaries() {
        assert_eq!(fmt_dur(0.0), "0.0s");
        assert_eq!(fmt_dur(59.94), "59.9s");
        assert_eq!(fmt_dur(60.0), "1:00");
        assert_eq!(fmt_dur(754.0), "12:34");
        assert_eq!(fmt_tok(0), "0");
        assert_eq!(fmt_tok(999), "999");
        assert_eq!(fmt_tok(1000), "1k");
        assert_eq!(fmt_tok(1234), "1.2k");
        assert_eq!(fmt_tok(100_000), "100k");
        let u = ka_protocol::Usage {
            input: 1000,
            output: 5,
            cost: 0.0002,
            ..Default::default()
        };
        assert_eq!(usage_tail(&u, 3.25), " · 3.2s · 1k in · 5 out · $0.0002");
        let zero = ka_protocol::Usage::default();
        assert_eq!(usage_tail(&zero, 0.0), " · 0.0s");
        let cached = ka_protocol::Usage {
            cache_read: 2000,
            ..Default::default()
        };
        assert!(usage_tail(&cached, 1.0).contains("2k cache"));
    }

    #[test]
    fn b64encode_matches_the_standard_vectors() {
        assert_eq!(b64encode(""), "");
        assert_eq!(b64encode("hello"), "aGVsbG8=");
        assert_eq!(
            b64encode("any carnal pleasure."),
            "YW55IGNhcm5hbCBwbGVhc3VyZS4="
        );
        assert_eq!(b64encode("héllo"), "aMOpbGxv", "multibyte input");
    }

    #[test]
    fn session_picker_row_zero_is_always_new() {
        let empty = SessionPicker {
            sessions: vec![],
            selected: 0,
            filter: String::new(),
            current: None,
        };
        assert_eq!(empty.rows()[0].0, "(new session)");
        let s = ka_strand::StrandSummary {
            path: std::path::PathBuf::from("/tmp/x.jsonl"),
            id: "s1-test".into(),
            ts: "2026-09-02T10:00:00Z".into(),
            title: "hello".into(),
            messages: 2,
            cost: 0.0,
            tokens: 0,
            parent: None,
        };
        let picker = SessionPicker {
            sessions: vec![s],
            selected: 0,
            filter: String::new(),
            current: None,
        };
        let rows = picker.rows();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, "(new session)");
        assert_eq!(rows[1].0, "#test  hello");
        assert!(rows[1].1.ends_with("msgs · 2h ago") || rows[1].1.contains("msgs · "));
        assert_eq!(picker.pick(), None, "row 0 switches to a fresh session");
    }

    #[test]
    fn path_token_finds_the_word_at_the_caret() {
        assert_eq!(path_token("", 0), None);
        assert_eq!(path_token("abc", 0).unwrap(), (0, "abc".to_string()));
        assert_eq!(path_token("abc", 3).unwrap(), (0, "abc".to_string()));
        assert_eq!(path_token("abc def", 4).unwrap(), (4, "def".to_string()));
        assert_eq!(path_token("abc def", 6).unwrap(), (4, "def".to_string()));
        assert_eq!(
            path_token("./src/m", 7).unwrap(),
            (0, "./src/m".to_string())
        );
        assert_eq!(path_token("x ./a y", 4).unwrap(), (2, "./a".to_string()));
        // caret between whitespace: no word
        assert_eq!(path_token("a  b", 2), None);
        assert_eq!(path_token("a ", 2), None);
        assert_eq!(path_token("a  b", 0), Some((0, "a".to_string())));
        // unicode keeps char indices
        assert_eq!(path_token("é 世界 x", 3).unwrap(), (2, "世界".to_string()));
        // newline/tab are whitespace too
        assert_eq!(path_token("a\tb", 2).unwrap(), (2, "b".to_string()));
        assert_eq!(path_token("a\nb", 1), None);
    }

    #[test]
    fn split_path_token_separates_dir_and_base() {
        let (d, b, a) = split_path_token("./src/main.rs");
        assert_eq!(d, "./src/");
        assert_eq!(b, "main.rs");
        assert!(!a);
        let (d, b, a) = split_path_token("~/notes.txt");
        assert_eq!(d, "~/");
        assert_eq!(b, "notes.txt");
        assert!(!a);
        let (d, b, a) = split_path_token("/etc/hosts");
        assert_eq!(d, "/etc/");
        assert_eq!(b, "hosts");
        assert!(a);
        let (d, b, a) = split_path_token("bareword");
        assert_eq!(d, "");
        assert_eq!(b, "bareword");
        assert!(!a);
        let (d, b, _) = split_path_token("dir/");
        assert_eq!(d, "dir/");
        assert_eq!(b, "");
    }

    #[test]
    fn complete_token_replaces_only_the_word() {
        let (t, c) = complete_token("run ./s now", 4, 3, "./src/");
        assert_eq!(t, "run ./src/ now");
        assert_eq!(c, 10);
        let (t, c) = complete_token("./s", 0, 3, "./src/main.rs");
        assert_eq!(t, "./src/main.rs");
        assert_eq!(c, 13);
        // multibyte prefix keeps char arithmetic straight
        let (t, c) = complete_token("é ./s", 2, 3, "./x.md");
        assert_eq!(t, "é ./x.md");
        assert_eq!(c, 8);
    }

    #[test]
    fn list_matches_filters_prefixes_and_hides_dotfiles() {
        let dir = std::env::temp_dir().join(format!("ka-path-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("alpha.rs"), "x").unwrap();
        std::fs::write(dir.join("beta.md"), "x").unwrap();
        std::fs::write(dir.join(".hidden"), "x").unwrap();
        let root = dir.to_string_lossy().to_string() + "/";
        let all = list_matches(&root, "");
        assert!(all.contains(&("src".to_string(), true)), "{all:?}");
        assert!(all.contains(&("alpha.rs".to_string(), false)), "{all:?}");
        assert!(
            all.iter().all(|(n, _)| !n.starts_with('.')),
            "dotfiles hidden: {all:?}"
        );
        let a = list_matches(&root, "a");
        assert_eq!(a, vec![("alpha.rs".to_string(), false)]);
        let dot = list_matches(&root, ".h");
        assert_eq!(
            dot,
            vec![(".hidden".to_string(), false)],
            "dotfiles when base dots"
        );
        let sub = list_matches(&(root.clone() + "src/"), "m");
        assert!(sub.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn rel_age_at_uses_the_hinnant_civil_days() {
        let now = 1_788_350_400; // 2026-09-02T12:00:00Z
        assert_eq!(rel_age_at("2026-09-02T11:59:30Z", now), "just now");
        assert_eq!(
            rel_age_at("2026-09-02T12:00:10Z", now),
            "just now",
            "future clamps"
        );
        assert_eq!(rel_age_at("2026-09-02T11:00:01Z", now), "59m ago");
        assert_eq!(rel_age_at("2026-09-02T11:00:00Z", now), "1h ago");
        assert_eq!(rel_age_at("2026-09-01T11:00:00Z", now), "1d ago");
        assert_eq!(rel_age_at("2026-07-30T12:00:00Z", now), "4w ago", "34 days");
        // fractional seconds and offsets parse like RFC3339
        assert_eq!(rel_age_at("2026-09-02T11:59:30.250Z", now), "just now");
        assert_eq!(rel_age_at("garbage", now), "garbage");
        assert_eq!(
            rel_age_at("2026-09-02", now),
            "2026-09-02",
            "missing time fields"
        );
        // epoch-aligned sanity: the civil formula matches the calendar
        assert_eq!(rel_age_at("1970-01-01T00:00:00Z", 0), "just now");
    }
    #[test]
    fn reverse_search_cycles_matches_newest_first() {
        let mut b = InputBuffer::default();
        for t in ["alpha one", "beta", "alpha two"] {
            b.insert_str(t);
            b.take();
        } // history oldest→newest: alpha one, beta, alpha two
        b.search_start();
        for c in "alpha".chars() {
            b.search_push(c);
        }
        assert_eq!(
            b.text, "alpha two",
            "first match is the newest entry containing the query"
        );
        assert_eq!(b.cursor, 9, "cursor lands at the match end");
        b.search_next();
        assert_eq!(b.text, "alpha one", "ctrl+r steps to the older match");
        b.search_next();
        assert_eq!(b.text, "alpha one", "exhausted history stays on the match");
        b.search_accept();
        assert!(!b.searching());
        assert_eq!(b.text, "alpha one", "accept keeps the match loaded");

        // a growing query restarts from the newest entry
        let mut c = InputBuffer::default();
        for t in ["alpha one", "alpha two"] {
            c.insert_str(t);
            c.take();
        }
        c.search_start();
        c.search_push('a');
        c.search_push('l');
        c.search_push('p');
        assert_eq!(c.text, "alpha two");
        assert_eq!(c.search_title().as_deref(), Some("rsearch: `alp`"));
    }

    #[test]
    fn reverse_search_cancel_restores_the_draft() {
        let mut b = InputBuffer::default();
        b.insert_str("keep");
        b.take();
        b.insert_str("draft in progress");
        b.cursor = 5;
        b.search_start();
        b.search_push('k');
        assert_eq!(b.text, "keep", "a live match replaces the draft view");
        b.search_cancel();
        assert_eq!(b.text, "draft in progress", "esc restores the draft");
        assert_eq!(b.cursor, 5);
        assert!(!b.searching());
    }

    #[test]
    fn reverse_search_empty_query_is_a_no_op() {
        let mut b = InputBuffer::default();
        b.insert_str("older");
        b.take();
        b.insert_str("live draft");
        b.search_start();
        assert_eq!(b.search_title().as_deref(), Some("rsearch: ``"));
        b.search_next();
        b.search_accept();
        assert_eq!(b.text, "live draft", "empty query never touches the buffer");

        // backspacing the query away re-matches from the newest entry
        let mut c = InputBuffer::default();
        c.insert_str("hello world");
        c.take();
        c.insert_str("draft");
        c.search_start();
        c.search_push('x');
        assert!(c.search_title().unwrap().contains("(no match)"));
        assert_eq!(c.text, "draft", "a missed query leaves the buffer alone");
        c.search_backspace();
        c.search_push('h');
        assert_eq!(c.text, "hello world");
        c.search_backspace();
        assert_eq!(c.search_title().as_deref(), Some("rsearch: ``"));
    }
    #[test]
    fn mention_token_requires_more_than_the_sigil() {
        assert_eq!(mention_token("@src/ma"), Some("src/ma"));
        assert_eq!(mention_token("@"), None, "bare @ never opens the popup");
        assert_eq!(mention_token("src/ma"), None, "missing sigil");
        assert_eq!(mention_token(""), None);
        // the word at the caret carries the sigil
        assert_eq!(
            path_token("run @src/ma", 11).unwrap(),
            (4, "@src/ma".to_string())
        );
    }

    #[test]
    fn walk_files_skips_noise_dirs_and_sorts() {
        let dir = std::env::temp_dir().join(format!("ka-walk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src/deep")).unwrap();
        std::fs::create_dir_all(dir.join(".git/objects")).unwrap();
        std::fs::create_dir_all(dir.join("target/debug")).unwrap();
        std::fs::create_dir_all(dir.join("node_modules/pkg")).unwrap();
        std::fs::write(dir.join("README.md"), "x").unwrap();
        std::fs::write(dir.join("src/main.rs"), "x").unwrap();
        std::fs::write(dir.join("src/deep/util.rs"), "x").unwrap();
        std::fs::write(dir.join(".hidden"), "x").unwrap();
        std::fs::write(dir.join("target/out.bin"), "x").unwrap();
        let got = walk_files(&dir, WALK_CAP);
        assert_eq!(
            got,
            vec![
                ("README.md".to_string(), false),
                ("src".to_string(), true),
                ("src/deep".to_string(), true),
                ("src/deep/util.rs".to_string(), false),
                ("src/main.rs".to_string(), false),
            ],
            "dotfiles, dot-dirs and skip-dirs are gone; sorted, no slashes on dirs"
        );
        // the cap truncates the sorted listing
        assert_eq!(walk_files(&dir, 2).len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mention_matches_use_the_last_segment_prefix() {
        let entries = vec![
            ("src/main.rs".to_string(), false),
            ("lib/main.rs".to_string(), false),
            ("README.md".to_string(), false),
            ("src".to_string(), true),
        ];
        assert_eq!(
            mention_matches(&entries, "src/ma"),
            vec![
                ("src/main.rs".to_string(), false),
                ("lib/main.rs".to_string(), false),
            ],
            "only the text after the last / is prefix-matched"
        );
        assert_eq!(
            mention_matches(&entries, "src"),
            vec![("src".to_string(), true)],
            "segment match, not a path-prefix match"
        );
        assert!(mention_matches(&entries, "zzz").is_empty());
    }

    #[test]
    fn mention_accept_insert_math() {
        // Tab on `@src/ma` with src/main.rs selected: token replaced by
        // the mention plus a trailing space
        let (t, c) = complete_token("run @src/ma", 4, 7, "@src/main.rs ");
        assert_eq!(t, "run @src/main.rs ");
        assert_eq!(c, 17);
        // a directory accept descends: token becomes the new prefix
        let (t, c) = complete_token("@src/", 0, 5, "@src/deep/");
        assert_eq!(t, "@src/deep/");
        assert_eq!(c, 10);
    }
    #[test]
    fn transcript_find_finds_from_the_anchor_row() {
        let mut t = Transcript::default();
        t.set_width(40);
        t.push(Line::User("hello world".into()));
        t.push(Line::Assistant("quick brown".into()));
        t.push(Line::Info("HELLO again".into()));
        // case-insensitive scan from the top
        let (first_i, first_off) = t.find_from(0, "HeLLo").unwrap();
        assert_eq!(first_i, 0, "first match from the top");
        assert_eq!(first_off, 0);
        assert!(t.find_from(0, "BROWN").is_some());
        // resume past the first hit: the note is the next match
        let (second_i, second_off) = t.find_from(first_off + 1, "hello").unwrap();
        assert_eq!(second_i, 2, "non-matching entries are skipped");
        assert!(
            second_off > first_off,
            "offsets come from the rendered cache"
        );
        // the anchor row itself is included…
        assert_eq!(t.find_from(second_off, "hello").unwrap().0, 2);
        // …but strictly past it is exhausted
        assert!(t.find_from(second_off + 1, "hello").is_none());
        // an anchor inside a multi-row entry skips that entry
        let mid = first_off + 1;
        assert_eq!(t.find_from(mid, "hello").unwrap().0, 2);
        assert!(t.find_from(0, "").is_none());
    }

    #[test]
    fn find_never_matches_blank_separator_rows() {
        let mut t = Transcript::default();
        t.set_width(40);
        t.push_separated(Line::User("alpha".into()));
        t.push_separated(Line::Assistant("needle here".into()));
        t.push_separated(Line::Info("beta".into()));
        // every anchor row must land on a content entry — the Report("")
        // separators hold no text and can never be hits
        let mut hits: Vec<usize> = Vec::new();
        for row in 0..t.total_rows() {
            if let Some((i, _)) = t.find_from(row, "needle") {
                hits.push(i);
            }
        }
        hits.dedup();
        assert_eq!(hits, vec![2], "only the assistant entry matches");
    }

    #[test]
    fn record_spill_dedupes_and_caps_at_50() {
        let mut spills: Vec<String> = Vec::new();
        record_spill(&mut spills, "/tmp/a");
        record_spill(&mut spills, "/tmp/b");
        record_spill(&mut spills, "/tmp/a");
        assert_eq!(spills, vec!["/tmp/a".to_string(), "/tmp/b".to_string()]);
        for i in 0..60 {
            record_spill(&mut spills, &format!("/tmp/x{i}"));
        }
        assert_eq!(spills.len(), 50, "capped at 50");
        assert_eq!(spills.last().unwrap(), "/tmp/x59", "newest last");
        assert!(
            !spills.contains(&"/tmp/a".to_string()),
            "oldest entries dropped"
        );
    }
    #[test]
    fn slash_commands_fork_checkpoint_restore() {
        // bare /fork copies the session as-is
        let fork = slash_command("/fork").unwrap();
        assert!(matches!(fork.event, Some(Command::ForkStrand { turns: 0 })));
        let fork3 = slash_command("/fork 3").unwrap();
        assert!(matches!(
            fork3.event,
            Some(Command::ForkStrand { turns: 3 })
        ));
        // an unparsable count is refused, not defaulted
        let bad = slash_command("/fork two").unwrap();
        assert!(bad.event.is_none(), "nothing sent on a bad count");
        assert_eq!(bad.note.as_deref(), Some("usage: /fork [turns]"));

        let cp = slash_command("/checkpoint").unwrap();
        assert!(matches!(cp.event, Some(Command::Checkpoint)));
        let restore = slash_command("/restore").unwrap();
        assert!(
            matches!(restore.event, Some(Command::RestoreCheckpoint { ref id }) if id == "list"),
            "bare /restore lists"
        );
        let restore_id = slash_command("/restore abc-123").unwrap();
        assert!(matches!(
            restore_id.event,
            Some(Command::RestoreCheckpoint { ref id }) if id == "abc-123"
        ));

        let names: Vec<String> = available_slash_commands()
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        for want in ["/fork", "/checkpoint", "/restore"] {
            assert!(names.contains(&want.to_string()), "missing {want}");
        }
    }

    #[test]
    fn session_picker_rows_show_cost_and_tokens() {
        let rich = SessionPicker {
            current: None,
            sessions: vec![ka_strand::StrandSummary {
                path: std::path::PathBuf::from("/tmp/r.jsonl"),
                id: "s1-cccc3333".into(),
                ts: "2026-09-02T10:00:00Z".into(),
                title: "priced".into(),
                messages: 7,
                cost: 1.234,
                tokens: 12_345,
                parent: None,
            }],
            selected: 0,
            filter: String::new(),
        };
        let (label, detail) = &rich.rows()[1];
        assert!(detail.contains("7 msgs"), "{detail}");
        assert!(detail.contains("$1.23"), "{detail}");
        assert!(detail.contains("12.3k tok"), "{detail}");
        assert!(!label.contains('$'), "label line stays clean: {label}");

        // sub-cent cost and zero tokens add nothing (no dangling separators)
        let free = SessionPicker {
            current: None,
            sessions: vec![ka_strand::StrandSummary {
                path: std::path::PathBuf::from("/tmp/f.jsonl"),
                id: "s1-dddd4444".into(),
                ts: "2026-09-02T10:00:00Z".into(),
                title: "free".into(),
                messages: 2,
                cost: 0.004,
                tokens: 0,
                parent: None,
            }],
            selected: 0,
            filter: String::new(),
        };
        let detail = &free.rows()[1].1;
        assert!(detail.contains("2 msgs · "), "{detail}");
        assert!(!detail.contains('$'), "sub-cent stays hidden: {detail}");
        assert!(!detail.contains("tok"), "{detail}");
    }

    /// Feed one event through `apply_event` with fresh scratch state.
    fn feed(lines: &mut Transcript, evt: &Event) {
        let mut busy = false;
        let mut busy_since = None;
        let mut meters = Meters::default();
        let mut pending = None;
        let mut turn_produced = false;
        let mut usage = None;
        let mut a = String::new();
        let mut t = String::new();
        let mut tool = String::new();
        let mut live_tool: Option<LiveTool> = None;
        let mut last_user: Option<String> = None;
        let mut last_error: Option<String> = None;
        let mut spills: Vec<String> = Vec::new();
        let mut sidebar = SidebarState::default();
        let _ = apply_event(
            evt,
            lines,
            &mut busy,
            &mut busy_since,
            &mut meters,
            &mut pending,
            &mut turn_produced,
            &mut usage,
            &mut a,
            &mut t,
            &mut tool,
            &mut live_tool,
            &mut last_user,
            &mut last_error,
            &mut spills,
            &mut sidebar,
        );
    }

    #[test]
    fn inventory_without_mcp_or_agents_lands_no_card_line() {
        let mut lines = Transcript::default();
        lines.set_width(90);
        feed(
            &mut lines,
            &Event::Inventory {
                tools: ["read", "edit", "bash"]
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
                mcp: Vec::new(),
                agents: Vec::new(),
                skills: vec!["rust-docs".into()],
                prompts: Vec::new(),
            },
        );
        // tools and skills live in the sidebar now: nothing in the chat
        assert!(lines.entries().is_empty(), "{:?}", lines.entries());
        assert_eq!(lines.total_rows(), 0);
    }

    #[test]
    fn inventory_marks_failed_mcp_and_omits_empty_segments() {
        let mut lines = Transcript::default();
        feed(
            &mut lines,
            &Event::Inventory {
                tools: vec!["read".into()],
                mcp: vec![
                    ka_protocol::McpSummary {
                        name: "fetch".into(),
                        ok: true,
                        tools: 3,
                    },
                    ka_protocol::McpSummary {
                        name: "jira".into(),
                        ok: false,
                        tools: 0,
                    },
                ],
                agents: Vec::new(),
                skills: Vec::new(),
                prompts: Vec::new(),
            },
        );
        let entries = lines.entries();
        let [Line::Info(text)] = entries else {
            panic!("one Info card expected, got {entries:?}")
        };
        let rows: Vec<&str> = text.split('\n').collect();
        assert_eq!(rows, ["mcp: fetch ✓ 3 · jira ✗"]);
    }

    #[test]
    fn inventory_agents_only_row_with_overflow_mark() {
        let mut lines = Transcript::default();
        let agents: Vec<String> = (0..6)
            .map(|i| format!("agent-with-a-long-name-{i}"))
            .collect();
        feed(
            &mut lines,
            &Event::Inventory {
                tools: vec!["read".into()],
                mcp: Vec::new(),
                agents,
                skills: vec!["rust-docs".into()],
                prompts: Vec::new(),
            },
        );
        let entries = lines.entries();
        let [Line::Info(text)] = entries else {
            panic!("one Info card expected, got {entries:?}")
        };
        assert!(
            text.starts_with("agents: agent-with-a-long-name-0"),
            "agents-only card, no headline: {text}"
        );
        assert!(text.contains("(+4)"), "overflow mark present: {text}");
        assert!(!text.contains("tools"), "no tools segment: {text}");
        assert!(!text.contains("skills"), "no skills segment: {text}");
        for row in text.split('\n') {
            assert!(row.chars().count() <= 90, "row fits ~90 cols: {row}");
        }
    }

    #[test]
    fn delete_forward_removes_char_at_cursor_and_is_noop_at_end() {
        let mut c = InputBuffer::default();
        c.insert_str("héllo");
        c.end();
        c.delete_forward();
        assert_eq!(c.text, "héllo", "no-op at the end of the text");
        c.home();
        c.right(); // cursor sits on the multibyte 'é'
        c.delete_forward();
        assert_eq!(c.text, "hllo", "the whole char is removed");
        assert_eq!(c.cursor, 1, "cursor stays put");
    }

    #[test]
    fn separate_before_puts_air_between_families_only() {
        let mut t = Transcript::default();
        separate_before(&mut t, Family::User);
        assert!(t.entries().is_empty(), "fresh transcript: no air");
        t.push(Line::User("first".into()));
        separate_before(&mut t, Family::User);
        assert_eq!(t.entries().len(), 1, "same family: no air");
        separate_before(&mut t, Family::Meta);
        assert_eq!(
            t.entries().last(),
            Some(&Line::Report(String::new())),
            "different family: exactly one blank"
        );
        separate_before(&mut t, Family::Assistant);
        assert_eq!(
            t.entries()
                .iter()
                .filter(|l| **l == Line::Report(String::new()))
                .count(),
            1,
            "blank tail: never doubled"
        );
    }

    #[test]
    fn tool_run_stays_tight_then_reopens_air() {
        let mut t = Transcript::default();
        t.push_tool_call(ToolCall {
            head: "→ read · lib.rs".into(),
            ok: true,
            ..no_note_call()
        });
        t.push_tool_call(ToolCall {
            head: "→ bash · cargo build".into(),
            ok: true,
            note: "ok".into(),
            ..no_note_call()
        });
        t.push_separated(Line::Assistant("answer".into()));
        // a turn ends on meta; the next user card reopens the air
        t.push_separated(Line::Summary {
            glyph: '✓',
            tone: SummaryTone::Ok,
            text: "done · 0.0s".into(),
        });
        t.push_separated(Line::Report("task row".into()));
        t.push_separated(Line::User("again".into()));
        let shapes: Vec<String> = t
            .entries()
            .iter()
            .map(|l| match l {
                Line::User(s) => format!("U:{s}"),
                Line::Assistant(s) => format!("A:{s}"),
                Line::ToolBlock(v) => format!(
                    "T:{}",
                    v.iter()
                        .map(|c| c.head.as_str())
                        .collect::<Vec<_>>()
                        .join("|")
                ),
                Line::Summary { text, .. } => format!("S:{text}"),
                Line::Report(s) => format!("R:{s}"),
                _ => "?".to_string(),
            })
            .collect();
        assert_eq!(
            shapes,
            [
                "T:→ read · lib.rs|→ bash · cargo build",
                "R:",
                "A:answer",
                "R:",
                "S:done · 0.0s",
                "R:task row",
                "R:",
                "U:again",
            ],
            "consecutive calls merge tight, meta runs stay tight, families get air"
        );
        // nothing is stranded: the blank is exactly BETWEEN the blocks
        assert_ne!(
            t.entries().last(),
            Some(&Line::Report(String::new())),
            "no trailing blank after the final user row"
        );
    }

    /// A [`ToolCall`] with no output, no spill, no duration — the
    /// replay/shape-test baseline.
    fn no_note_call() -> ToolCall {
        ToolCall {
            head: String::new(),
            ok: true,
            note: String::new(),
            excerpt: String::new(),
            spill: None,
            dur: None,
            expanded: false,
        }
    }

    #[test]
    fn call_started_upgrades_stream_header_by_id() {
        let mut lines = Transcript::default();
        let mut busy = true;
        let mut busy_since = None;
        let mut meters = Meters::default();
        let mut pending = None;
        let mut turn_produced = false;
        let mut usage = None;
        let mut a = String::new();
        let mut t = String::new();
        let mut tool = String::new();
        let mut live_tool: Option<LiveTool> = None;
        let mut last_user: Option<String> = None;
        let mut last_error: Option<String> = None;
        let mut spills: Vec<String> = Vec::new();
        let mut sidebar = SidebarState::default();
        let mut feed = |lines: &mut Transcript, evt: &Event| -> (String, Option<Line>) {
            let _ = apply_event(
                evt,
                lines,
                &mut busy,
                &mut busy_since,
                &mut meters,
                &mut pending,
                &mut turn_produced,
                &mut usage,
                &mut a,
                &mut t,
                &mut tool,
                &mut live_tool,
                &mut last_user,
                &mut last_error,
                &mut spills,
                &mut sidebar,
            );
            (tool.clone(), lines.entries().last().cloned())
        };
        let (head, _last) = feed(
            &mut lines,
            &Event::Delta {
                kind: ka_protocol::DeltaKind::Call {
                    tool: "bash".into(),
                    id: "c1".into(),
                },
            },
        );
        assert_eq!(head, "→ bash", "stream header starts plain");
        let (head, last) = feed(
            &mut lines,
            &Event::CallStarted {
                tool: "bash".into(),
                id: "c1".into(),
                detail: "cargo build".into(),
            },
        );
        assert_eq!(head, "→ bash · cargo build", "same id upgrades in place");
        assert!(last.is_none(), "upgrade must not close the row");
        let (head, last) = feed(
            &mut lines,
            &Event::CallStarted {
                tool: "read".into(),
                id: "c2".into(),
                detail: "lib.rs".into(),
            },
        );
        // the never-finished bash header closes as a bare block row
        let got = last.expect("new call closes the old row");
        let Line::ToolBlock(v) = got else {
            panic!("expected a ToolBlock, got {got:?}");
        };
        assert_eq!(v[0].head, "→ bash · cargo build");
        assert!(!v[0].ok, "a call closed without CallFinished reads failed");
        assert_eq!(head, "→ read · lib.rs");
    }

    #[test]
    fn info_warn_err_render_three_tiers() {
        let info = super::render_line(&Line::Info("steering this turn".into()), 40, false);
        let warn = super::render_line(&Line::Warn("turn running".into()), 40, false);
        let err = super::render_line(&Line::Err("429 too many requests".into()), 40, false);
        let text = |rows: &Vec<ratatui::text::Line<'static>>| {
            rows[0]
                .spans
                .iter()
                .map(|s| s.content.to_string())
                .collect::<String>()
        };
        // gutter rows carry their style on the line (push_gutter), so
        // Paragraph paints prefix and text alike
        let fg = |rows: &Vec<ratatui::text::Line<'static>>| rows[0].style.fg;
        // info: `· ` muted
        assert_eq!(text(&info), "· steering this turn");
        assert_eq!(fg(&info), Some(crate::palette::META));
        // warn: `⚠ ` salmon
        assert_eq!(text(&warn), "⚠ turn running");
        assert_eq!(fg(&warn), Some(crate::palette::WARN));
        // err: `! ` bold — the loudest system row
        assert_eq!(text(&err), "! 429 too many requests");
        assert_eq!(fg(&err), Some(crate::palette::ERR));
        assert!(
            err[0]
                .style
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD)
        );
    }

    #[test]
    fn shell_rows_render_plain_fg() {
        let rows = super::render_line(&Line::Shell("hello world".into()), 40, false);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].spans[0].content.as_ref(), "hello world");
        assert_eq!(rows[0].spans[0].style.fg, Some(crate::palette::FG));
    }

    #[test]
    fn summary_rows_render_tone_glyph_and_muted_text() {
        let row = |glyph, tone| {
            super::render_line(
                &Line::Summary {
                    glyph,
                    tone,
                    text: "done · 2.0s".into(),
                },
                40,
                false,
            )
        };
        for (glyph, tone, fg) in [
            ('✓', SummaryTone::Ok, crate::palette::OK),
            ('◐', SummaryTone::Warn, crate::palette::WARN),
        ] {
            let rows = row(glyph, tone);
            let first = &rows[0].spans[0];
            assert_eq!(first.content.as_ref(), format!("{glyph} "));
            assert_eq!(first.style.fg, Some(fg));
            assert_eq!(rows[0].spans[1].style.fg, Some(crate::palette::META));
        }
        // the error tone is bold so failures outweigh every other row
        let rows = row('✗', SummaryTone::Err);
        assert_eq!(rows[0].spans[0].style.fg, Some(crate::palette::ERR));
        assert!(
            rows[0].spans[0]
                .style
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD)
        );
    }

    #[test]
    fn row_refs_map_toolblock_rows() {
        let mut t = Transcript::default();
        t.push(Line::Thought("deep\nthoughts".into()));
        t.push_tool_call(ToolCall {
            head: "→ a".into(),
            ..no_note_call()
        });
        t.push_tool_call(ToolCall {
            head: "→ b".into(),
            ..no_note_call()
        });
        // rows 0..2 belong to the thought entry (2 rendered rows + air)
        let air = t.rendered_rows_of(0);
        assert_eq!(t.row_ref_at(0), Some(RowRef::Entry(0)));
        assert_eq!(t.row_ref_at(air - 1), Some(RowRef::Entry(0)));
        // the blank separator between the families reads as its own entry
        assert_eq!(t.row_ref_at(air), Some(RowRef::Entry(1)));
        // the block's two rows map to their call index
        assert_eq!(t.row_ref_at(air + 1), Some(RowRef::ToolCall(2, 0)));
        assert_eq!(t.row_ref_at(air + 2), Some(RowRef::ToolCall(2, 1)));
        assert_eq!(
            t.tool_call(2, 1).map(|c| c.head.as_str()),
            Some("→ b"),
            "the click source resolves the call"
        );
    }

    #[test]
    fn tool_call_expands_full_content_inline() {
        let call = ToolCall {
            head: "→ bash · cargo build".into(),
            ok: true,
            note: "first line".into(),
            excerpt: "first line\n\nthird line".into(),
            spill: None,
            dur: Some(1.25),
            expanded: false,
        };
        // collapsed: one header row with the fold marker
        let out = super::render_line(&Line::ToolBlock(vec![call.clone()]), 40, false);
        assert_eq!(out.len(), 1, "collapsed = one row");
        let text: String = out[0].spans.iter().map(|s| s.content.to_string()).collect();
        assert!(text.starts_with(" │ ▸ → bash"), "{text:?}");
        // expanded: header (▾) + every excerpt line, blank line included,
        // each railed and faint
        let expanded = ToolCall {
            expanded: true,
            ..call
        };
        let out = super::render_line(&Line::ToolBlock(vec![expanded]), 40, false);
        assert_eq!(out.len(), 4, "header + 3 excerpt lines: {out:?}");
        let texts: Vec<String> = out
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
            .collect();
        assert!(texts[0].starts_with(" │ ▾ → bash"), "{texts:?}");
        assert_eq!(texts[1], " │   first line");
        assert_eq!(texts[2], " │   ");
        assert_eq!(texts[3], " │   third line");
        assert_eq!(out[1].spans[2].style.fg, Some(crate::palette::FAINT));
        // long lines hard-continue with a deeper prefix
        let long = ToolCall {
            excerpt: "x".repeat(100),
            expanded: true,
            ..no_note_call()
        };
        let out = super::render_line(&Line::ToolBlock(vec![long]), 20, false);
        assert!(out.len() > 2, "wrapped into continuations");
        for row in &out[1..] {
            assert!(row.spans[1].content.starts_with(' '));
        }
    }

    #[test]
    fn expanded_call_points_at_the_spill_file() {
        let call = ToolCall {
            head: "→ bash".into(),
            excerpt: "tail of the run".into(),
            spill: Some("/tmp/spill-1".into()),
            expanded: true,
            ..no_note_call()
        };
        let out = super::render_line(&Line::ToolBlock(vec![call]), 60, false);
        let last = out.last().unwrap();
        let text: String = last.spans.iter().map(|s| s.content.to_string()).collect();
        assert!(
            text.contains("spill file") && text.contains("/spills"),
            "{text:?}"
        );
        // the trailer rides the muted tier, not the content tier
        assert_eq!(
            last.spans.last().unwrap().style.fg,
            Some(crate::palette::META)
        );
    }

    #[test]
    fn toggling_expands_and_collapses_in_place() {
        let mut t = Transcript::default();
        t.set_width(40);
        t.push_tool_call(ToolCall {
            head: "→ bash".into(),
            excerpt: "line one\nline two".into(),
            expanded: false,
            ..no_note_call()
        });
        let (entry, call) = t.last_tool_ref().expect("ref for the fresh block");
        assert_eq!(t.rendered_rows_of(entry), 1, "collapsed: one header row");
        // expand: header + two content rows, flag lives on the call
        t.toggle_tool_call(entry, call);
        assert!(t.tool_call(entry, call).unwrap().expanded, "flag flipped");
        assert_eq!(t.rendered_rows_of(entry), 3, "header + 2 content rows");
        let row_text = |r: usize| -> String {
            t.row(r)
                .unwrap()
                .spans
                .iter()
                .map(|s| s.content.to_string())
                .collect()
        };
        assert_eq!(row_text(1), " │   line one");
        assert_eq!(row_text(2), " │   line two");
        // collapse again: back to one row
        t.toggle_tool_call(entry, call);
        assert!(!t.tool_call(entry, call).unwrap().expanded);
        assert_eq!(t.rendered_rows_of(entry), 1);
        // stale indices (rewound past the block) no-op instead of panicking
        t.toggle_tool_call(99, 0);
        assert_eq!(t.rendered_rows_of(entry), 1);
        // the fold flag survives a width rebuild (it rides the entry data)
        t.toggle_tool_call(entry, call);
        t.set_width(72);
        assert!(
            t.tool_call(entry, call).unwrap().expanded,
            "flag survives resize"
        );
    }

    #[test]
    fn pad_to_width_uses_display_columns() {
        assert_eq!(pad_to_width("ab".into(), 5), "ab   ");
        assert_eq!(pad_to_width("世界".into(), 5), "世界 ");
        assert_eq!(pad_to_width("toolong".into(), 3), "toolong");
    }

    #[test]
    fn queue_head_auto_sends_fifo_and_drains() {
        let mut queue = vec!["first".to_string(), "second".to_string()];
        assert_eq!(pop_queue_head(&mut queue).as_deref(), Some("first"));
        assert_eq!(pop_queue_head(&mut queue).as_deref(), Some("second"));
        assert_eq!(pop_queue_head(&mut queue), None, "empty queue: no send");
    }

    #[test]
    fn busy_title_carries_only_the_queue_hint() {
        // action hints moved to the status bar; the title keeps state only
        assert_eq!(busy_input_title(0), "input");
        assert_eq!(busy_input_title(2), "input · 2 queued");
    }

    #[test]
    fn input_area_rows_picker_and_draft_only() {
        assert_eq!(input_area_rows(None, "long\ndraft\ntext", 40), 3);
        assert_eq!(input_area_rows(None, "one line", 40), 1);
        // the draft arm folds long lines: 200 cols at width 40 → 5 rows
        assert_eq!(input_area_rows(None, &"x".repeat(200), 40), 5);
        // the picker borrows the box for its four tier rows
        assert_eq!(
            input_area_rows(
                Some(&ModePicker::for_mode(ka_protocol::Mode::Free)),
                "x",
                40
            ),
            4
        );
    }

    #[test]
    fn ask_renders_numbered_modal_options() {
        let ask = PendingAsk {
            id: AskId("a".into()),
            question: "allow write to modify files?".into(),
            options: vec!["allow".into(), "deny".into()],
            detail: Some("+a\n".into()),
            selected: 0,
        };
        let rows = ask_modal_body(&ask, 40);
        let text = |l: &ratatui::text::Line<'static>| {
            l.spans
                .iter()
                .map(|s| s.content.to_string())
                .collect::<String>()
        };
        // bold question, then the budgeted detail, then numbered options
        assert_eq!(text(&rows[0]), "allow write to modify files?");
        assert_eq!(text(&rows[1]), "+a");
        // the selected option wears the full-row pink bar
        assert_eq!(text(&rows[2]), pad_to_width("1 allow".to_string(), 40));
        assert_eq!(text(&rows[3]), "2 deny");
        assert_eq!(rows[2].spans[0].style, selection_style());
        assert_ne!(rows[3].spans[0].style, selection_style());
        // the bar follows the selection
        let ask = PendingAsk { selected: 1, ..ask };
        let rows = ask_modal_body(&ask, 40);
        assert_eq!(rows[3].spans[0].style, selection_style());
        assert_ne!(rows[2].spans[0].style, selection_style());
    }

    #[test]
    fn ask_detail_rows_colorize_and_clamp() {
        let rows = ask_detail_rows(
            "--- a/f.rs\n+++ b/f.rs\n@@ -1,3 +1,3 @@\n context\n-removed\n+added\n",
            8,
        );
        let fg = |l: &ratatui::text::Line<'static>| l.spans[0].style.fg;
        assert_eq!(rows[0].spans[0].content.as_ref(), "--- a/f.rs");
        assert_eq!(fg(&rows[0]), Some(crate::palette::FAINT));
        assert_eq!(fg(&rows[1]), Some(crate::palette::FAINT));
        assert_eq!(fg(&rows[2]), Some(crate::palette::META));
        assert_eq!(fg(&rows[3]), None, "context stays default");
        assert_eq!(fg(&rows[4]), Some(crate::palette::ERR));
        assert_eq!(fg(&rows[5]), Some(crate::palette::OK));
        // clamp keeps the budget and notes the remainder
        let clamped = ask_detail_rows("ctx\n+added\n-removed\n", 2);
        assert_eq!(clamped.len(), 3);
        assert!(
            clamped[2].spans[0].content.contains("1 more"),
            "{:?}",
            clamped[2].spans[0].content
        );
        // the budget keeps trailer headroom: 2 allowed rows means the
        // third detail row becomes the `… +N more` trailer
        assert_eq!(clamped.len(), 3, "trailer rides in the budget");
    }

    #[test]
    fn wrap_rows_breaks_at_spaces_hard_tokens_and_newlines() {
        // space-preferred breaks
        let rows = wrap_rows("alpha beta gamma", 6);
        let texts: Vec<&str> = rows.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(texts, vec!["alpha", "beta", "gamma"]);
        assert_eq!(rows[0].start, 0);
        assert_eq!(rows[1].start, 6, "continuation starts after the space");

        // hard break: a token with no space still wraps
        let rows = wrap_rows("abcdefghijkl", 5);
        let texts: Vec<&str> = rows.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(texts, vec!["abcde", "fghij", "kl"]);

        // wide chars count as two display columns
        let rows = wrap_rows("世界 world", 5);
        assert_eq!(rows[0].text, "世界", "{}", rows[0].text);
        assert_eq!(rows[1].text, "world");

        // explicit newlines always break; empty lines stay rows
        let rows = wrap_rows("a\n\nb", 10);
        let texts: Vec<&str> = rows.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(texts, vec!["a", "", "b"]);
        assert_eq!(rows[2].start, 3);
    }

    #[test]
    fn visual_cursor_row_maps_wrapped_positions() {
        let rows = wrap_rows("alpha beta", 6);
        assert_eq!(visual_cursor_row(&rows, 0), 0);
        assert_eq!(visual_cursor_row(&rows, 5), 0, "end of 'alpha'");
        assert_eq!(visual_cursor_row(&rows, 6), 1, "cursor after the space");
        assert_eq!(visual_cursor_row(&rows, 10), 1);
    }
    // ── sidebar ──────────────────────────────────────────────────
    use unicode_width::UnicodeWidthStr;

    fn meters_sample() -> Meters {
        Meters {
            model: "mockco/mock".into(),
            mode: "free".into(),
            effort: String::new(),
            session: "s19a4f2e1b0-3f9c2a81d4b7".into(),
            context: (12_000, 200_000),
            cost: 0.0123,
            cache_hit: None,
            ..Default::default()
        }
    }

    fn plain_text(rows: &[ratatui::text::Line<'static>]) -> Vec<String> {
        rows.iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn info_rows_reports_session_facts() {
        let sidebar = SidebarState {
            cwd: "\u{2026}/projects/ka".into(),
            branch: Some("main".into()),
            ..Default::default()
        };
        let rows = info_rows(&sidebar, &meters_sample(), 60);
        let text = plain_text(&rows);
        assert!(
            text.contains(&"\u{2026}/projects/ka:main".to_string()),
            "{text:?}"
        );
        assert!(text.contains(&"session #3f9c2a81".to_string()), "{text:?}");
        assert!(text.contains(&"model mockco/mock".to_string()), "{text:?}");
        assert!(text.contains(&"cost $0.0123".to_string()), "{text:?}");
        assert!(
            text.iter().any(|t| t.starts_with("ctx 12000/200000")),
            "{text:?}"
        );
    }

    #[test]
    fn todos_rows_mark_done_and_next() {
        let sidebar = SidebarState {
            todos: vec![
                ka_protocol::TodoItem {
                    text: "survey".into(),
                    state: ka_protocol::TodoState::Done,
                },
                ka_protocol::TodoItem {
                    text: "implement".into(),
                    state: ka_protocol::TodoState::Pending,
                },
            ],
            ..Default::default()
        };
        let rows = todos_rows(&sidebar, 60);
        let text = plain_text(&rows);
        let done = text
            .iter()
            .find(|t| t.contains("survey"))
            .expect("done row");
        assert!(done.starts_with("\u{2713} "), "{done}");
        let pending = text
            .iter()
            .find(|t| t.contains("implement"))
            .expect("pending row");
        assert!(pending.starts_with("\u{b7} "), "{pending}");
        // done rows carry the crossed-out modifier, the first pending
        // row is accented bold ('next')
        let done_idx = rows
            .iter()
            .position(|l| l.spans.iter().any(|s| s.content.contains("survey")))
            .unwrap();
        assert!(rows[done_idx].spans.iter().any(|s| {
            s.style
                .add_modifier
                .contains(ratatui::style::Modifier::CROSSED_OUT)
        }));
        let pend_idx = rows
            .iter()
            .position(|l| l.spans.iter().any(|s| s.content.contains("implement")))
            .unwrap();
        assert!(rows[pend_idx].spans.iter().any(|s| {
            s.style
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD)
                && s.style.fg == Some(crate::palette::ACCENT)
        }));
        // empty state explains itself instead of rendering nothing
        let empty = todos_rows(&SidebarState::default(), 60);
        assert!(plain_text(&empty)[0].contains("no live todo list"));
    }

    #[test]
    fn inventory_rows_list_skills_agents_and_mcp_health() {
        let sidebar = SidebarState {
            inventory: Inventory {
                mcp: vec![
                    ka_protocol::McpSummary {
                        name: "demo".into(),
                        ok: true,
                        tools: 2,
                    },
                    ka_protocol::McpSummary {
                        name: "jira".into(),
                        ok: false,
                        tools: 0,
                    },
                ],
                skills: vec!["rust-docs".into()],
                agents: vec!["scout".into()],
                prompts: Vec::new(),
                ..Default::default()
            },
            ..Default::default()
        };
        let text = plain_text(&inventory_rows(&sidebar, 60));
        assert!(text.contains(&"rust-docs".to_string()), "{text:?}");
        assert!(text.contains(&"scout".to_string()), "{text:?}");
        assert!(text.iter().any(|t| t == "demo \u{2713} 2"), "{text:?}");
        assert!(text.iter().any(|t| t == "jira \u{2717}"), "{text:?}");
    }

    #[test]
    fn popup_rows_truncate_multibyte_to_width() {
        let sidebar = SidebarState {
            cwd: "\u{2026}/\u{4e16}\u{754c}/\u{4e16}\u{754c}".into(),
            branch: Some("main".into()),
            todos: vec![ka_protocol::TodoItem {
                text: "\u{4e16}\u{754c}\u{4e16}\u{754c}\u{4e16}\u{754c}".into(),
                state: ka_protocol::TodoState::Pending,
            }],
            ..Default::default()
        };
        for row in info_rows(&sidebar, &Meters::default(), 10) {
            let text: String = row.spans.iter().map(|s| s.content.as_ref()).collect();
            assert!(text.width() <= 10, "{text}");
        }
        for row in todos_rows(&sidebar, 10) {
            let text: String = row.spans.iter().map(|s| s.content.as_ref()).collect();
            assert!(text.width() <= 10, "{text}");
        }
    }

    #[test]
    fn transcript_width_is_full_width_minus_chrome() {
        // margin cols on each side + the paragraph's side padding; no
        // sidebar column anywhere in the layout anymore
        assert_eq!(transcript_width(120), 116);
        assert_eq!(transcript_width(100), 96);
        assert_eq!(transcript_width(60), 56, "never underflows");
    }

    #[test]
    fn trunc_cols_is_width_aware() {
        assert_eq!(trunc_cols("short", 24), "short");
        let cut = trunc_cols("世界世界世界", 7);
        assert!(cut.width() <= 7, "{cut}");
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn apply_event_updates_sidebar_inventory_and_todos() {
        let mut lines = Transcript::default();
        let mut busy = false;
        let mut busy_since = None;
        let mut meters = Meters::default();
        let mut pending = None;
        let mut turn_produced = false;
        let mut usage = None;
        let mut a = String::new();
        let mut t = String::new();
        let mut tool = String::new();
        let mut live_tool: Option<LiveTool> = None;
        let mut last_user: Option<String> = None;
        let mut last_error: Option<String> = None;
        let mut spills: Vec<String> = Vec::new();
        let mut sidebar = SidebarState::default();

        let _ = apply_event(
            &Event::Inventory {
                tools: vec!["read".into()],
                mcp: vec![ka_protocol::McpSummary {
                    name: "demo".into(),
                    ok: true,
                    tools: 2,
                }],
                agents: vec!["coder".into()],
                skills: vec!["rust-docs".into()],
                prompts: Vec::new(),
            },
            &mut lines,
            &mut busy,
            &mut busy_since,
            &mut meters,
            &mut pending,
            &mut turn_produced,
            &mut usage,
            &mut a,
            &mut t,
            &mut tool,
            &mut live_tool,
            &mut last_user,
            &mut last_error,
            &mut spills,
            &mut sidebar,
        );
        assert_eq!(sidebar.inventory.tools, vec!["read".to_string()]);
        assert_eq!(sidebar.inventory.skills, vec!["rust-docs".to_string()]);

        let _ = apply_event(
            &Event::Todos {
                items: vec![ka_protocol::TodoItem {
                    text: "dig".into(),
                    state: ka_protocol::TodoState::Done,
                }],
            },
            &mut lines,
            &mut busy,
            &mut busy_since,
            &mut meters,
            &mut pending,
            &mut turn_produced,
            &mut usage,
            &mut a,
            &mut t,
            &mut tool,
            &mut live_tool,
            &mut last_user,
            &mut last_error,
            &mut spills,
            &mut sidebar,
        );
        assert_eq!(sidebar.todos.len(), 1);
        assert_eq!(sidebar.todos[0].state, ka_protocol::TodoState::Done);
        // whole-list replacement: a follow-up event supersedes
        let _ = apply_event(
            &Event::Todos { items: Vec::new() },
            &mut lines,
            &mut busy,
            &mut busy_since,
            &mut meters,
            &mut pending,
            &mut turn_produced,
            &mut usage,
            &mut a,
            &mut t,
            &mut tool,
            &mut live_tool,
            &mut last_user,
            &mut last_error,
            &mut spills,
            &mut sidebar,
        );
        assert!(sidebar.todos.is_empty());
    }
    #[test]
    fn apply_event_title_sets_sidebar_session_title() {
        let mut lines = Transcript::default();
        let mut busy = false;
        let mut busy_since = None;
        let mut meters = Meters::default();
        let mut pending = None;
        let mut turn_produced = false;
        let mut usage = None;
        let mut a = String::new();
        let mut t = String::new();
        let mut tool = String::new();
        let mut live_tool: Option<LiveTool> = None;
        let mut last_user: Option<String> = None;
        let mut last_error: Option<String> = None;
        let mut spills: Vec<String> = Vec::new();
        let mut sidebar = SidebarState::default();
        let mut feed = |evt: &Event, sidebar: &mut SidebarState| {
            let _ = apply_event(
                evt,
                &mut lines,
                &mut busy,
                &mut busy_since,
                &mut meters,
                &mut pending,
                &mut turn_produced,
                &mut usage,
                &mut a,
                &mut t,
                &mut tool,
                &mut live_tool,
                &mut last_user,
                &mut last_error,
                &mut spills,
                sidebar,
            );
        };

        feed(
            &Event::Title {
                title: "Fix the parser".into(),
            },
            &mut sidebar,
        );
        assert_eq!(sidebar.title.as_deref(), Some("Fix the parser"));
        // empty titles (additive default) never blank a real one
        feed(
            &Event::Title {
                title: String::new(),
            },
            &mut sidebar,
        );
        assert_eq!(sidebar.title.as_deref(), Some("Fix the parser"));

        assert_eq!(sidebar.title.as_deref(), Some("Fix the parser"));
    }

    #[test]
    fn thinking_blocks_collapse_to_one_row_and_expand() {
        let thought = Line::Thought("first line\nsecond line\nthird line".into());
        // collapsed (default): the first line plus a count marker, one row
        let collapsed = super::render_line(&thought, 60, false);
        assert_eq!(collapsed.len(), 1, "{collapsed:?}");
        let text: String = collapsed[0]
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert!(text.contains("first line"), "{text}");
        assert!(text.contains("… +3"), "{text}");
        assert!(text.contains("▸"), "collapsed marker: {text}");
        assert!(
            !text.contains("second line"),
            "hidden when collapsed: {text}"
        );

        // open: every line renders
        let open = super::render_line(&thought, 60, true);
        let text: String = open
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("first line") && text.contains("third line"),
            "{text}"
        );

        // single-line thoughts never collapse
        let one = super::render_line(&Line::Thought("just one".into()), 60, false);
        let text: String = one[0].spans.iter().map(|s| s.content.to_string()).collect();
        assert!(text.contains("just one") && !text.contains("(+1"), "{text}");

        // the toggle rebuilds the cache: row counts follow the flag
        let mut t = Transcript::default();
        t.set_width(60);
        t.push(thought);
        assert_eq!(t.total_rows(), 1, "collapsed by default");
        t.set_thoughts_open(true);
        assert_eq!(t.total_rows(), 3, "expanded after the toggle");
        t.set_thoughts_open(false);
        assert_eq!(t.total_rows(), 1, "collapsed again");
    }
    #[test]
    fn tool_row_verdict_right_aligned() {
        let call = ToolCall {
            head: "→ bash · cargo build".into(),
            ok: true,
            note: "done".into(),
            excerpt: "done".into(),
            spill: None,
            dur: Some(1.25),
            expanded: false,
        };
        let out = super::render_line(&Line::ToolBlock(vec![call]), 40, false);
        assert_eq!(out.len(), 1, "one call = one row, no wrap");
        let row = &out[0];
        let text: String = row.spans.iter().map(|s| s.content.to_string()).collect();
        // rail + fold marker on the left, verdict glyph at the right edge
        assert!(text.starts_with(" │ ▸ → bash"), "{text:?}");
        assert!(text.ends_with('✓'), "{text:?}");
        assert_eq!(text.chars().count(), 40, "row fills exactly the width");
        // the verdict is OK-colored; the duration right before it is faint
        let last = row.spans.last().unwrap();
        assert_eq!(last.content.as_ref(), "✓");
        assert_eq!(last.style.fg, Some(crate::palette::OK));
        let dur = &row.spans[row.spans.len() - 2];
        assert_eq!(dur.content.as_ref(), "1.2s ");
        assert_eq!(dur.style.fg, Some(crate::palette::FAINT));
        // a failed call flips the verdict to the red ✗
        let fail = ToolCall {
            ok: false,
            ..no_note_call()
        };
        let out = super::render_line(&Line::ToolBlock(vec![fail]), 20, false);
        let last = out[0].spans.last().unwrap();
        assert_eq!(last.content.as_ref(), "✗");
        assert_eq!(last.style.fg, Some(crate::palette::ERR));
        // sub-tenth durations and replayed calls omit the duration
        let quiet = ToolCall {
            head: "→ x".into(),
            dur: Some(0.04),
            ..no_note_call()
        };
        let out = super::render_line(&Line::ToolBlock(vec![quiet]), 20, false);
        let text: String = out[0].spans.iter().map(|s| s.content.to_string()).collect();
        assert!(!text.contains("0.0s"), "{text:?}");
    }

    /// Every overlay speaks one dialect: rounded corners. Opens each
    /// modal on a test backend and checks its top-left corner glyph.
    #[test]
    fn all_modals_rounded() {
        use ratatui::backend::TestBackend;
        let mk = |modal: Modal| -> ratatui::Terminal<TestBackend> {
            let mut terminal = ratatui::Terminal::new(TestBackend::new(120, 40)).unwrap();
            terminal
                .draw(|f| {
                    let mut t = Transcript::default();
                    super::render(
                        f,
                        &mut t,
                        None,
                        "",
                        0,
                        false,
                        None,
                        Instant::now(),
                        0,
                        None,
                        None,
                        None,
                        None,
                        None,
                        Some(&modal),
                        None,
                        &Meters::default(),
                        &SidebarState::default(),
                        false,
                        true,
                        "◆",
                        &std::cell::Cell::new(None::<StripZones>),
                        None,
                        &std::cell::Cell::new(None::<TitleArrows>),
                        &std::cell::Cell::new(None::<(ratatui::layout::Rect, usize)>),
                        &std::cell::Cell::new(0u16),
                    )
                })
                .unwrap();
            terminal
        };
        let modals = vec![
            Modal::Session(SessionPicker {
                sessions: vec![],
                selected: 0,
                filter: String::new(),
                current: None,
            }),
            Modal::Help,
            Modal::Model(ModelPicker {
                models: vec![],
                vendor: None,
                configured_only: false,
                selected: 0,
                filter: String::new(),
            }),
            Modal::Provider(ProviderPicker {
                providers: vec![],
                counts: vec![],
                selected: 0,
                filter: String::new(),
            }),
            Modal::Key(KeyPrompt {
                provider: "mockco".into(),
                env_var: "MOCK_KEY".into(),
                doc_url: String::new(),
                drill: None,
                input: String::new(),
                pending_model: None,
            }),
            Modal::Spills {
                items: vec![],
                selected: 0,
            },
            Modal::Prompts {
                items: vec![],
                selected: 0,
            },
            Modal::Rewind {
                items: vec![(1, "hello".into())],
                selected: 0,
            },
            Modal::Memory {
                rows: vec![],
                inbox: vec![],
                selected: 0,
            },
            Modal::Usage { rows: vec![] },
            Modal::Context { rows: vec![] },
            Modal::Tree {
                items: vec![],
                targets: vec![],
                selected: 0,
            },
            Modal::Tasks {
                entries: vec![],
                selected: 0,
            },
            Modal::TaskDetail {
                id: 1,
                text: "x".into(),
                scroll: None,
            },
            Modal::Debug {
                rows: vec![],
                scroll: None,
            },
            Modal::Todos { rows: vec![] },
            Modal::Skills { rows: vec![] },
            Modal::Info { rows: vec![] },
        ];
        for m in modals {
            let terminal = mk(m.clone());
            let buf = terminal.backend().buffer();
            // find the modal's top border row: a `╭` somewhere on the frame
            let has_rounded_top_left = (0..buf.area.width).any(|x| buf[(x, 0)].symbol() == "╭")
                || (1..buf.area.height.saturating_sub(8))
                    .any(|y| (0..buf.area.width).any(|x| buf[(x, y)].symbol() == "╭"));
            assert!(has_rounded_top_left, "{m:?} must draw a rounded corner");
        }
    }

    #[test]
    fn title_row_shows_session_title() {
        use ratatui::backend::TestBackend;
        let frame_title = |sidebar: &SidebarState| -> String {
            let mut terminal = ratatui::Terminal::new(TestBackend::new(120, 40)).unwrap();
            terminal
                .draw(|f| {
                    let mut t = Transcript::default();
                    super::render(
                        f,
                        &mut t,
                        None,
                        "",
                        0,
                        false,
                        None,
                        Instant::now(),
                        0,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        &Meters::default(),
                        sidebar,
                        true,
                        true,
                        "◆",
                        &std::cell::Cell::new(None::<StripZones>),
                        None,
                        &std::cell::Cell::new(None::<TitleArrows>),
                        &std::cell::Cell::new(None::<(ratatui::layout::Rect, usize)>),
                        &std::cell::Cell::new(0u16),
                    )
                })
                .unwrap();
            (1..100u16)
                .map(|x| terminal.backend().buffer()[(x, 1)].symbol())
                .collect()
        };
        let mut sidebar = SidebarState::default();
        assert!(
            frame_title(&sidebar).contains('◆'),
            "bare glyph with no title"
        );
        sidebar.title = Some("refactor the login flow".into());
        let row = frame_title(&sidebar);
        assert!(row.contains("◆ refactor the login flow"), "{row}");
    }

    #[test]
    fn strip_buttons_bright_label_faint_key() {
        use ratatui::backend::TestBackend;
        let mut terminal = ratatui::Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal
            .draw(|f| {
                let mut t = Transcript::default();
                super::render(
                    f,
                    &mut t,
                    None,
                    "",
                    0,
                    false,
                    None,
                    Instant::now(),
                    0,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    &Meters::default(),
                    &SidebarState::default(),
                    true,
                    true,
                    "◆",
                    &std::cell::Cell::new(None::<StripZones>),
                    None,
                    &std::cell::Cell::new(None::<TitleArrows>),
                    &std::cell::Cell::new(None::<(ratatui::layout::Rect, usize)>),
                    &std::cell::Cell::new(0u16),
                )
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        let strip: String = (0..30u16).map(|x| buf[(x, 38)].symbol()).collect();
        assert!(strip.contains("todos"), "{strip}");
        // locate the label vs its key hint, compare fg tones
        let todos_x = strip.find('t').unwrap() as u16;
        let alt_x = strip.find("alt").unwrap() as u16;
        let label_fg = buf[(todos_x, 38)].fg;
        let hint_fg = buf[(alt_x, 38)].fg;
        assert_eq!(label_fg, crate::palette::FG_STRONG);
        assert_eq!(hint_fg, crate::palette::FAINT);
        // the separator rides the quiet border tone
        let sep_x = strip.find('│').unwrap() as u16;
        assert_eq!(buf[(sep_x, 38)].fg, crate::palette::BORDER_QUIET);
    }

    #[test]
    fn toast_renders_and_expires() {
        use ratatui::backend::TestBackend;
        let draw = |toast: Option<&str>| -> (String, usize) {
            let mut terminal = ratatui::Terminal::new(TestBackend::new(120, 40)).unwrap();
            let mut rows_with = 0;
            terminal
                .draw(|f| {
                    let mut t = Transcript::default();
                    super::render(
                        f,
                        &mut t,
                        None,
                        "",
                        0,
                        false,
                        None,
                        Instant::now(),
                        0,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        &Meters::default(),
                        &SidebarState::default(),
                        false,
                        true,
                        "◆",
                        &std::cell::Cell::new(None::<StripZones>),
                        toast,
                        &std::cell::Cell::new(None::<TitleArrows>),
                        &std::cell::Cell::new(None::<(ratatui::layout::Rect, usize)>),
                        &std::cell::Cell::new(0u16),
                    )
                })
                .unwrap();
            let buf = terminal.backend().buffer();
            let text: String = buf.content.iter().map(|c| c.symbol().to_string()).collect();
            let _ = &mut rows_with;
            (text, rows_with)
        };
        let (with_toast, _) = draw(Some("✓ copied last reply"));
        assert!(
            with_toast.contains("copied last reply"),
            "toast visible while fresh"
        );
        let (without, _) = draw(None);
        assert!(
            !without.contains("copied last reply"),
            "no toast row without a toast"
        );
        // the run loop only passes the message while younger than the TTL
        let mut toast = Some(("hi".to_string(), Instant::now()));
        assert!(
            toast
                .as_ref()
                .is_some_and(|(_, at)| at.elapsed() < TOAST_TTL)
        );
        toast = Some((
            "hi".to_string(),
            Instant::now() - TOAST_TTL - Duration::from_secs(1),
        ));
        assert!(
            toast
                .as_ref()
                .is_none_or(|(_, at)| at.elapsed() >= TOAST_TTL)
        );
    }

    #[test]
    fn mouse_toggle_toasts() {
        // the toggle announces itself with the persisted mode names
        assert!(mouse_mode_text(true).contains("capture"));
        assert!(mouse_mode_text(false).contains("native"));
    }

    #[test]
    fn slash_popup_selection_stays_visible() {
        use ratatui::backend::TestBackend;
        let items: Vec<(String, String)> = (0..30)
            .map(|i| (format!("/cmd{i:02}"), format!("desc {i}")))
            .collect();
        let draw = |selected: usize| -> String {
            let popup = SlashPopup {
                items: items.clone(),
                selected,
            };
            let mut terminal = ratatui::Terminal::new(TestBackend::new(120, 40)).unwrap();
            terminal
                .draw(|f| {
                    let mut t = Transcript::default();
                    super::render(
                        f,
                        &mut t,
                        None,
                        "/c",
                        2,
                        false,
                        None,
                        Instant::now(),
                        0,
                        None,
                        None,
                        Some(&popup),
                        None,
                        None,
                        None,
                        None,
                        &Meters::default(),
                        &SidebarState::default(),
                        false,
                        true,
                        "◆",
                        &std::cell::Cell::new(None::<StripZones>),
                        None,
                        &std::cell::Cell::new(None::<TitleArrows>),
                        &std::cell::Cell::new(None::<(ratatui::layout::Rect, usize)>),
                        &std::cell::Cell::new(0u16),
                    )
                })
                .unwrap();
            terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol().to_string())
                .collect()
        };
        // selection 0: /cmd00 visible
        assert!(draw(0).contains("/cmd00"));
        // deep in the list: the selected row is on screen, the top of the
        // list is scrolled away — the old take(7) render lost the selection
        let deep = draw(24);
        assert!(deep.contains("/cmd24"), "selected row must be visible");
        assert!(!deep.contains("/cmd00"), "list is windowed");
        // bottom of the list
        assert!(draw(29).contains("/cmd29"));
    }

    #[test]
    fn frame_has_top_margin_glyph_title_and_rounded_input() {
        use ratatui::backend::TestBackend;
        let mut terminal = ratatui::Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal
            .draw(|f| {
                let mut t = Transcript::default();
                super::render(
                    f,
                    &mut t,
                    None,
                    "",
                    0,
                    false,
                    None,
                    Instant::now(),
                    0,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    &Meters::default(),
                    &SidebarState::default(),
                    true,
                    true,
                    "◆",
                    &std::cell::Cell::new(None::<StripZones>),
                    None,
                    &std::cell::Cell::new(None::<TitleArrows>),
                    &std::cell::Cell::new(None::<(ratatui::layout::Rect, usize)>),
                    &std::cell::Cell::new(0u16),
                )
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        // (a) the first UI row is empty canvas
        for x in 0..120u16 {
            let cell = &buf[(x, 0)];
            assert_eq!(cell.symbol(), " ", "row 0 col {x} must be blank");
            assert_eq!(cell.bg, crate::palette::BG, "row 0 rides the canvas");
        }
        // the transcript title row carries the header glyph (◆ default)
        let title_row: String = (1..93u16).map(|x| buf[(x, 1)].symbol()).collect();
        assert!(title_row.contains('◆'), "glyph title: {title_row}");
        // the chat owns the full width (no sidebar column): the title
        // row runs to the right margin
        let tx_end: String = (110..119u16).map(|x| buf[(x, 1)].symbol()).collect();
        assert!(tx_end.contains('─') || tx_end.contains('◆'), "{tx_end}");
        // the input box closes with rounded corners (rows 35..38)
        assert_eq!(buf[(1, 35)].symbol(), "╭");
        assert_eq!(buf[(118, 35)].symbol(), "╮");
        assert_eq!(buf[(1, 37)].symbol(), "╰");
        assert_eq!(buf[(118, 37)].symbol(), "╯");
        // the strip row carries the popup buttons
        let strip: String = (0..120u16).map(|x| buf[(x, 38)].symbol()).collect();
        assert!(
            strip.contains("todos") && strip.contains("skills") && strip.contains("info"),
            "{strip}"
        );
        // the status bar still owns the last row (no bottom margin)
        let last_row: String = (0..120u16).map(|x| buf[(x, 39)].symbol()).collect();
        assert!(last_row.contains("enter"), "status hints on the last row");
    }

    #[test]
    fn empty_idle_transcript_shows_the_welcome() {
        use ratatui::backend::TestBackend;
        let frame_text = |transcript: &mut Transcript, busy: bool, fresh: bool| -> String {
            let mut terminal = ratatui::Terminal::new(TestBackend::new(120, 40)).unwrap();
            terminal
                .draw(|f| {
                    super::render(
                        f,
                        transcript,
                        None,
                        "",
                        0,
                        busy,
                        None,
                        Instant::now(),
                        0,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        &Meters::default(),
                        &SidebarState::default(),
                        fresh,
                        true,
                        "◆",
                        &std::cell::Cell::new(None::<StripZones>),
                        None,
                        &std::cell::Cell::new(None::<TitleArrows>),
                        &std::cell::Cell::new(None::<(ratatui::layout::Rect, usize)>),
                        &std::cell::Cell::new(0u16),
                    )
                })
                .unwrap();
            terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol().to_string())
                .collect()
        };
        let frame = frame_text(&mut Transcript::default(), false, true);
        assert!(frame.contains("new conversation"), "welcome on fresh chat");
        assert!(frame.contains("/help keys"), "hint row present");
        // busy, carrying entries, or an older chat still replaying: the
        // welcome steps aside
        assert!(
            !frame_text(&mut Transcript::default(), true, true).contains("new conversation"),
            "busy turn: no welcome"
        );
        assert!(
            !frame_text(&mut Transcript::default(), false, false).contains("new conversation"),
            "older chat before its replay lands: no welcome"
        );
        let mut t = Transcript::default();
        t.set_width(80);
        t.push(Line::User("hello".into()));
        assert!(
            !frame_text(&mut t, false, true).contains("new conversation"),
            "carrying entries: no welcome"
        );
    }

    #[test]
    fn idle_hints_teach_the_active_mouse_mode() {
        use ratatui::backend::TestBackend;
        let draw = |captured: bool| -> String {
            // the info section renders `cwd:branch`; with a default
            // (empty) SidebarState every section is skipped, so seed the
            // cwd to make the sidebar visible at all
            let sidebar = SidebarState {
                cwd: "/tmp/ka".to_string(),
                branch: Some("main".to_string()),
                ..Default::default()
            };
            let mut terminal = ratatui::Terminal::new(TestBackend::new(120, 40)).unwrap();
            terminal
                .draw(|f| {
                    super::render(
                        f,
                        &mut Transcript::default(),
                        None,
                        "",
                        0,
                        false,
                        None,
                        Instant::now(),
                        0,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        &Meters::default(),
                        &sidebar,
                        false,
                        captured,
                        "◆",
                        &std::cell::Cell::new(None::<StripZones>),
                        None,
                        &std::cell::Cell::new(None::<TitleArrows>),
                        &std::cell::Cell::new(None::<(ratatui::layout::Rect, usize)>),
                        &std::cell::Cell::new(0u16),
                    )
                })
                .unwrap();
            terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol().to_string())
                .collect()
        };
        let captured = draw(true);
        assert!(
            captured.contains("⇧drag"),
            "captured mode teaches the shift bypass: {:?}",
            captured
                .chars()
                .filter(|c| *c != ' ')
                .take(400)
                .collect::<String>()
        );
        let native = draw(false);
        assert!(
            native.contains("drag") && native.contains("↑↓ scroll"),
            "native mode teaches plain drag + scroll: {native}"
        );
        assert!(
            !native.contains("⇧drag"),
            "no shift bypass advertised without capture"
        );
        // the bottom strip renders in both modes; cwd:branch rides the
        // right side (seeded on the state above)
        assert!(
            native.contains("todos alt+o")
                && native.contains("skills ^t")
                && native.contains("info alt+i"),
            "strip buttons visible in native mode: {native}"
        );
        assert!(
            native.contains("/tmp/ka:main"),
            "strip carries cwd:branch: {native}"
        );
        assert!(
            draw(true).contains("todos alt+o"),
            "strip visible in capture too"
        );
    }

    #[test]
    fn jump_to_user_message_steps_between_user_rows() {
        // user messages start at rows 5, 40, 100; total 200, visible 20
        let rows = [5usize, 40, 100];
        let mut scroll: Option<usize> = None;
        // pinned at the tail (start 180): up finds the newest message
        jump_to_user_message(&mut scroll, &rows, 200, 20, true);
        assert_eq!(scroll, Some(100));
        // down from there: nothing below the pinned tail stays pinned
        scroll = None;
        jump_to_user_message(&mut scroll, &rows, 200, 20, false);
        assert_eq!(scroll, None);
        // anchored mid-history: up = previous, down = next
        scroll = Some(50);
        jump_to_user_message(&mut scroll, &rows, 200, 20, true);
        assert_eq!(scroll, Some(40));
        scroll = Some(50);
        jump_to_user_message(&mut scroll, &rows, 200, 20, false);
        assert_eq!(scroll, Some(100));
        // exactly on a message start: strict comparison steps past it
        scroll = Some(40);
        jump_to_user_message(&mut scroll, &rows, 200, 20, false);
        assert_eq!(scroll, Some(100));
        // above the first message: up is a no-op
        scroll = Some(0);
        jump_to_user_message(&mut scroll, &rows, 200, 20, true);
        assert_eq!(scroll, Some(0));
        // past the last message, not pinned: down lands on the live tail
        scroll = Some(120);
        jump_to_user_message(&mut scroll, &rows, 200, 20, false);
        assert_eq!(scroll, None);
        // no user rows / zero visible: never moves
        scroll = Some(7);
        jump_to_user_message(&mut scroll, &[], 200, 20, true);
        assert_eq!(scroll, Some(7));
        scroll = Some(7);
        jump_to_user_message(&mut scroll, &rows, 200, 0, false);
        assert_eq!(scroll, Some(7));
    }

    #[test]
    fn wheel_steps_scroll_and_repin() {
        let mut scroll = None;
        line_up(&mut scroll, 100, 20);
        assert_eq!(scroll, Some(77), "three rows up from the pinned tail");
        line_down(&mut scroll, 100, 20);
        assert_eq!(scroll, None, "77+3 reaches the tail anchor: re-pins");
        scroll = Some(40);
        line_down(&mut scroll, 100, 20);
        assert_eq!(scroll, Some(43));
        line_down(&mut scroll, 10, 20);
        assert_eq!(scroll, None, "everything visible: always pinned");
    }

    #[test]
    fn strip_zone_hit_testing() {
        let zone = StripZones {
            todos: ratatui::layout::Rect {
                x: 1,
                y: 38,
                width: 11,
                height: 1,
            },
            skills: ratatui::layout::Rect {
                x: 15,
                y: 38,
                width: 9,
                height: 1,
            },
            info: ratatui::layout::Rect {
                x: 27,
                y: 38,
                width: 11,
                height: 1,
            },
        };
        assert_eq!(zone.hit(1, 38), Some(StripButton::Todos));
        assert_eq!(zone.hit(11, 38), Some(StripButton::Todos));
        assert_eq!(zone.hit(15, 38), Some(StripButton::Skills));
        assert_eq!(zone.hit(27, 38), Some(StripButton::Info));
        assert_eq!(zone.hit(14, 38), None, "gap between buttons");
        assert_eq!(zone.hit(15, 37), None, "row above the strip");
    }

    #[test]
    fn title_arrows_hit_testing() {
        let zone = TitleArrows {
            up: ratatui::layout::Rect {
                x: 88,
                y: 1,
                width: 2,
                height: 1,
            },
            down: ratatui::layout::Rect {
                x: 90,
                y: 1,
                width: 2,
                height: 1,
            },
        };
        assert_eq!(zone.hit(88, 1), Some(true), "up arrow cell");
        assert_eq!(zone.hit(89, 1), Some(true), "up zone is two cells wide");
        assert_eq!(zone.hit(90, 1), Some(false), "down arrow cell");
        assert_eq!(zone.hit(91, 1), Some(false), "down zone is two cells wide");
        assert_eq!(zone.hit(92, 1), None, "past the arrows");
        assert_eq!(zone.hit(88, 2), None, "row below the title");
    }

    #[test]
    fn title_arrows_render_and_record_click_zones() {
        use ratatui::backend::TestBackend;
        let mut t = Transcript::default();
        t.set_width(80);
        t.push(Line::User("hello".into()));
        let mut terminal = ratatui::Terminal::new(TestBackend::new(120, 40)).unwrap();
        let strip_zone: std::cell::Cell<Option<StripZones>> = std::cell::Cell::new(None);
        let title_arrows: std::cell::Cell<Option<TitleArrows>> = std::cell::Cell::new(None);
        terminal
            .draw(|f| {
                super::render(
                    f,
                    &mut t,
                    None,
                    "",
                    0,
                    false,
                    None,
                    Instant::now(),
                    0,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    &Meters::default(),
                    &SidebarState::default(),
                    true,
                    true,
                    "◆",
                    &strip_zone,
                    None,
                    &title_arrows,
                    &std::cell::Cell::new(None::<(ratatui::layout::Rect, usize)>),
                    &std::cell::Cell::new(0u16),
                )
            })
            .unwrap();
        let row: String = (0..120u16)
            .map(|x| terminal.backend().buffer()[(x, 1)].symbol())
            .collect();
        assert!(row.contains('▲') && row.contains('▼'), "{row}");
        assert!(title_arrows.get().is_some(), "click zones recorded");
    }

    #[test]
    fn live_band_stays_inside_the_transcript_across_resizes() {
        use ratatui::backend::TestBackend;
        // a cached tool row plus a live block streaming wide chars and a
        // 200-col token, while the terminal drags across the sidebar
        // threshold: the band must fold at each frame's own width (no
        // wrap-driven row drift, no band/surface bg outside the
        // transcript pane)
        let mut t = Transcript::default();
        t.push_tool_call(ToolCall {
            head: "→ bash · cargo build".into(),
            ..no_note_call()
        });
        let live = LiveBlock {
            thought: "thinking".into(),
            tool_header: "→ bash · cargo build".into(),
            live_tool: Some(LiveTool {
                id: "c1".into(),
                preview: vec![
                    format!("{} {}", "世界".repeat(30), "x".repeat(200)),
                    "second line".to_string(),
                ],
                last: None,
                started: Instant::now(),
            }),
            md: crate::markdown::render("# heading\ntext", 60),
        };
        for width in [99u16, 100, 101] {
            let mut terminal = ratatui::Terminal::new(TestBackend::new(width, 24)).unwrap();
            let sb_zone = std::cell::Cell::new(None::<StripZones>);
            terminal
                .draw(|f| {
                    super::render(
                        f,
                        &mut t,
                        None,
                        "",
                        0,
                        true,
                        Some(Instant::now()),
                        Instant::now(),
                        0,
                        None,
                        Some(&live),
                        None,
                        None,
                        None,
                        None,
                        None,
                        &Meters::default(),
                        &SidebarState::default(),
                        false,
                        true,
                        "◆",
                        &sb_zone,
                        None,
                        &std::cell::Cell::new(None::<TitleArrows>),
                        &std::cell::Cell::new(None::<(ratatui::layout::Rect, usize)>),
                        &std::cell::Cell::new(0u16),
                    )
                })
                .unwrap();
            let buf = terminal.backend().buffer();
            let area = buf.area;
            // the live band owns BG_TOOL now: header + its preview rows
            // (the cached railed rows sit on the canvas, no band fill)
            let band_rows = (0..area.height)
                .filter(|&y| (0..area.width).any(|x| buf[(x, y)].bg == crate::palette::BG_TOOL))
                .count();
            assert_eq!(
                band_rows,
                1 + live
                    .live_tool
                    .as_ref()
                    .map(|lt| lt.preview.len())
                    .unwrap_or(0),
                "band rows at width {width}"
            );
            // nothing band- or output-colored bleeds past the input box's
            // top edge (the strip and status rows stay chrome-clean)
            for y in (area.height - 2)..area.height {
                for x in 0..area.width {
                    let cell = &buf[(x, y)];
                    assert_ne!(
                        cell.bg,
                        crate::palette::BG_TOOL,
                        "tool bleed at {x},{y} w{width}"
                    );
                    assert_ne!(
                        cell.bg,
                        crate::palette::BG_OUTPUT,
                        "output bleed at {x},{y} w{width}"
                    );
                }
            }
        }
    }

    #[test]
    fn prompt_row_parser_splits_coords_and_args() {
        let (spec, args) = parse_prompt_row("review/repo (pr, dry)");
        assert_eq!(spec, Some(("review".into(), "repo".into())));
        assert_eq!(args, vec!["pr".to_string(), "dry".to_string()]);
        let (spec, args) = parse_prompt_row("review/repo");
        assert_eq!(spec, Some(("review".into(), "repo".into())));
        assert!(args.is_empty());
    }

    #[test]
    fn prompt_invocation_parses_kv_pairs() {
        let (server, name, args) = parse_prompt_invocation("review/repo pr=42 dry=true").unwrap();
        assert_eq!(server, "review");
        assert_eq!(name, "repo");
        assert_eq!(args.get("pr").map(String::as_str), Some("42"));
        assert_eq!(args.get("dry").map(String::as_str), Some("true"));
        assert!(parse_prompt_invocation("noseparator").is_none());
    }

    #[test]
    fn slash_mcp_refresh_and_prompt_dispatch() {
        let slash = slash_command("/mcp refresh").unwrap();
        assert!(matches!(slash.event, Some(Command::RefreshMcp)));
        // bare /mcp is a usage note, not an event
        let slash = slash_command("/mcp").unwrap();
        assert!(slash.event.is_none());
        assert!(slash.note.as_deref().is_some_and(|n| n.contains("usage")));

        let slash = slash_command("/prompt review/repo pr=42").unwrap();
        match slash.event {
            Some(Command::CallPrompt { server, name, args }) => {
                assert_eq!((server.as_str(), name.as_str()), ("review", "repo"));
                assert_eq!(args.get("pr").map(String::as_str), Some("42"));
            }
            other => panic!("expected CallPrompt, got {other:?}"),
        }
        // bare /prompt opens the picker modal
        let slash = slash_command("/prompt").unwrap();
        assert!(matches!(slash.modal, Some(ModalKind::Prompts)));
    }

    #[test]
    fn replay_digest_divider_renders_faint_row() {
        let mut lines = Transcript::default();
        feed(
            &mut lines,
            &Event::Replay {
                messages: vec![
                    ka_protocol::ReplayedMessage {
                        role: "digest".into(),
                        content: String::new(),
                        digest: true,
                        thinking: None,
                        calls: Vec::new(),
                    },
                    ka_protocol::ReplayedMessage {
                        role: "user".into(),
                        content: "after the digest".into(),
                        digest: false,
                        thinking: None,
                        calls: Vec::new(),
                    },
                ],
            },
        );
        let text = format!("{:?}", lines.entries());
        assert!(text.contains("digest"), "{text:?}");
        assert!(text.contains("after the digest"), "{text:?}");
    }

    #[test]
    fn replay_renders_thought_tool_rows_and_assistant_text() {
        let mut lines = Transcript::default();
        feed(
            &mut lines,
            &Event::Replay {
                messages: vec![
                    ka_protocol::ReplayedMessage {
                        role: "user".into(),
                        content: "list files".into(),
                        digest: false,
                        thinking: None,
                        calls: Vec::new(),
                    },
                    ka_protocol::ReplayedMessage {
                        role: "assistant".into(),
                        content: "found it".into(),
                        digest: false,
                        thinking: Some("scanning the tree".into()),
                        calls: vec![ka_protocol::ReplayedCall {
                            id: "c1".into(),
                            tool: "read".into(),
                            detail: "lib.rs".into(),
                            result: Some("fn main() {}".into()),
                            is_error: false,
                        }],
                    },
                ],
            },
        );
        let text = format!("{:?}", lines.entries());
        assert!(text.contains("scanning the tree"), "thought row: {text:?}");
        assert!(
            text.contains("ToolBlock([ToolCall { head: \"→ read · lib.rs\""),
            "tool block: {text:?}"
        );
        assert!(
            text.contains(r#"note: "fn main() {}""#) && text.contains("ok: true"),
            "merged result note + verdict: {text:?}"
        );
        assert!(text.contains("found it"), "assistant text: {text:?}");
    }

    #[test]
    fn seed_history_dedups_consecutive_and_caps() {
        let mut b = InputBuffer::default();
        b.seed_history(["a".into(), "a".into(), "b".into(), "a".into()]);
        assert_eq!(b.history.len(), 3, "consecutive duplicates collapse");
        b.history_prev();
        assert_eq!(b.text, "a", "the newest seeded item recalls first");

        // cap: 150 distinct prompts → the newest 100 survive
        let mut c = InputBuffer::default();
        c.seed_history((0..150).map(|i| format!("p{i}")));
        assert_eq!(c.history.len(), 100);
        c.history_prev();
        assert_eq!(c.text, "p149", "newest survives the cap");
    }

    #[test]
    fn tree_rows_walk_the_family_parent_before_child() {
        let all = vec![
            ka_strand::StrandSummary {
                path: std::path::PathBuf::new(),
                id: "root".into(),
                ts: "2026-01-02T10:00:00Z".into(),
                title: "the root".into(),
                messages: 3,
                cost: 0.0,
                parent: None,
                tokens: 0,
            },
            ka_strand::StrandSummary {
                path: std::path::PathBuf::new(),
                id: "unrelated".into(),
                ts: "2026-01-03T10:00:00Z".into(),
                title: "not family".into(),
                messages: 9,
                cost: 0.0,
                parent: None,
                tokens: 0,
            },
            ka_strand::StrandSummary {
                path: std::path::PathBuf::new(),
                id: "kid".into(),
                ts: "2026-01-04T10:00:00Z".into(),
                title: "a child".into(),
                messages: 1,
                cost: 0.0,
                parent: Some("root".into()),
                tokens: 0,
            },
            ka_strand::StrandSummary {
                path: std::path::PathBuf::new(),
                id: "grandkid".into(),
                ts: "2026-01-05T10:00:00Z".into(),
                title: "a grandchild".into(),
                messages: 2,
                cost: 0.0,
                parent: Some("kid".into()),
                tokens: 0,
            },
        ];
        let (rows, targets) = tree_modal_rows(&all, Some("root"), 60);
        assert_eq!(
            targets,
            vec![
                "root".to_string(),
                "kid".to_string(),
                "grandkid".to_string()
            ]
        );
        assert_eq!(rows.len(), 3);
        assert!(
            rows[0].starts_with("\u{25b8} "),
            "current is marked: {:?}",
            rows[0]
        );
        assert!(rows[1].starts_with("  "), "child indented: {:?}", rows[1]);
        assert!(
            rows[2].starts_with("    "),
            "grandchild double-indented: {:?}",
            rows[2]
        );
        assert!(!rows.iter().any(|r| r.contains("not family")));
        // the walk is descendants-only: from the grandkid the closure is
        // just itself (fork history stays out, matching the old build)
        let (rows2, targets2) = tree_modal_rows(&all, Some("grandkid"), 60);
        assert_eq!(targets2, vec!["grandkid".to_string()]);
        assert!(rows2[0].starts_with("\u{25b8} "), "{:?}", rows2[0]);
        // a strand with no family renders alone, unindented
        let (rows3, targets3) = tree_modal_rows(&all, Some("unrelated"), 60);
        assert_eq!(targets3, vec!["unrelated".to_string()]);
        assert!(!rows3[0].starts_with(' '));
    }

    #[test]
    fn slash_tree_opens_modal() {
        let slash = slash_command("/tree").unwrap();
        assert!(matches!(slash.modal, Some(ModalKind::Tree)));
        assert!(slash.event.is_none());
    }

    #[test]
    fn image_part_from_path_sniffs_and_encodes() {
        let dir = std::env::temp_dir().join(format!("ka-img-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let png: &[u8] = &[
            0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52,
        ];
        let path = dir.join("p.png");
        std::fs::write(&path, png).unwrap();
        let part = image_part_from_path(&path).unwrap();
        assert_eq!(part.media_type, "image/png");
        assert_eq!(part.data, b64encode(png));
        // unsupported file rejected
        let txt = dir.join("t.txt");
        std::fs::write(&txt, "plain").unwrap();
        assert!(image_part_from_path(&txt).is_err());
        // missing file rejected
        assert!(image_part_from_path(&dir.join("nope.png")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn handle_image_command_stages_or_reports() {
        let dir = std::env::temp_dir().join(format!("ka-imgc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let png: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        std::fs::write(dir.join("p.png"), png).unwrap();
        let mut pending: Option<ka_protocol::ImagePart> = None;
        let out = handle_image_command(
            &format!("/image {}", dir.join("p.png").display()),
            &mut pending,
        );
        assert!(out.is_some());
        assert!(out.unwrap().is_ok());
        assert!(pending.is_some(), "image staged");
        assert_eq!(pending.unwrap().media_type, "image/png");

        // non-image paths error without staging
        let mut pending: Option<ka_protocol::ImagePart> = None;
        let out = handle_image_command("/image /definitely/absent.png", &mut pending);
        assert!(matches!(out, Some(Err(_))));
        assert!(pending.is_none());

        // bare /image (no path) is not this command
        let mut pending: Option<ka_protocol::ImagePart> = None;
        assert!(handle_image_command("/image", &mut pending).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sniff_image_bytes_reads_magic_prefixes() {
        assert_eq!(
            sniff_image_bytes(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0]),
            Some("image/png")
        );
        // short prefix still sniffs: 3 signature bytes are enough
        assert_eq!(sniff_image_bytes(&[0x89, b'P', b'N']), None);
        assert_eq!(
            sniff_image_bytes(&[0xFF, 0xD8, 0xFF, 0xE0]),
            Some("image/jpeg")
        );
        assert_eq!(sniff_image_bytes(b"GIF89a...."), Some("image/gif"));
        assert_eq!(sniff_image_bytes(b"GIF87a"), Some("image/gif"));
        assert_eq!(
            sniff_image_bytes(b"RIFF\x00\x00\x00\x00WEBP"),
            Some("image/webp")
        );
        assert_eq!(sniff_image_bytes(b"RIFFshort"), None);
        assert_eq!(sniff_image_bytes(b"plain text!!!"), None);
        assert_eq!(sniff_image_bytes(&[]), None);
    }

    #[test]
    fn win_to_wsl_maps_drive_paths() {
        assert_eq!(
            win_to_wsl("C:\\Users\\javier\\AppData\\Local\\Temp\\ka-clip.png"),
            Some(std::path::PathBuf::from(
                "/mnt/c/Users/javier/AppData/Local/Temp/ka-clip.png"
            ))
        );
        assert_eq!(
            win_to_wsl("c:\\x\\y"),
            Some(std::path::PathBuf::from("/mnt/c/x/y"))
        );
        // quotes and padding from console output are tolerated
        assert_eq!(
            win_to_wsl("\"D:\\a b\\c.png\"\r\n"),
            Some(std::path::PathBuf::from("/mnt/d/a b/c.png"))
        );
        // not a drive path
        assert_eq!(win_to_wsl("relative/path.png"), None);
        assert_eq!(win_to_wsl(""), None);
    }

    #[test]
    fn image_part_from_bytes_caps_and_sniffs() {
        // over the cap → rejected before any encoding
        let big = vec![0u8; 5 * 1024 * 1024 + 1];
        let err = image_part_from_bytes(&big).unwrap_err();
        assert!(err.contains("cap"), "{err}");
        // unknown bytes → unsupported type
        let err = image_part_from_bytes(b"not an image").unwrap_err();
        assert!(err.contains("unsupported"), "{err}");
        // a real png prefix round-trips
        let png: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3];
        let part = image_part_from_bytes(png).unwrap();
        assert_eq!(part.media_type, "image/png");
        assert_eq!(part.data, b64encode(png));
    }
    #[test]
    fn thought_blocks_toggle_individually_and_map_rows() {
        let mut t = Transcript::default();
        t.set_width(60);
        t.push(Line::User("q".into()));
        t.push(Line::Thought("a\nb\nc".into()));
        t.push(Line::Thought("solo".into()));
        t.push(Line::Assistant("ans".into()));
        // collapsed by default: entry 1 renders 1 row, solo entry 2 = 1 row
        let rows = t.total_rows();
        // user(1) + thought(1) + thought(1) + assistant block(>=1)
        assert!(rows >= 4, "{rows}");

        // toggle block 1 open: its rendered height grows
        let before = t.rendered_rows_of(1);
        t.toggle_thought(1);
        let after = t.rendered_rows_of(1);
        assert!(after > before, "open block grows: {before} -> {after}");

        // single-line thoughts refuse to toggle
        let solo_before = t.rendered_rows_of(2);
        t.toggle_thought(2);
        assert_eq!(t.rendered_rows_of(2), solo_before);

        // row -> ref mapping: the first row of each entry maps back
        // to it (user blocks render with padding, so use real heights)
        let e = t.row_ref_at(0).unwrap();
        assert_eq!(e, RowRef::Entry(0), "row 0 is the user block");
        let first_of_1 = t.rendered_rows_of(0);
        let e1 = t.row_ref_at(first_of_1).unwrap();
        assert_eq!(
            e1,
            RowRef::Entry(1),
            "row {first_of_1} is the thought block"
        );

        // global Alt+T reset clears per-block overrides
        t.set_thoughts_open(true);
        let open_rows = t.rendered_rows_of(1);
        assert!(open_rows == after, "global open respects no overrides");
    }
}
