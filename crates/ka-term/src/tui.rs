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
    /// Turn report (muted info row).
    Report(String),
    /// Turn report (error row).
    ReportErr(String),
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

/// The searchable text of a transcript entry.
fn line_text(line: &Line) -> &str {
    match line {
        Line::User(t)
        | Line::Assistant(t)
        | Line::Thought(t)
        | Line::Tool(t)
        | Line::Note(t)
        | Line::Report(t)
        | Line::ReportErr(t) => t,
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
        Line::Tool(_) => Family::Tool,
        Line::Note(_) | Line::Report(_) | Line::ReportErr(_) => Family::Meta,
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
/// Render one transcript entry into styled rows at `width`.
fn render_line(line: &Line, width: u16) -> Vec<ratatui::text::Line<'static>> {
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
            let mut rows = crate::markdown::render(text, width);
            apply_output_surface(&mut rows, width, bg);
            out.extend(rows);
            out.push(surface_blank(width, bg));
        }
        Line::Thought(text) => push_gutter(&mut out, text, width, "⋯ ", crate::palette::THOUGHT),
        // finished tool calls cache as ONE full-width band row, so tool
        // activity reads as its own stratum (the live block's band)
        Line::Tool(text) => out.push(TuiLine::from(vec![ratatui::text::Span::styled(
            pad_to_width(format!(" {text}"), width as usize),
            crate::palette::TOOL_BAND_STYLE,
        )])),
        Line::Note(text) => push_gutter(
            &mut out,
            text,
            width,
            "! ",
            ratatui::style::Style::new().fg(crate::palette::ERR),
        ),
        // the vertical separator is one true canvas-blank row (gutter
        // text is empty, so the prefix loop would emit nothing)
        Line::Report(text) if text.is_empty() => out.push(TuiLine::default()),
        Line::Report(text) => push_gutter(&mut out, text, width, "─ ", crate::palette::META.into()),
        Line::ReportErr(text) => push_gutter(
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
    term_h.saturating_sub(input_h + 3) as usize
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

/// Rows the input area needs: while a permission ask is up the form
/// (question lines + one options row) borrows the box from the draft,
/// and the /mode picker borrows it for its four tier rows.
fn input_area_rows(ask: Option<&PendingAsk>, picker: Option<&ModePicker>, draft: &str) -> usize {
    match ask {
        Some(a) => {
            a.question.split('\n').count()
                + 1
                + a.detail
                    .as_ref()
                    .map_or(0, |d| ask_detail_rows(d, ASK_DETAIL_MAX).len())
        }
        None if picker.is_some() => MODE_CHOICES.len(),
        None => draft.split('\n').count(),
    }
}

/// Max rows of diff detail shown in an ask form (the form borrows the
/// input box; uncapped diffs would crowd out the choice).
const ASK_DETAIL_MAX: usize = 8;

/// Colorized rows for an ask's diff detail: additions in OK, removals
/// in ERR, hunk headers in META, file headers in FAINT, context plain.
/// Clamped to `max` rows with a `… N more` trailer.
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
            format!("… {more} more"),
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

/// Everything the right sidebar displays. Session figures come from the
/// footer meters at render time; this state carries the rest.
pub struct SidebarState {
    /// Bootstrap inventory.
    pub inventory: Inventory,
    /// Live todo list ([`Event::Todos`]; whole-list replacement).
    pub todos: Vec<ka_protocol::TodoItem>,
    /// Working directory, display-shortened.
    pub cwd: String,
    /// Git branch when cheaply detectable at startup.
    pub branch: Option<String>,
    /// Skills section expanded? Collapsed via a mouse click on its
    /// header; the header keeps rendering as `skills (+N)`.
    pub skills_open: bool,
    /// Cursor is over the skills header (drives the hover affordance).
    pub skills_hover: bool,
    /// Window/sidebar title glyph ([tui] header_glyph, default ◆).
    pub header_glyph: String,
    /// The active session's display title ([`Event::Title`]; stored
    /// record or auto-generated). None until the engine announces one.
    pub title: Option<String>,
}

impl Default for SidebarState {
    fn default() -> Self {
        Self {
            inventory: Default::default(),
            todos: Vec::new(),
            cwd: String::new(),
            branch: None,
            skills_open: true,
            skills_hover: false,
            title: None,
            header_glyph: "◆".to_string(),
        }
    }
}

/// A clickable sidebar region, recorded at render time so the mouse
/// handler can hit-test without duplicating the layout math. New
/// collapsible sections add a variant here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SidebarZone {
    /// The skills section header row (spans the sidebar's full width).
    SkillsHeader(ratatui::layout::Rect),
}

impl SidebarZone {
    /// Does a terminal-cell click land inside this zone?
    fn hit(&self, x: u16, y: u16) -> bool {
        match self {
            SidebarZone::SkillsHeader(rect) => rect.contains(ratatui::layout::Position { x, y }),
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

/// Sidebar skills header text: bare when expanded, count-marked when
/// collapsed (the ` ▾`/` ▸` affordance rides separately as a dim span).
fn skills_header_label(open: bool, count: usize) -> String {
    if open {
        "skills".to_string()
    } else {
        format!("skills (+{count})")
    }
}

/// Full skills header line: label plus the ` ▾`/` ▸` disclosure arrow.
/// Hover lifts the whole header onto the modal surface tint (ACCENT on
/// BG_SURFACE); at rest the label keeps its accent-bold look and the
/// arrow stays dim META.
fn skills_header_line(open: bool, hover: bool, count: usize) -> ratatui::text::Line<'static> {
    use ratatui::text::{Line as TuiLine, Span};
    let label = skills_header_label(open, count);
    let arrow = if open { " ▾" } else { " ▸" };
    if hover {
        let st = ratatui::style::Style::new()
            .fg(crate::palette::ACCENT)
            .bg(crate::palette::BG_SURFACE);
        TuiLine::from(vec![Span::styled(label, st), Span::styled(arrow, st)])
    } else {
        TuiLine::from(vec![
            Span::styled(label, crate::palette::ACCENT_BOLD),
            Span::styled(arrow, crate::palette::META),
        ])
    }
}

/// Sidebar column width when shown.
const SIDEBAR_WIDTH: u16 = 26;
/// Minimum terminal width for the sidebar; below it the transcript keeps
/// the full row exactly as before.
const SIDEBAR_MIN_WIDTH: u16 = 100;
/// Inner text width of the transcript column: the terminal narrows by
/// one margin column on each side, by the sidebar's columns once it
/// fits, and by the paragraph's side padding. Both the cache
/// (`Transcript::set_width`) and the live surface derive from this so
/// they always agree.
fn transcript_width(term_w: u16) -> u16 {
    let base = if term_w >= SIDEBAR_MIN_WIDTH {
        term_w - SIDEBAR_WIDTH
    } else {
        term_w
    };
    base.saturating_sub(4) // 2 margin cols + the paragraph's side padding
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
/// `hit`: when given, the cell receives the skills header's on-screen
/// rectangle (or `None` when no skills header is on screen), so the
/// mouse handler can hit-test with render-time truth.
fn sidebar_rows(
    sidebar: &SidebarState,
    meters: &Meters,
    width: usize,
    height: usize,
    hit: Option<(&std::cell::Cell<Option<SidebarZone>>, ratatui::layout::Rect)>,
) -> Vec<ratatui::text::Line<'static>> {
    use ratatui::style::{Modifier, Style};
    use ratatui::text::{Line as TuiLine, Span};

    let header = |name: &str| TuiLine::styled(name.to_string(), crate::palette::ACCENT_BOLD);
    let plain = |s: String| TuiLine::from(s);
    // ── session: mirrors the sidebar meters, one fact per row ──
    let mut session = vec![header("session")];
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
        sidebar
            .title
            .as_deref()
            .filter(|t| !t.is_empty())
            .map(str::to_string),
        (!meters.model.is_empty()).then(|| format!("model {}", meters.model)),
        (!meters.mode.is_empty()).then(|| format!("mode {}", meters.mode)),
        (!meters.effort.is_empty()).then(|| format!("effort {}", meters.effort)),
        short_session(&meters.session).map(|t| format!("#{t}")),
        (meters.cost > 0.0).then(|| format!("${:.4}", meters.cost)),
        (used > 0).then_some(ctx_row),
    ]
    .into_iter()
    .flatten()
    {
        session.push(plain(trunc_cols(&row, width)));
    }

    let done_style = Style::new()
        .fg(crate::palette::FAINT)
        .add_modifier(Modifier::CROSSED_OUT);
    let first_pending = sidebar
        .todos
        .iter()
        .position(|t| t.state == ka_protocol::TodoState::Pending);
    let todos = std::iter::once(header("todos"))
        .chain(
            sidebar
                .todos
                .iter()
                .enumerate()
                .map(|(i, t)| match t.state {
                    ka_protocol::TodoState::Done => TuiLine::from(vec![
                        Span::styled("✓ ", done_style),
                        Span::styled(trunc_cols(&t.text, width.saturating_sub(2)), done_style),
                    ]),
                    // the first pending item is 'next': accent bold; the rest
                    // stay plain, both under a uniform `· ` lead
                    ka_protocol::TodoState::Pending if Some(i) == first_pending => {
                        TuiLine::from(vec![
                            Span::styled("· ", crate::palette::ACCENT_BOLD),
                            Span::styled(
                                trunc_cols(&t.text, width.saturating_sub(2)),
                                crate::palette::ACCENT_BOLD,
                            ),
                        ])
                    }
                    ka_protocol::TodoState::Pending => TuiLine::from(format!(
                        "· {}",
                        trunc_cols(&t.text, width.saturating_sub(2))
                    )),
                }),
        )
        .collect::<Vec<_>>();

    // ── mcp: `name ✓ n` / `name ✗` ──
    let mut mcp = vec![header("mcp")];
    for m in &sidebar.inventory.mcp {
        let row = if m.ok {
            TuiLine::from(vec![
                Span::raw(format!("{} ", trunc_cols(&m.name, width.saturating_sub(4)))),
                Span::styled("✓", Style::new().fg(crate::palette::OK)),
                Span::raw(format!(" {}", m.tools)),
            ])
        } else {
            TuiLine::from(vec![
                Span::raw(format!("{} ", trunc_cols(&m.name, width.saturating_sub(4)))),
                Span::styled("✗", Style::new().fg(crate::palette::ERR)),
            ])
        };
        mcp.push(row);
    }
    let names = |label: &str, items: &[String]| {
        std::iter::once(header(label))
            .chain(items.iter().map(|s| plain(trunc_cols(s, width))))
            .collect::<Vec<_>>()
    };
    // collapsed skills render their header alone, with the count moved
    // into the label; the header carries the disclosure arrow and the
    // hover affordance
    let skills = if sidebar.skills_open {
        let mut rows = names("skills", &sidebar.inventory.skills);
        if !rows.is_empty() {
            rows[0] =
                skills_header_line(true, sidebar.skills_hover, sidebar.inventory.skills.len());
        }
        rows
    } else {
        vec![skills_header_line(
            false,
            sidebar.skills_hover,
            sidebar.inventory.skills.len(),
        )]
    };
    let agents = names("agents", &sidebar.inventory.agents);

    // ── info: one `cwd-short:branch` row at the bottom; without a
    // startup branch snapshot the whole section stays off ──
    let mut info = vec![header("info")];
    if let Some(b) = &sidebar.branch {
        info.push(plain(trunc_cols(&format!("{}:{b}", sidebar.cwd), width)));
    }

    // flatten top-to-bottom, dropping sections that no longer fit and
    // capping any list section that would overflow the remaining height.
    // Air: one blank row after each header and one between sections;
    // priorities keep their top-down order, the rest truncates.
    const SKILLS_SECTION: usize = 3;
    let sections = [session, todos, mcp, skills, agents, info];
    let mut out: Vec<TuiLine> = Vec::with_capacity(height.min(40));
    let mut room = height;
    let mut placed = false;
    let mut skills_header_row: Option<usize> = None;
    for (si, section) in sections.into_iter().enumerate() {
        let Some((head, body)) = section.split_first() else {
            continue;
        };
        // a collapsed skills section carries no body but its header
        // must still land on screen
        let forced = si == SKILLS_SECTION && !sidebar.skills_open && body.is_empty();
        if body.is_empty() && !forced {
            continue; // empty sections vanish
        }
        if placed {
            if room == 0 {
                break;
            }
            out.push(TuiLine::default()); // one blank row between sections
            room -= 1;
        }
        let show = (room - 2).min(body.len());
        if show == 0 && !forced {
            break; // not even one body row fits under the header air
        }
        placed = true;
        if si == SKILLS_SECTION {
            skills_header_row = Some(out.len());
        }
        out.push(head.clone());
        out.push(TuiLine::default()); // blank under the header
        if show < body.len() {
            // the cut must be visible: the mark replaces the last row
            out.extend(body[..show - 1].iter().cloned());
            out.push(TuiLine::styled(
                format!("(+{})", body.len() - show + 1),
                crate::palette::META,
            ));
        } else {
            out.extend(body.iter().cloned());
        }
        room -= 2 + show;
    }
    if let Some((cell, area)) = hit {
        cell.set(skills_header_row.map(|row| {
            SidebarZone::SkillsHeader(ratatui::layout::Rect {
                x: area.x,
                // the block title consumes the area's first row, so a
                // rendered row lives one below area.y
                y: area.y + 1 + row as u16,
                width: area.width,
                height: 1,
            })
        }));
    }
    out
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
/// ka-agent-free).
pub static AGENTS: std::sync::OnceLock<Vec<(String, String)>> = std::sync::OnceLock::new();

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

/// The shared follow-up prompt that starts a build turn from the plan
/// file (both `/build` and `/approve`).
fn build_followup() -> String {
    "Switching to build mode. Read .ka/plans/plan.md and implement it step by \
step now; verify each step."
        .to_string()
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
    /// present). Both the flat `/model` list and the post-connect drill
    /// set this.
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
    pub fn rows(&self) -> Vec<&ModelInfo> {
        let f = self.filter.to_lowercase();
        self.models
            .iter()
            .filter(|m| {
                self.vendor
                    .as_deref()
                    .is_none_or(|v| m.id.split('/').next() == Some(v))
            })
            .filter(|m| !self.configured_only || m.key_env.is_empty() || m.key_set)
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
    /// Memory viewer (/memory): project + user memory files.
    Memory {
        /// Rendered rows (path header + content lines).
        rows: Vec<String>,
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
    /// Vendor to drill into (its model list) once the key is saved.
    pub drill: Option<String>,
}

/// Run the TUI over an engine handle. Blocks until exit.
pub async fn run(
    mut commands: mpsc::Sender<Command>,
    mut events: mpsc::Receiver<Event>,
    initial_model: &str,
    providers: Vec<ProviderInfo>,
    models: Vec<ModelInfo>,
    agents: Vec<(String, String)>,
    header_glyph: &str,
) -> std::io::Result<Exit> {
    let _ = AGENTS.set(agents.clone());
    let mut terminal = ratatui::init();
    // Kitty keyboard protocol: Shift+Enter as a distinct key + bracketed
    // paste. Best effort — hosts without support degrade to plain Enter;
    // Ctrl+J always works as the newline fallback. Mouse capture starts
    // ON so the wheel scrolls the transcript out of the box; Ctrl+M
    // releases it for native selection (shift+drag selects under
    // capture too).
    let _ = crossterm::execute!(
        std::io::stdout(),
        crossterm::event::PushKeyboardEnhancementFlags(
            crossterm::event::KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | crossterm::event::KeyboardEnhancementFlags::REPORT_EVENT_TYPES
        ),
        crossterm::event::EnableBracketedPaste,
        crossterm::event::EnableMouseCapture,
    );
    // any-event (motion) tracking: crossterm has no wrapper for the raw
    // `1003` sequence; without it Moved events only flow while a button
    // is held. Popped at every capture-disable path below.
    let _ = std::io::stdout().write_all(b"\x1b[?1003h");
    let _ = std::io::stdout().flush();
    let result = app(
        &mut terminal,
        &mut commands,
        &mut events,
        initial_model,
        providers,
        models,
        agents,
        header_glyph,
    )
    .await;
    let _ = std::io::stdout().write_all(b"\x1b[?1003l");
    let _ = std::io::stdout().flush();
    let _ = crossterm::execute!(
        std::io::stdout(),
        crossterm::event::PopKeyboardEnhancementFlags,
        crossterm::event::DisableBracketedPaste,
        crossterm::event::DisableMouseCapture
    );
    ratatui::restore();
    result
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
) -> std::io::Result<Exit> {
    use crossterm::event::{Event as TermEvent, KeyCode, KeyModifiers};
    let _ = agents.clone();

    let mut scroll: Option<usize> = None;
    let mut transcript = Transcript::default();

    let mut mouse_capture = true;
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
    // pending image attachment: staged by `/image <path>`, consumed by
    // the next sent prompt
    let mut pending_image: Option<ka_protocol::ImagePart> = None;
    let mut sidebar = SidebarState {
        cwd: shorten_cwd(&std::env::current_dir().unwrap_or_default()),
        branch: detect_branch(),
        header_glyph: header_glyph.to_string(),
        ..Default::default()
    };
    let mut exit = None;
    let mut slash_popup: Option<SlashPopup> = None;
    let mut path_popup: Option<PathPopup> = None;
    // last /find query + the row to resume a bare /find after
    let mut find_last: Option<(String, usize)> = None;
    let mut modal: Option<Modal> = None;
    let mut mode_picker: Option<ModePicker> = None;
    // clickable sidebar regions, refreshed every frame by render()
    let sidebar_zone: std::cell::Cell<Option<SidebarZone>> = std::cell::Cell::new(None);
    // clickable ▲▼ user-message jump targets on the title row
    let title_arrows: std::cell::Cell<Option<TitleArrows>> = std::cell::Cell::new(None);
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
        let md_width = transcript_width(term_w);
        transcript.set_width(md_width);
        let input_h = input_height(input_area_rows(
            ask.as_ref(),
            mode_picker.as_ref(),
            &input.text,
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
                tool: tool_live_rows(current_tool.as_str(), live_tool.as_ref(), md_width as usize),
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
                &transcript,
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
                &sidebar_zone,
                &title_arrows,
            );
        })?;

        tokio::select! {
            biased;
            maybe_term = term_events.next() => {
                if let Some(Ok(TermEvent::Key(key))) = maybe_term {
                    if key.kind == crossterm::event::KeyEventKind::Release {
                        continue;
                    }
                    // Ctrl+C always quits or aborts, ahead of every other
                    // capture (asks, modals, popups all swallow chars)
                    if (key.code, key.modifiers) == (KeyCode::Char('c'), KeyModifiers::CONTROL) {
                        if busy {
                            let _ = commands.send(Command::Abort).await;
                        } else {
                            exit = Some(Exit::Quit);
                        }
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
                            // direct pick: 1..9 answers that option at once
                            KeyCode::Char(c) if c.is_ascii_digit() && c != '0' => {
                                let idx = (c as u8 - b'1') as usize;
                                if idx < ask.options.len() {
                                    let id = ask.id.clone();
                                    pending = None;
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
                                    }
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
                                                    drill: None,
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
                                            })
                                            .await;
                                    }
                                    _ => {}
                                }
                            }
                            Modal::Memory { .. } => {
                                if key.code == KeyCode::Esc {
                                    modal = None;
                                }
                            }
                            Modal::Usage { .. } => {
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
                                        modal = None;
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
                                            transcript.push_separated(Line::Note(format!(
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
                                                        mouse_capture,
                                                        terminal,
                                                    ),
                                                    None => run_external(
                                                        "less",
                                                        &["-R", path.as_str()],
                                                        mouse_capture,
                                                        terminal,
                                                    ),
                                                };
                                            if let Err(e) = opened {
                                                transcript.push_separated(Line::Note(format!("pager failed: {e}")));
                                            }
                                        }
                                    }
                                }
                                _ => {}
                            },
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
                        (KeyCode::Char('m'), KeyModifiers::CONTROL) => {
                            // capture on by default (wheel scrolling);
                            // toggling releases the mouse for native
                            // text selection. Motion tracking (1003)
                            // follows capture so hover never leaks.
                            mouse_capture = !mouse_capture;
                            let _ = if mouse_capture {
                                std::io::stdout()
                                    .write_all(b"\x1b[?1003h")
                                    .and_then(|_| std::io::stdout().flush())
                                    .and_then(|_| {
                                        crossterm::execute!(
                                            std::io::stdout(),
                                            crossterm::event::EnableMouseCapture
                                        )
                                    })
                            } else {
                                std::io::stdout()
                                    .write_all(b"\x1b[?1003l")
                                    .and_then(|_| std::io::stdout().flush())
                                    .and_then(|_| {
                                        crossterm::execute!(
                                            std::io::stdout(),
                                            crossterm::event::DisableMouseCapture
                                        )
                                    })
                            };
                        }
                        // Reverse history search: while active it captures
                        // editing keys, so its arms sit ahead of the
                        // standard Esc/Enter/Backspace/char handling.
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
                        (KeyCode::Char(c), _) if input.searching() => input.search_push(c),
                        (KeyCode::Esc, _) if busy => {
                            let _ = commands.send(Command::Abort).await;
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
                                            transcript.push_separated(Line::Note("no previous /find".into()));
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
                                        transcript.push_separated(Line::Note(format!("no matches for '{q}'")));
                                        // resume past the searched-from row so a
                                        // later /find re-scans only fresh rows
                                        find_last = Some((q, from));
                                    }
                                }
                                continue;
                            }
                            if text.trim() == "/retry" {
                                if busy {
                                    transcript.push_separated(Line::Note(
                                        "⏳ turn running — esc to abort first".into(),
                                    ));
                                } else if let Some(p) = last_user.clone() {
                                    // retry = a fresh turn with the same prompt
                                    transcript.push_separated(Line::User(p.clone()));
                                    busy = true;
                                    let _ = commands.send(Command::Prompt { text: p, schema: None, images: Vec::new() }).await;
                                } else {
                                    transcript.push_separated(Line::Note("nothing to retry yet".into()));
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
                                    None => transcript.push_separated(Line::Note(
                                        "no assistant reply to copy".into(),
                                    )),
                                    Some(s) => {
                                        // silent success: OSC52 into the clipboard
                                        let mut stdout = std::io::stdout().lock();
                                        let _ = stdout.write_all(b"\x1b]52;c;");
                                        let _ = stdout.write_all(b64encode(&s).as_bytes());
                                        let _ = stdout.write_all(b"\x07");
                                        let _ = stdout.flush();
                                    }
                                }
                                continue;
                            }
                            // /image: stage an attachment instead of a prompt
                            if text.starts_with("/image ") || text == "/image" {
                                match handle_image_command(&text, &mut pending_image) {
                                    Some(Ok(note)) => {
                                        transcript.push_separated(Line::Note(note));
                                    }
                                    Some(Err(e)) => transcript.push_separated(Line::Note(e)),
                                    None => {}
                                }
                                input.text.clear();
                                input.cursor = 0;
                                continue;
                            }
                            // /clip: paste a clipboard image as the
                            // next prompt's attachment
                            if text == "/clip" {
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
                                    Ok(note) => transcript.push_separated(Line::Note(note)),
                                    Err(e) => transcript.push_separated(Line::Note(e)),
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
                                    transcript.push_separated(Line::Note(note));
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
                    })
                });
                match prompt {
                    Some(p) => modal = Some(Modal::Key(p)),
                    None => transcript.push_separated(Line::Note("no api key variable is known for this model".into())),
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
                                            configured_only: true,
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
                                            let files = discover_memory_files(&cwd);
                                            let mut rows = Vec::new();
                                            if files.is_empty() {
                                                rows.push("(no memory files)".into());
                                                rows.push(
                                                    "create ./MEMORY.md or ~/.config/ka/MEMORY.md".into(),
                                                );
                                            }
                                            for (path, content) in files {
                                                rows.push(format!("▸ {}", path.display()));
                                                for line in content.lines() {
                                                    rows.push(line.to_string());
                                                }
                                            }
                                            Modal::Memory { rows }
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
                                            let mut ids: Vec<String> =
                                                current.clone().map(|c| vec![c]).unwrap_or_default();
                                            let mut i = 0;
                                            while i < ids.len() {
                                                let frontier = ids.clone();
                                                for s in &all {
                                                    if s.parent
                                                        .as_deref()
                                                        .is_some_and(|p| frontier.iter().any(|id| id == p))
                                                        && !ids.contains(&s.id)
                                                    {
                                                        ids.push(s.id.clone());
                                                    }
                                                }
                                                i += 1;
                                                if i > all.len() + 1 {
                                                    break;
                                                }
                                            }
                                            let mut items = Vec::new();
                                            let mut targets = Vec::new();
                                            for s in &all {
                                                if !ids.contains(&s.id) {
                                                    continue;
                                                }
                                                let date =
                                                    s.ts.get(..10).unwrap_or(&s.ts).to_string();
                                                let marker = if Some(&s.id) == current.as_ref() {
                                                    "▸ "
                                                } else {
                                                    ""
                                                };
                                                items.push(format!(
                                                    "{marker}{} · {date} · {} msgs",
                                                    s.title, s.messages
                                                ));
                                                targets.push(s.id.clone());
                                            }
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
                                    });
                            }
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
                                    transcript.push_separated(Line::Note("(mode set; starting)".into()));
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
                                        transcript.push_separated(Line::Note(format!(
                                            "⏳ queued · {} waiting for this turn to end",
                                            queue.len()
                                        )));
                                    }
                                    continue;
                                }
                            }
                            transcript.push_separated(Line::User(text.clone()));
                            let cmd = if busy {
                                transcript.push_separated(Line::Note(
                                    "⚡ steering this turn".into(),
                                ));
                                Command::Interject { text }
                            } else {
                                // plain new turn: the /retry target; a
                                // staged /image attachment rides along
                                last_user = Some(text.clone());
                                if let Some(img) = &pending_image {
                                    let kb = img.data.len() * 3 / 4 / 1024;
                                    transcript.push_separated(Line::Note(format!(
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
                        (KeyCode::Up, _) if !busy => input.history_prev(),
                        (KeyCode::Down, _)
                            if !busy && slash_popup.is_none() && input.text.contains('\n') =>
                        {
                            input.move_down();
                        }
                        (KeyCode::Down, _) if !busy => input.history_next(),
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
                                    transcript.push_separated(Line::Note(format!("edit failed: {e}")));
                                }
                                Ok(()) => {
                                    let editor = std::env::var("EDITOR")
                                        .ok()
                                        .filter(|e| !e.is_empty())
                                        .unwrap_or_else(|| "vi".to_string());
                                    match run_external(
                                        &editor,
                                        &[path_str.as_str()],
                                        mouse_capture,
                                        terminal,
                                    ) {
                                        Err(e) => transcript.push_separated(Line::Note(format!("editor failed: {e}"))),
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
                                                Err(e) => transcript.push_separated(Line::Note(format!(
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
                            | Modal::Context { .. } => {}
                        }
                    } else if path_popup.is_some() {
                        path_popup = None;
                        input.insert_str(&text);
                        slash_popup = update_suggestions(&input.text);
                    } else if !input.searching()
                        && text.trim().chars().filter(|c| !c.is_whitespace()).count() == 1
                        && text.trim().chars().all(|c| !c.is_whitespace())
                    {
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
                                        transcript.push_separated(Line::Note(format!(
                                            "[img {name} · {kb}KB] — press enter to send"
                                        )));
                                        slash_popup = None;
                                        continue;
                                    }
                                    Err(e) => {
                                        transcript.push_separated(Line::Note(e));
                                        slash_popup = None;
                                        continue;
                                    }
                                }
                            }
                        }
                        slash_popup = update_suggestions(&input.text);
                    } else if input.searching() {
                        for c in text.chars().filter(|c| !c.is_whitespace()) {
                            input.search_push(c);
                        }
                    } else {
                        slash_popup = update_suggestions(&input.text);
                    }
                } else if let Some(Ok(TermEvent::Mouse(mouse))) = maybe_term {
                    // the wheel scrolls the chat; overlays keep focus. Line
                    // granularity (page keys keep their page step). With
                    // capture off (Ctrl+M) the terminal owns the mouse
                    // (native selection).
                    if mouse_capture
                        && modal.is_none()
                        && pending.is_none()
                        && slash_popup.is_none()
                        && path_popup.is_none()
                    {
                        match mouse.kind {
                            // a click on the skills header toggles the
                            // section; checked before the wheel arms so a
                            // click never also scrolls
                            crossterm::event::MouseEventKind::Down(
                                crossterm::event::MouseButton::Left,
                            ) => {
                                // ▲▼ jump arrows on the transcript title
                                // row: step between user messages
                                if let Some(up) = title_arrows
                                    .get()
                                    .and_then(|z| z.hit(mouse.column, mouse.row))
                                {
                                    let rows = transcript.user_entry_rows();
                                    let total = transcript.total_rows();
                                    jump_to_user_message(
                                        &mut scroll,
                                        &rows,
                                        total,
                                        view_rows,
                                        up,
                                    );
                                } else if sidebar_zone
                                    .get()
                                    .is_some_and(|z| z.hit(mouse.column, mouse.row))
                                {
                                    sidebar.skills_open = !sidebar.skills_open;
                                }
                            }
                            // hover affordance: only re-render when the
                            // pointer actually crosses the header boundary
                            crossterm::event::MouseEventKind::Moved
                            | crossterm::event::MouseEventKind::Drag(
                                crossterm::event::MouseButton::Left,
                            ) => {
                                let hit = sidebar_zone
                                    .get()
                                    .is_some_and(|z| z.hit(mouse.column, mouse.row));
                                if sidebar.skills_hover != hit {
                                    sidebar.skills_hover = hit;
                                }
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
                        apply_event(
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
                        );
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
                            && plan_drafted(
                                plan_started,
                                std::path::Path::new(".ka/plans/plan.md"),
                            )
                        {
                            plan_started = None;
                            transcript.push_separated(Line::Note(
                                "Plan drafted — review .ka/plans/plan.md, then /approve to build"
                                    .into(),
                            ));
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
            _ = spin.tick(), if busy => {}
        }
    }
    Ok(exit.unwrap_or(Exit::Quit))
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

/// One dim closing row for a turn that produced output:
/// `{model} · {elapsed}s · ${cost}`, capped at 60 columns.
fn turn_meta_row(model: &str, elapsed: f64, cost: f64) -> String {
    let mut segs: Vec<String> = Vec::new();
    if !model.is_empty() {
        segs.push(model.to_string());
    }
    segs.push(format!("{elapsed:.1}s"));
    segs.push(format!("${cost:.4}"));
    trunc_cols(&segs.join(" · "), 60)
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
) {
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
                if !current_tool.is_empty() {
                    transcript.push_separated(Line::Tool(std::mem::take(current_tool)));
                }
                *current_tool = tool_header(tool, "");
                *live_tool = Some(LiveTool {
                    id: id.clone(),
                    preview: Vec::new(),
                    last: None,
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
                // close the old row and open a fresh live block
                _ => {
                    if !current_tool.is_empty() {
                        transcript.push_separated(Line::Tool(std::mem::take(current_tool)));
                    }
                    *current_tool = tool_header(tool, detail);
                    *live_tool = Some(LiveTool {
                        id: id.clone(),
                        preview: Vec::new(),
                        last: None,
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
                // no matching block (replay paths, id drift): legacy
                // one-shot append on the current row
                _ => append_tool_note(current_tool, excerpt, *is_error),
            }
        }
        Event::CallFinished { .. } => {
            // collapse the live block: the preview was transient; the
            // compact `→ tool ✓|✗ note` row is what gets cached
            if let Some(lt) = live_tool.take() {
                if let Some((excerpt, is_error)) = lt.last {
                    append_tool_note(current_tool, &excerpt, is_error);
                }
            }
            if !current_tool.is_empty() {
                transcript.push_separated(Line::Tool(std::mem::take(current_tool)));
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
        }
        Event::TurnFinished { stop, usage } => {
            let elapsed = busy_since.map_or(0.0, |t| t.elapsed().as_secs_f64());
            let produced = *turn_produced;
            flush_live_text(transcript, current_thought, current_assistant);
            if let Some(lt) = live_tool.take() {
                if let Some((excerpt, is_error)) = lt.last {
                    append_tool_note(current_tool, &excerpt, is_error);
                }
            }
            if !current_tool.is_empty() {
                transcript.push_separated(Line::Tool(std::mem::take(current_tool)));
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
            if !silent {
                let row = match stop {
                    ka_protocol::Stop::Done => Line::Report(format!("done{tail}")),
                    ka_protocol::Stop::Aborted => {
                        Line::Report(format!("aborted · partial kept{tail}"))
                    }
                    ka_protocol::Stop::Length => {
                        Line::Report(format!("stopped at output limit{tail}"))
                    }
                    ka_protocol::Stop::Error => match last_error.take() {
                        Some(msg) => {
                            let msg: String = if msg.chars().count() > 90 {
                                msg.chars().take(89).chain(std::iter::once('…')).collect()
                            } else {
                                msg
                            };
                            Line::ReportErr(format!("failed · {msg}{tail} · /retry"))
                        }
                        None => Line::ReportErr(format!("failed{tail} · /retry")),
                    },
                };
                transcript.push_separated(row);
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
            // the assistant output's closing metadata row
            if produced || !silent {
                transcript.push_separated(Line::Report(turn_meta_row(
                    &meters.model,
                    elapsed,
                    usage.cost,
                )));
            }
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
                transcript.push_separated(Line::ReportErr(message.clone()));
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
                    transcript.push_separated(Line::Report("⋯ digest ⋯".into()));
                    continue;
                }
                if m.role == "user" {
                    transcript.push_separated(Line::User(m.content.clone()));
                } else {
                    transcript.push_separated(Line::Assistant(m.content.clone()));
                }
            }
        }
        Event::Note { message } => transcript.push_separated(Line::Note(message.clone())),
        Event::Inventory {
            tools,
            mcp,
            agents,
            skills,
            prompts,
        } => {
            let card = inventory_card(mcp, agents);
            if !card.is_empty() {
                transcript.push_separated(Line::Report(card));
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
            transcript.push_separated(Line::Note("⋯ digesting context…".to_string()))
        }
        Event::DigestFinished { .. } => {}
        // the run loop opens the /context modal from this event
        Event::ContextBreakdown { .. } => {}
    }
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
/// screen and force a full redraw. The mouse-capture state from the
/// Ctrl+M toggle is preserved. The child's exit status is ignored —
/// editors and pagers exit non-zero routinely.
fn run_external(
    program: &str,
    args: &[&str],
    mouse_capture: bool,
    terminal: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
) -> std::io::Result<()> {
    use std::io::Write as _;
    let _ = std::io::stdout().flush();
    let _ = crossterm::terminal::disable_raw_mode();
    let _ = crossterm::execute!(std::io::stdout(), crossterm::terminal::LeaveAlternateScreen);
    let res = std::process::Command::new(program)
        .args(args)
        .status()
        .map(|_| ());
    let _ = crossterm::execute!(std::io::stdout(), crossterm::terminal::EnterAlternateScreen);
    let _ = crossterm::terminal::enable_raw_mode();
    if mouse_capture {
        let _ = crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture);
        let _ = std::io::stdout().write_all(b"\x1b[?1003h");
        let _ = std::io::stdout().flush();
    }
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
    // custom files come after builtins, prefixed so they read as aliases
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    for c in scan_custom_commands(&cwd) {
        let hint = if c.argument_hint.is_empty() {
            String::new()
        } else {
            format!(" ({})", c.argument_hint)
        };
        out.push((
            format!("cmd:{}", c.name),
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
        ("/prompt".to_string(), "run an MCP prompt".to_string()),
        (
            "/provider".to_string(),
            "connect a provider (api key)".to_string(),
        ),
        ("/mode".to_string(), "pick a permission mode".to_string()),
        (
            "/plan".to_string(),
            "research the task, write .ka/plans/plan.md".to_string(),
        ),
        ("/build".to_string(), "implement the plan file".to_string()),
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
    /// Strand tree.
    Tree,
    /// Help overlay.
    Help,
}

/// Load a custom command body from `.ka/commands/<name>.md` (project) or
/// the user dir; `$ARGUMENTS` substituted with the rest of the line.
fn project_trusted_in(state_home: &std::path::Path, cwd: &std::path::Path) -> bool {
    let Ok(text) = std::fs::read_to_string(state_home.join("ka/trust.json")) else {
        return false;
    };
    text.contains(&cwd.to_string_lossy().to_string())
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
    if project_trusted_in(state_home, cwd) {
        dirs.extend(PROJECT_COMMAND_DIRS.iter().map(|d| cwd.join(d)));
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
    let mut candidates = vec![
        cwd.join(format!(".ka/commands/{name}.md")),
        cwd.join(format!(".agents/commands/{name}.md")),
        cwd.join(format!(".claude/commands/{name}.md")),
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
        if !is_user_file && !project_trusted_in(state_home, cwd) {
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
            followup: Some(build_followup()),
        }),
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
        "/export" => Some(Slash {
            note: None,
            event: Some(Command::ExportMarkdown {
                out: rest.map(std::path::PathBuf::from),
            }),
            quit: false,
            followup: None,
            modal: None,
        }),
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
            event: Some(Command::ContextBreakdown),
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
    tool: Vec<ratatui::text::Line<'static>>,
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
    let mut out: String = line.chars().take(width.saturating_sub(1)).collect();
    if out.chars().count() < line.chars().count() {
        out.push('…');
    }
    out
}

/// Live tool block rows on the tool band: the `→ {tool}` header plus up
/// to [`PREVIEW_WINDOW`] dim preview lines while the call is unfinished.
/// Every row is inset one column and filled to the full transcript
/// width with [`BG_TOOL`] — the band is the live block's background,
/// same fill treatment as the assistant surface.
fn tool_live_rows(
    header: &str,
    live: Option<&LiveTool>,
    width: usize,
) -> Vec<ratatui::text::Line<'static>> {
    use ratatui::text::Line as TuiLine;
    let Some(lt) = live else {
        return Vec::new();
    };
    // one full-width band row: one column of air, the content, band fill.
    // The bg must live on a SPAN — Paragraph ignores line-level styles
    let band_row = |content: String, fg: ratatui::style::Color| {
        TuiLine::from(vec![ratatui::text::Span::styled(
            pad_to_width(format!(" {content}"), width),
            ratatui::style::Style::new()
                .fg(fg)
                .bg(crate::palette::BG_TOOL),
        )])
    };
    let mut rows = vec![band_row(header.to_string(), crate::palette::TOOL)];
    let start = lt.preview.len().saturating_sub(PREVIEW_WINDOW);
    for line in &lt.preview[start..] {
        rows.push(band_row(
            format!("  {}", preview_row(line, width)),
            crate::palette::FAINT,
        ));
    }
    rows
}

/// Append the compact ` ✓|✗ {first line ≤120}` note for a finished call.
fn append_tool_note(row: &mut String, excerpt: &str, is_error: bool) {
    let first = excerpt.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let note: String = first.chars().take(120).collect();
    if is_error {
        row.push_str(&format!(" ✗ {note}"));
    } else {
        row.push_str(&format!(" ✓ {note}"));
    }
}

/// The one selection identity of the whole TUI: a full-row petrol bar
/// carrying cream text (pad_to_width fills the row with SEL_BG).
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
fn discover_memory_files(cwd: &std::path::Path) -> Vec<(std::path::PathBuf, String)> {
    let mut out = Vec::new();
    let project = cwd.join("MEMORY.md");
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

/// The status bar's right zone: `{model} · {mode} · ctx {pct}% ·
/// ${cost}` — facts joined only when known, cost always on.
fn status_right(meters: &Meters) -> String {
    let (used, window) = meters.context;
    let mut segs: Vec<String> = Vec::new();
    if !meters.model.is_empty() {
        segs.push(meters.model.clone());
    }
    if !meters.mode.is_empty() {
        segs.push(meters.mode.clone());
    }
    if window > 0 {
        segs.push(format!(
            "ctx {}%",
            (used as f64 / window as f64 * 100.0) as u64
        ));
    } else if used > 0 {
        segs.push(format!("~{} tok", fmt_tok(used)));
    }
    segs.push(format!("${:.4}", meters.cost));
    segs.join(" · ")
}

#[allow(clippy::too_many_arguments)]
fn render(
    frame: &mut ratatui::Frame,
    transcript: &Transcript,
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
    sidebar_zone: &std::cell::Cell<Option<SidebarZone>>,
    title_arrows: &std::cell::Cell<Option<TitleArrows>>,
) {
    let header_glyph = sidebar.header_glyph.as_str();
    use ratatui::layout::Constraint::{Length, Min};
    use ratatui::style::{Modifier, Style};
    use ratatui::text::{Line as TuiLine, Span};
    use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Wrap};
    // ── canvas: paint the whole frame before any widget so the app sits
    // on the charcoal ground with cream prose, whatever the terminal
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
        Length(input_height(input_area_rows(ask, picker, input))),
        Length(1),
    ])
    .split(outer);
    // wide terminals: the transcript row splits, the sidebar takes a
    // fixed right column; narrow terminals keep the full-width row.
    // The threshold reads the raw terminal width, matching
    // `transcript_width`'s sidebar check.
    let (tx_area, sb_area) = if frame.area().width >= SIDEBAR_MIN_WIDTH {
        let cols = ratatui::layout::Layout::horizontal([
            ratatui::layout::Constraint::Min(0),
            ratatui::layout::Constraint::Length(SIDEBAR_WIDTH),
        ])
        .split(chunks[1]);
        (cols[0], Some(cols[1]))
    } else {
        (chunks[1], None)
    };
    let tx_inner = tx_area.width.saturating_sub(2);
    // modals center within the transcript pane, one col clear of its
    // edges: they never touch the sidebar border, the input box, or the
    // status bar
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
        live_rows.extend(lb.tool.iter().cloned());
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
        // rides below the last live row and is never cached
        live_rows.push(working_row(ask, busy_since, now));
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
    let title = if pinned {
        header_glyph.to_string()
    } else {
        format!("{header_glyph} · ↑{} above (pgdn/esc)", start)
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
    // Only once the user has sent a message (nothing to jump to
    // before), and only when the pane is wide enough that the arrows
    // never collide with the title text. ▲ steps to the previous user
    // message, ▼ to the next; both zones are recorded for the mouse
    // handler.
    title_arrows.set(None);
    let has_user = transcript
        .entries()
        .iter()
        .any(|l| matches!(l, Line::User(_)));
    if has_user && tx_area.width >= 12 {
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

    // ── sidebar: session · todos · mcp · skills · agents · info ──
    if let Some(sb) = sb_area {
        // left border + horizontal padding: content starts two cols in
        // from the border, one col of air remains at the right edge
        let rows = sidebar_rows(
            sidebar,
            meters,
            (SIDEBAR_WIDTH - 3) as usize,
            sb.height as usize,
            Some((sidebar_zone, sb)),
        );
        let widget = Paragraph::new(rows)
            .block(
                Block::default()
                    .borders(Borders::LEFT)
                    .title(padded_title(header_glyph))
                    .border_style(crate::palette::BORDER_STYLE)
                    .padding(ratatui::widgets::Padding::horizontal(1)),
            )
            .style(ratatui::style::Style::new().bg(crate::palette::BG_PANEL));
        frame.render_widget(widget, sb);
    } else {
        // no sidebar column: no clickable zones this frame
        sidebar_zone.set(None);
    }
    // ── input ─────────────────────────────────────────────────────
    // a permission ask borrows the box as a form (the draft returns
    // after the answer); titles carry only structural/draft state —
    // the action hints live in the status bar
    let title = if ask.is_some() {
        "permission".to_string()
    } else if picker.is_some() {
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
    let body: Vec<TuiLine> = if let Some(ask) = ask {
        // question text, then one options row: the selected option is
        // inverse video, every option carries a single leading space so
        // the text never shifts when the selection moves
        let mut rows = vec![TuiLine::from(ask.question.as_str())];
        if let Some(detail) = &ask.detail {
            rows.extend(ask_detail_rows(detail, ASK_DETAIL_MAX));
        }
        let mut opts: Vec<Span> = Vec::new();
        for (i, opt) in ask.options.iter().enumerate() {
            if i == ask.selected {
                opts.push(Span::styled(format!(" {opt}"), selection_style()));
            } else {
                opts.push(Span::raw(format!(" {opt}")));
            }
        }
        rows.push(TuiLine::from(opts));
        rows
    } else if let Some(pk) = picker {
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
    } else if input.is_empty() && !busy && popup.is_none() && path.is_none() && modal.is_none() {
        vec![TuiLine::styled("ask ka", crate::palette::PLACEHOLDER)]
    } else {
        // shared horizontal window: all rows shift together so the cursor
        // row can always show the cursor
        let (_, cur_col) = cursor_row_col(input, cursor);
        let inner_w = chunks[2].width.saturating_sub(4) as usize; // borders + padding
        let scroll_col = cur_col.saturating_sub(inner_w.saturating_sub(1).max(1));
        input
            .split('\n')
            .map(|r| TuiLine::from(r.chars().skip(scroll_col).collect::<String>()))
            .collect()
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
    if ask.is_none() && picker.is_none() {
        let (cur_row, cur_col) = cursor_row_col(input, cursor);
        let inner_w = chunks[2].width.saturating_sub(4) as usize; // borders + padding
        let scroll_col = cur_col.saturating_sub(inner_w.saturating_sub(1).max(1));
        let area = chunks[2];
        frame.set_cursor_position((
            area.x + 2 + (cur_col - scroll_col) as u16,
            area.y + 1 + cur_row.min(area.height.saturating_sub(2) as usize) as u16,
        ));
    }

    // ── status bar: contextual key hints left, meters right ───────
    let hints: Vec<Span<'static>> = if ask.is_some() {
        hint_spans(&[(" ↑↓", "select"), (" ⏎", "confirm"), (" esc", "deny")])
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
            Modal::Memory { .. } => hint_spans(&[(" esc", "close")]),
            Modal::Usage { .. } => hint_spans(&[(" any", "close")]),
            Modal::Context { .. } => hint_spans(&[(" any", "close")]),
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
    } else {
        hint_spans(&[(" enter", "send"), (" /", "commands")])
    };
    let right = status_right(meters);
    let w = unicode_width::UnicodeWidthStr::width;
    let left_cols: usize = hints.iter().map(|s| w(s.content.as_ref())).sum();
    // one leading + one trailing col of air: the left zone starts a col
    // in, the right zone ends a col before the chunk edge
    let pad = chunks[3]
        .width
        .saturating_sub(2)
        .saturating_sub((left_cols + w(right.as_str())) as u16)
        .max(1) as usize;
    let mut bar: Vec<Span<'static>> = vec![Span::raw(" ")];
    bar.extend(hints);
    bar.push(Span::raw(" ".repeat(pad)));
    bar.push(Span::styled(right, crate::palette::META));
    bar.push(Span::raw(" "));
    frame.render_widget(Paragraph::new(TuiLine::from(bar)), chunks[3]);

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
        let inner_w = rect.width.saturating_sub(4) as usize; // borders + padding
        let mut text = Vec::new();
        for (i, (name, desc)) in popup.items.iter().take(7).enumerate() {
            let desc_trim: String = desc.chars().take(32).collect();
            let row = format!("{name:<12} {desc_trim}");
            if i == popup.selected {
                text.push(TuiLine::styled(
                    pad_to_width(row, inner_w),
                    selection_style(),
                ));
            } else {
                text.push(TuiLine::raw(row));
            }
        }
        let widget = Paragraph::new(text)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(padded_title("commands"))
                    .border_style(crate::palette::BORDER_STYLE)
                    .padding(ratatui::widgets::Padding::horizontal(1)),
            )
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
        let inner_w = rect.width.saturating_sub(4) as usize; // borders + padding
        let mut text = Vec::new();
        for (i, (name, is_dir)) in path.entries.iter().take(7).enumerate() {
            let slash = if *is_dir { "/" } else { "" };
            if i == path.selected {
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
                Block::default()
                    .borders(Borders::ALL)
                    .title(padded_title(if path.mentions { "files" } else { "path" }))
                    .border_style(crate::palette::BORDER_STYLE)
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
                let mut text = vec![TuiLine::from(vec![
                    Span::styled("filter: ", ratatui::style::Style::default()),
                    Span::styled(picker.filter.clone(), crate::palette::ACCENT_STYLE),
                ])];
                let inner_w = width.saturating_sub(4) as usize; // borders + padding
                for (i, (label, detail)) in rows.iter().take((height as usize) - 5).enumerate() {
                    let row = pad_to_width(format!("{label}  —  {detail}"), inner_w);
                    if i == picker.selected {
                        text.push(TuiLine::styled(row, selection_style()));
                    } else {
                        text.push(TuiLine::raw(row));
                    }
                }
                let widget = Paragraph::new(text)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(padded_title("sessions"))
                            .border_style(crate::palette::BORDER_STYLE)
                            .padding(ratatui::widgets::Padding::new(1, 1, 1, 1)),
                    )
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
                let width = 64.min(frame.area().width);
                let rect = centered(width, height, modal_area);
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
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(padded_title("api key"))
                            .border_style(crate::palette::BORDER_STYLE)
                            .padding(ratatui::widgets::Padding::new(1, 1, 1, 1)),
                    )
                    .style(ratatui::style::Style::new().bg(crate::palette::BG_SURFACE))
                    .wrap(Wrap { trim: false });
                frame.render_widget(widget, rect);
            }
            Modal::Help => {
                let height = 34u16.min(frame.area().height.saturating_sub(2));
                let width = 66.min(frame.area().width);
                let rect = centered(width, height, modal_area);
                let keys = vec![
                    ("Enter", "send · interject mid-turn"),
                    ("Esc / Ctrl-C", "abort turn · close overlays · unpin scroll"),
                    ("Ctrl+C", "quit (abort the running turn first)"),
                    ("Ctrl+M", "mouse capture · shift+drag select"),
                    ("Ctrl+R", "search history · ctrl+r next · enter accept"),
                    ("Alt+E", "edit the draft in $EDITOR"),
                    ("PgUp / PgDn", "scroll the transcript"),
                    ("↑ ↓", "history · navigate pickers"),
                    ("Tab", "complete slash command"),
                    ("@path + Tab", "complete any file in the project"),
                    ("/find <text>", "search the transcript · bare repeats"),
                    ("/spills", "browse spill files in $PAGER"),
                    ("Alt+←/→", "word jump"),
                    ("Ctrl+U/K/W", "kill to line start/end · word"),
                    ("Ctrl+Y", "yank"),
                    ("Ctrl+Z", "undo"),
                    ("Ctrl+L", "clear screen"),
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
                text.push(TuiLine::from(Span::styled(
                    "custom commands: .ka/commands/*.md (project, trust-gated) or \
~/.config/ka/commands/*.md; body supports $ARGUMENTS",
                    ratatui::style::Style::default().fg(crate::palette::META),
                )));
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
                            .title(padded_title("help"))
                            .border_style(crate::palette::BORDER_STYLE)
                            .padding(ratatui::widgets::Padding::new(1, 1, 1, 1)),
                    )
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
                let mut text = vec![TuiLine::from(vec![
                    Span::styled("filter: ", ratatui::style::Style::default()),
                    Span::styled(picker.filter.clone(), crate::palette::ACCENT_STYLE),
                ])];
                let cap = (height as usize).saturating_sub(5);
                let inner_w = width.saturating_sub(4) as usize; // borders + padding
                for (i, m) in rows.iter().take(cap).enumerate() {
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
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(padded_title(
                                picker
                                    .vendor
                                    .as_ref()
                                    .map_or("model".to_string(), |v| format!("model · {v}")),
                            ))
                            .border_style(crate::palette::BORDER_STYLE)
                            .padding(ratatui::widgets::Padding::new(1, 1, 1, 1)),
                    )
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
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(padded_title("providers"))
                            .border_style(crate::palette::BORDER_STYLE)
                            .padding(ratatui::widgets::Padding::new(1, 1, 1, 1)),
                    )
                    .style(ratatui::style::Style::new().bg(crate::palette::BG_SURFACE))
                    .wrap(Wrap { trim: false });
                frame.render_widget(widget, rect);
            }
            Modal::Settings(panel) => {
                // border(2) + vertical padding(2) + the ROWS/provider rows
                let height = (SettingsPanel::ROWS + panel.providers.len() + 7) as u16;
                let height = height.min(frame.area().height.saturating_sub(2));
                let width = 72.min(frame.area().width);
                let rect = centered(width, height, modal_area);
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
                        format!("  +{} more (see `ka providers`)", rest.len()),
                        crate::palette::META,
                    ));
                }
                let widget = Paragraph::new(text)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(padded_title("settings"))
                            .border_style(crate::palette::BORDER_STYLE)
                            .padding(ratatui::widgets::Padding::new(1, 1, 1, 1)),
                    )
                    .style(ratatui::style::Style::new().bg(crate::palette::BG_SURFACE))
                    .wrap(Wrap { trim: false });
                frame.render_widget(widget, rect);
            }
            Modal::Spills { items, selected } => {
                // border(2) + vertical padding(2) + list rows
                let height = (items.len() as u16 + 5).clamp(7, 21);
                let width = 68.min(frame.area().width);
                let rect = centered(width, height, modal_area);
                let inner_w = width.saturating_sub(4) as usize; // borders + padding
                let mut text = Vec::new();
                if items.is_empty() {
                    text.push(TuiLine::styled(
                        "(no spill files yet — full tool output lands here)",
                        crate::palette::META,
                    ));
                }
                let cap = (height as usize).saturating_sub(5);
                for (i, path) in items.iter().take(cap).enumerate() {
                    if i == *selected {
                        text.push(TuiLine::styled(
                            pad_to_width(path.clone(), inner_w),
                            selection_style(),
                        ));
                    } else {
                        text.push(TuiLine::raw(path.clone()));
                    }
                }
                let widget = Paragraph::new(text)
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(padded_title("spills"))
                            .border_style(crate::palette::BORDER_STYLE)
                            .padding(ratatui::widgets::Padding::new(1, 1, 1, 1)),
                    )
                    .style(ratatui::style::Style::new().bg(crate::palette::BG_SURFACE))
                    .wrap(Wrap { trim: false });
                frame.render_widget(widget, rect);
            }
            Modal::Prompts { items, selected } => {
                let height = (items.len() as u16 + 5).clamp(7, 21);
                let width = 68.min(frame.area().width);
                let rect = centered(width, height, modal_area);
                let inner_w = width.saturating_sub(4) as usize;
                let mut text = Vec::new();
                if items.is_empty() {
                    text.push(TuiLine::styled(
                        "(no MCP prompts advertised)",
                        crate::palette::META,
                    ));
                }
                let cap = (height as usize).saturating_sub(5);
                for (i, row) in items.iter().take(cap).enumerate() {
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
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(padded_title("prompts"))
                            .border_style(crate::palette::BORDER_STYLE)
                            .padding(ratatui::widgets::Padding::new(1, 1, 1, 1)),
                    )
                    .style(ratatui::style::Style::new().bg(crate::palette::BG_SURFACE))
                    .wrap(Wrap { trim: false });
                frame.render_widget(widget, rect);
            }
            Modal::Memory { rows } => {
                let height = (rows.len() as u16 + 4).clamp(6, 24);
                let width = 72.min(frame.area().width);
                let rect = centered(width, height, modal_area);
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
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(padded_title("memory"))
                            .border_style(crate::palette::BORDER_STYLE)
                            .padding(ratatui::widgets::Padding::new(1, 1, 1, 1)),
                    )
                    .style(ratatui::style::Style::new().bg(crate::palette::BG_SURFACE))
                    .wrap(Wrap { trim: false });
                frame.render_widget(widget, rect);
            }
            Modal::Usage { rows } => {
                let height = (rows.len() as u16 + 4).clamp(6, 24);
                let width = 72.min(frame.area().width);
                let rect = centered(width, height, modal_area);
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
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(padded_title("usage"))
                            .border_style(crate::palette::BORDER_STYLE)
                            .padding(ratatui::widgets::Padding::new(1, 1, 1, 1)),
                    )
                    .style(ratatui::style::Style::new().bg(crate::palette::BG_SURFACE))
                    .wrap(Wrap { trim: false });
                frame.render_widget(widget, rect);
            }
            Modal::Context { rows } => {
                let height = (rows.len() as u16 + 4).clamp(6, 24);
                let width = 72.min(frame.area().width);
                let rect = centered(width, height, modal_area);
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
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(padded_title("context"))
                            .border_style(crate::palette::BORDER_STYLE)
                            .padding(ratatui::widgets::Padding::new(1, 1, 1, 1)),
                    )
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
                let inner_w = width.saturating_sub(4) as usize;
                let mut text = Vec::new();
                if items.is_empty() {
                    text.push(TuiLine::styled(
                        "(no related sessions)",
                        crate::palette::META,
                    ));
                }
                let cap = (height as usize).saturating_sub(5);
                for (i, row) in items.iter().take(cap).enumerate() {
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
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(padded_title("tree"))
                            .border_style(crate::palette::BORDER_STYLE)
                            .padding(ratatui::widgets::Padding::new(1, 1, 1, 1)),
                    )
                    .style(ratatui::style::Style::new().bg(crate::palette::BG_SURFACE))
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
        // top margin + input area + footer + transcript top border
        assert_eq!(visible_rows(24, 3), 18);
        assert_eq!(visible_rows(5, 3), 0, "never underflows");
        assert_eq!(visible_rows(0, 0), 0);
        // growth of the input eats the viewport one row at a time
        assert_eq!(visible_rows(24, 8), 13);
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
    fn wheel_lines_roundtrip_repins_at_tail() {
        let mut scroll = None;
        line_up(&mut scroll, 100, 20);
        assert_eq!(scroll, Some(80 - 3), "pinned wheel-up unpins 3 rows");
        line_up(&mut scroll, 100, 20);
        assert_eq!(scroll, Some(80 - 6));
        line_down(&mut scroll, 100, 20);
        assert_eq!(scroll, Some(80 - 3));
        line_down(&mut scroll, 100, 20);
        line_down(&mut scroll, 100, 20);
        assert_eq!(scroll, None, "reaching the tail re-pins");
        // wheel-down while pinned is a no-op
        let mut pinned = None;
        line_down(&mut pinned, 100, 20);
        assert_eq!(pinned, None);
        // small transcript: wheel-up is a no-op
        let mut small = None;
        line_up(&mut small, 5, 20);
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
        ] {
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
        assert_eq!(
            status_right(&m),
            "ollama/qwen3.5:9b · guarded · ctx 10% · $0.0123"
        );
        // fresh session: unknown model/mode/window collapse away
        assert_eq!(status_right(&Meters::default()), "$0.0000");
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
        let out = super::render_line(&Line::Assistant("**hi** there".into()), 40);
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
        assert_eq!(row_text(content).trim_end(), "hi there");
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
        assert!(text(&out[1]).starts_with("❯ alpha"));
        assert!(
            text(&out[2]).starts_with("  beta"),
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
        std::fs::write(
            state.join("ka/trust.json"),
            format!("{{\"projects\":[{{\"path\":\"{}\"}}]}}", dir.display()),
        )
        .unwrap();
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
            with_path.event,
            Some(Command::ExportMarkdown { out: Some(ref p) }) if p == &std::path::PathBuf::from("o.md")
        ));
        assert!(matches!(
            slash_command("/export").unwrap().event,
            Some(Command::ExportMarkdown { out: None })
        ));
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

        apply_event(
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
        apply_event(
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
        apply_event(
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

        apply_event(
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
        apply_event(
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
        apply_event(
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
        apply_event(
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
        apply_event(
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
        apply_event(
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
        apply_event(
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
        apply_event(
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
        apply_event(
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
        apply_event(
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
        apply_event(
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
                Line::Tool(s) => format!("T:{s}"),
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
                "T:→ bash ✓ line-3",
                "R:",
                "A:after",
                "R:",
                "T:→ bash ✗ boom",
                "R:",
                "R:0.0s · $0.0000",
            ],
            "final order must interleave text and tools: {shapes:?}"
        );
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
        // width truncation is char-based, ellipsis-marked
        let wide = "é".repeat(20);
        let row = preview_row(&wide, 10);
        assert_eq!(row.chars().count(), 10, "{row:?}");
        assert!(row.ends_with('…'));
        assert_eq!(preview_row("short", 10), "short");
        // the block renders header + at most the 3 newest dim lines
        let lt = LiveTool {
            id: "c1".into(),
            preview: window.clone(),
            last: Some(("line-5".into(), false)),
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
        // inset one col, padded to the full band width
        assert_eq!(texts[0].trim_end(), " → bash");
        assert_eq!(
            texts[1..].iter().map(|t| t.trim_end()).collect::<Vec<_>>(),
            ["   line-3", "   line-4", "   line-5"]
        );
        for (i, row) in rows.iter().enumerate() {
            assert_eq!(texts[i].chars().count(), 40, "band row fills the width");
            assert!(
                row.spans
                    .iter()
                    .all(|s| s.style.bg == Some(crate::palette::BG_TOOL)),
                "live rows ride BG_TOOL: {row:?}"
            );
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

        apply_event(
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
        apply_event(
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
        apply_event(
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
        apply_event(
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
            4,
            "assistant, air, error report, meta row — nothing stranded: {entries:?}"
        );
        assert!(matches!(entries[0], Line::Assistant(_)));
        // air opens between the card and the error report (family change)
        assert_eq!(&entries[1], &Line::Report(String::new()));
        let Line::ReportErr(report) = &entries[2] else {
            panic!("third entry must be a ReportErr: {entries:?}");
        };
        for want in ["failed", "429", "/retry", "in", "out", "$0.0002"] {
            assert!(report.contains(want), "report {report:?} lacks {want}");
        }
        // the closing meta row follows its ReportErr sibling tight: same
        // meta family, and no blank is stranded after the turn's last row
        assert_eq!(&entries[3], &Line::Report("0.0s · $0.0002".into()));
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

        apply_event(
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
        apply_event(
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
        t.push(Line::Note("HELLO again".into()));
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
        t.push_separated(Line::Note("beta".into()));
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
        apply_event(
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
        let [Line::Report(text)] = entries else {
            panic!("one Report row expected, got {entries:?}")
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
        let [Line::Report(text)] = entries else {
            panic!("one Report row expected, got {entries:?}")
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
        t.push_separated(Line::Tool("→ read · lib.rs".into()));
        t.push_separated(Line::Tool("→ bash ✓ ok".into()));
        t.push_separated(Line::Assistant("answer".into()));
        // a turn ends on meta; the next user card reopens the air
        t.push_separated(Line::Report("done · 0.0s".into()));
        t.push_separated(Line::Report("mock · 0.0s · $0".into()));
        t.push_separated(Line::User("again".into()));
        let shapes: Vec<String> = t
            .entries()
            .iter()
            .map(|l| match l {
                Line::User(s) => format!("U:{s}"),
                Line::Assistant(s) => format!("A:{s}"),
                Line::Tool(s) => format!("T:{s}"),
                Line::Report(s) => format!("R:{s}"),
                _ => "?".to_string(),
            })
            .collect();
        assert_eq!(
            shapes,
            [
                "T:→ read · lib.rs",
                "T:→ bash ✓ ok",
                "R:",
                "A:answer",
                "R:",
                "R:done · 0.0s",
                "R:mock · 0.0s · $0",
                "R:",
                "U:again",
            ],
            "tool runs stay tight, meta runs stay tight, families get air"
        );
        // nothing is stranded: the blank is exactly BETWEEN the blocks
        assert_ne!(
            t.entries().last(),
            Some(&Line::Report(String::new())),
            "no trailing blank after the final user row"
        );
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
            apply_event(
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
        assert_eq!(
            last,
            Some(Line::Tool("→ bash · cargo build".into())),
            "new call closes the old row"
        );
        assert_eq!(head, "→ read · lib.rs");
    }

    #[test]
    fn turn_meta_row_joins_and_caps_at_sixty_columns() {
        assert_eq!(turn_meta_row("m/1", 3.25, 0.0002), "m/1 · 3.2s · $0.0002");
        assert_eq!(turn_meta_row("", 0.0, 0.0), "0.0s · $0.0000");
        let wide = turn_meta_row(&"x".repeat(80), 1.0, 0.0);
        assert!(wide.width() <= 60, "{wide}");
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
    fn ask_form_borrows_the_input_box_rows() {
        let ask = PendingAsk {
            id: AskId("a".into()),
            question: "two\nlines".into(),
            options: vec!["allow".into(), "deny".into()],
            detail: None,
            selected: 0,
        };
        // question lines + one options row, draft ignored while pending
        assert_eq!(input_area_rows(Some(&ask), None, "long\ndraft\ntext"), 3);
        assert_eq!(input_area_rows(None, None, "long\ndraft\ntext"), 3);
        assert_eq!(input_area_rows(None, None, "one line"), 1);
        // the picker borrows the box for its four tier rows
        assert_eq!(
            input_area_rows(
                None,
                Some(&ModePicker::for_mode(ka_protocol::Mode::Free)),
                "x"
            ),
            4
        );
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
        // the ask form reserves rows for the detail block
        let ask = PendingAsk {
            id: AskId("a".into()),
            question: "allow?".into(),
            options: vec!["allow".into()],
            selected: 0,
            detail: Some("ctx\n+added\n".into()),
        };
        assert_eq!(input_area_rows(Some(&ask), None, ""), 4);
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
    fn sidebar_omits_empty_sections_and_shows_session_info() {
        let sidebar = SidebarState {
            cwd: "…/projects/ka".into(),
            branch: Some("main".into()),
            ..Default::default()
        };
        let rows = sidebar_rows(&sidebar, &meters_sample(), 24, 40, None);
        let text = plain_text(&rows);
        assert_eq!(text[0], "session");
        assert!(text.contains(&"model mockco/mock".to_string()), "{text:?}");
        assert!(text.contains(&"mode free".to_string()), "{text:?}");
        assert!(text.contains(&"#3f9c2a81".to_string()), "{text:?}");
        assert!(text.contains(&"$0.0123".to_string()), "{text:?}");
        assert!(
            text.iter().any(|t| t.starts_with("ctx 12000/200000")),
            "{text:?}"
        );
        // no todos/mcp/skills/agents sections when empty
        assert!(!text.contains(&"todos".to_string()), "{text:?}");
        assert!(!text.contains(&"mcp".to_string()), "{text:?}");
        assert!(!text.contains(&"skills".to_string()), "{text:?}");
        assert!(!text.contains(&"agents".to_string()), "{text:?}");
        // info: one `cwd-short:branch` row; the section vanishes without
        // a startup branch snapshot
        assert!(text.contains(&"info".to_string()), "{text:?}");
        assert!(text.contains(&"…/projects/ka:main".to_string()), "{text:?}");
    }

    #[test]
    fn sidebar_todos_mark_done_rows_and_mcp_health() {
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
                prompts: Vec::new(),
                ..Default::default()
            },
            ..Default::default()
        };
        let rows = sidebar_rows(&sidebar, &Meters::default(), 24, 40, None);
        let text = plain_text(&rows);
        let done = text
            .iter()
            .find(|t| t.contains("survey"))
            .expect("done row");
        assert!(done.starts_with("✓ "), "{done}");
        let pending = text
            .iter()
            .find(|t| t.contains("implement"))
            .expect("pending row");
        assert!(pending.starts_with("· "), "{pending}");
        assert!(text.iter().any(|t| t == "demo ✓ 2"), "{text:?}");
        assert!(text.iter().any(|t| t == "jira ✗"), "{text:?}");
        assert!(text.iter().any(|t| t == "rust-docs"), "{text:?}");
        // done rows carry the crossed-out modifier, pending rows do not
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
        // the first pending item is 'next': ACCENT fg + BOLD, others plain
        assert!(rows[pend_idx].spans.iter().any(|s| {
            s.style
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD)
                && s.style.fg == Some(crate::palette::ACCENT)
        }));
    }

    #[test]
    fn sidebar_caps_lists_with_overflow_marks() {
        let many: Vec<String> = (0..30).map(|i| format!("skill-{i}")).collect();
        let sidebar = SidebarState {
            inventory: Inventory {
                skills: many,
                prompts: Vec::new(),
                ..Default::default()
            },
            ..Default::default()
        };
        // Meters::default leaves session/todos/mcp empty, so skills owns
        // the whole budget: header + air + show = min(height-2, 30) list
        // rows, where the cut mark replaces the last one. At height 8:
        // header, blank, skill-0..4, mark.
        let rows = sidebar_rows(&sidebar, &Meters::default(), 24, 8, None);
        let text = plain_text(&rows);
        assert_eq!(text.len(), 8, "{text:?}");
        assert_eq!(text[0], "skills ▾", "{text:?}");
        assert_eq!(text[1], "", "blank row under the header: {text:?}");
        assert_eq!(text[2], "skill-0", "{text:?}");
        assert_eq!(text[6], "skill-4", "{text:?}");
        assert_eq!(text[7], "(+25)", "{text:?}");
    }

    #[test]
    fn skills_header_label_marks_count_when_collapsed() {
        assert_eq!(skills_header_label(true, 4), "skills");
        assert_eq!(skills_header_label(false, 0), "skills (+0)");
        assert_eq!(skills_header_label(false, 12), "skills (+12)");
    }

    #[test]
    fn sidebar_collapsed_skills_render_header_alone() {
        let sidebar = SidebarState {
            inventory: Inventory {
                skills: vec!["a".into(), "b".into(), "c".into()],
                prompts: Vec::new(),
                ..Default::default()
            },
            skills_open: false,
            ..Default::default()
        };
        let rows = sidebar_rows(&sidebar, &Meters::default(), 24, 40, None);
        let text = plain_text(&rows);
        let idx = text
            .iter()
            .position(|t| t == "skills (+3) ▸")
            .expect("collapsed header present");
        // only the header row: the next rendered row is another section's
        // separating blank / header, never a skill name
        assert!(!text.iter().any(|t| t == "a" || t == "b" || t == "c"));
        assert!(idx + 1 < text.len());
    }

    #[test]
    fn sidebar_zone_hit_testing() {
        let zone = SidebarZone::SkillsHeader(ratatui::layout::Rect {
            x: 94,
            y: 5,
            width: 26,
            height: 1,
        });
        assert!(zone.hit(94, 5));
        assert!(zone.hit(119, 5), "right edge of the sidebar chunk");
        assert!(!zone.hit(120, 5), "past the sidebar");
        assert!(!zone.hit(100, 6), "row below the header");
        assert!(!zone.hit(93, 5), "left of the sidebar");
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
    fn skills_header_hover_lifts_onto_surface_tint() {
        use ratatui::style::Modifier;
        let rest = skills_header_line(true, false, 4);
        let rest_spans: Vec<_> = rest
            .spans
            .iter()
            .map(|s| (s.content.as_ref(), s.style))
            .collect();
        assert_eq!(rest_spans[0].0, "skills");
        assert_eq!(rest_spans[1].0, " ▾");
        assert_eq!(rest_spans[0].1.bg, None);
        // collapsed label keeps its count under hover too
        let hovered = skills_header_line(false, true, 4);
        let hovered_spans: Vec<_> = hovered
            .spans
            .iter()
            .map(|s| (s.content.as_ref(), s.style))
            .collect();
        assert_eq!(hovered_spans[0].0, "skills (+4)");
        assert_eq!(hovered_spans[1].0, " ▸");
        for (_, st) in &hovered_spans {
            assert_eq!(st.fg, Some(crate::palette::ACCENT));
            assert_eq!(st.bg, Some(crate::palette::BG_SURFACE));
            assert!(!st.add_modifier.contains(Modifier::UNDERLINED));
        }
    }

    #[test]
    fn sidebar_puts_air_between_sections_and_under_headers() {
        let sidebar = SidebarState {
            cwd: "…/ka".into(),
            branch: Some("main".into()),
            todos: vec![ka_protocol::TodoItem {
                text: "only todo".into(),
                state: ka_protocol::TodoState::Pending,
            }],
            ..Default::default()
        };
        let rows = sidebar_rows(&sidebar, &Meters::default(), 24, 40, None);
        let text = plain_text(&rows);
        // every section header is followed by a blank row; a blank row
        // also separates each section from the previous one
        for (i, row) in text.iter().enumerate() {
            if matches!(row.as_str(), "session" | "todos" | "info") {
                assert_eq!(text[i + 1], "", "blank under header {row}: {text:?}");
                if i > 0 {
                    assert_eq!(text[i - 1], "", "blank before header {row}: {text:?}");
                }
            }
        }
        assert!(text.contains(&"· only todo".to_string()), "{text:?}");
        assert!(text.contains(&"…/ka:main".to_string()), "{text:?}");
    }

    #[test]
    fn sidebar_truncates_multibyte_rows_to_width() {
        let sidebar = SidebarState {
            cwd: "…/世界/世界".into(),
            ..Default::default()
        };
        let rows = sidebar_rows(&sidebar, &Meters::default(), 10, 40, None);
        for row in &rows {
            let text: String = row.spans.iter().map(|s| s.content.as_ref()).collect();
            assert!(text.width() <= 10, "{text}");
        }
    }

    #[test]
    fn transcript_width_shrinks_only_when_sidebar_fits() {
        // margin cols on each side, sidebar once it fits, side padding
        assert_eq!(transcript_width(90), 86, "narrow: margins + padding");
        assert_eq!(transcript_width(100), 70, "at threshold: minus sidebar");
        assert_eq!(transcript_width(120), 90);
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

        apply_event(
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

        apply_event(
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
        apply_event(
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
            apply_event(
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

        // the title renders as the first fact row of the session section
        let rendered = plain_text(&sidebar_rows(&sidebar, &meters, 26, 40, None));
        assert!(
            rendered.iter().any(|r| r.contains("Fix the parser")),
            "got: {rendered:?}"
        );
    }

    #[test]
    fn sidebar_without_title_shows_no_title_row() {
        let sidebar = SidebarState::default();
        let rows = sidebar_rows(&sidebar, &Meters::default(), 26, 40, None);
        assert!(rows.is_empty(), "no title → session section starts empty");
    }

    #[test]
    fn cached_tool_row_is_a_single_band_row() {
        let out = super::render_line(&Line::Tool("→ bash ✓ done".into()), 40);
        assert_eq!(out.len(), 1, "exactly one band row, no trailing blank");
        let row = &out[0];
        let text: String = row.spans.iter().map(|s| s.content.to_string()).collect();
        assert_eq!(text.trim_end(), " → bash ✓ done", "inset one col");
        assert_eq!(text.chars().count(), 40, "band fills the width");
        let band = crate::palette::TOOL_BAND_STYLE;
        assert!(
            row.spans
                .iter()
                .all(|s| s.style.fg == band.fg && s.style.bg == band.bg),
            "cached row rides the tool band: {row:?}"
        );
    }

    #[test]
    fn frame_has_top_margin_glyph_title_and_rounded_input() {
        use ratatui::backend::TestBackend;
        let mut terminal = ratatui::Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal
            .draw(|f| {
                super::render(
                    f,
                    &Transcript::default(),
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
                    &std::cell::Cell::new(None),
                    &std::cell::Cell::new(None),
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
        // the sidebar title carries it too
        let sb_title: String = (93..119u16).map(|x| buf[(x, 1)].symbol()).collect();
        assert!(sb_title.contains('◆'), "sidebar glyph title: {sb_title}");
        // the input box closes with rounded corners (rows 36..39)
        assert_eq!(buf[(1, 36)].symbol(), "╭");
        assert_eq!(buf[(118, 36)].symbol(), "╮");
        assert_eq!(buf[(1, 38)].symbol(), "╰");
        assert_eq!(buf[(118, 38)].symbol(), "╯");
        // the status bar still owns the last row (no bottom margin)
        let last_row: String = (0..120u16).map(|x| buf[(x, 39)].symbol()).collect();
        assert!(last_row.contains("enter"), "status hints on the last row");
    }

    #[test]
    fn title_arrows_render_only_after_user_messages() {
        use ratatui::backend::TestBackend;

        let draw = |t: &Transcript| {
            let arrows: std::cell::Cell<Option<TitleArrows>> = std::cell::Cell::new(None);
            let mut terminal = ratatui::Terminal::new(TestBackend::new(120, 40)).unwrap();
            terminal
                .draw(|f| {
                    super::render(
                        f,
                        t,
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
                        &std::cell::Cell::new(None),
                        &arrows,
                    )
                })
                .unwrap();
            let buf = terminal.backend().buffer();
            let syms: Vec<String> = (88..93u16).map(|x| buf[(x, 1)].symbol().into()).collect();
            (arrows.get(), syms)
        };

        // empty transcript: no arrows, no zone
        let (zone, syms) = draw(&Transcript::default());
        assert!(zone.is_none(), "no user message -> no jump arrows");
        assert!(
            !syms.iter().any(|c| c == "▲" || c == "▼"),
            "no glyphs on the title row: {syms:?}"
        );

        // one user message: arrows drawn at the end of the title row
        let mut t = Transcript::default();
        t.set_width(60);
        for _ in 0..60 {
            t.push(Line::User("a user message".into()));
        }
        let (zone, syms) = draw(&t);
        let z = zone.expect("user message -> jump arrows recorded");
        assert_eq!(syms[2], "▲", "▲ at the end of the title row: {syms:?}");
        assert_eq!(syms[4], "▼", "▼ at the very edge: {syms:?}");
        assert_eq!(z.hit(90, 1), Some(true), "▲ hit zone");
        assert_eq!(z.hit(92, 1), Some(false), "▼ hit zone");
        assert_eq!(z.hit(88, 1), None, "air left of the arrows");
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
                    },
                    ka_protocol::ReplayedMessage {
                        role: "user".into(),
                        content: "after the digest".into(),
                        digest: false,
                    },
                ],
            },
        );
        let text = format!("{:?}", lines.entries());
        assert!(text.contains("digest"), "{text:?}");
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
}
