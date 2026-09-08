//! Markdown renderer for the TUI transcript. One accent color for
//! headings, faint fence lines around syntax-colored code, quiet gutters
//! for quotes, and pure font-modifier emphasis on cream text — all tuned
//! to the complementary cream/charcoal palette in [`crate::palette`]. The only
//! dependency beyond ratatui is unicode-width, already in the tree via
//! ratatui.

use crate::palette;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line as TuiLine, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

fn header_style() -> Style {
    Style::default()
        .fg(palette::HEADING)
        .add_modifier(Modifier::BOLD)
}

/// Blank line after a heading unless the source already has one.
fn push_heading_gap(out: &mut Vec<TuiLine<'static>>, lines: &[&str], i: usize) {
    if lines.get(i + 1).is_some_and(|n| !n.trim_start().is_empty()) {
        out.push(TuiLine::default());
    }
}

/// Render markdown text into styled terminal lines at `width` (the
/// transcript's inner width). Tables size themselves to fit; anything that
/// cannot fit degrades to plain rows so nothing is lost or clipped.
pub fn render(text: &str, width: u16) -> Vec<TuiLine<'static>> {
    let mut out: Vec<TuiLine<'static>> = Vec::new();
    let mut in_code = false;
    let mut code_lang = String::new();
    let mut code_lines: Vec<String> = Vec::new();
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim_start();
        if let Some(fence) = trimmed.strip_prefix("```") {
            if in_code {
                push_code_block(&mut out, &code_lines, &code_lang);
                code_lines.clear();
                code_lang.clear();
                in_code = false;
            } else {
                in_code = true;
                code_lang = fence.trim().to_lowercase();
            }
            i += 1;
            continue;
        }
        if in_code {
            code_lines.push(line.to_string());
            i += 1;
            continue;
        }
        if is_table_row(trimmed) {
            let mut block: Vec<&str> = vec![trimmed];
            while lines
                .get(i + 1)
                .is_some_and(|n| is_table_row(n.trim_start()))
            {
                i += 1;
                block.push(lines[i].trim_start());
            }
            match parse_table(&block).and_then(|(h, b)| render_table(&h, &b, width)) {
                Some(grid) => out.extend(grid),
                None => {
                    for r in &block {
                        out.push(TuiLine::from(inline_spans(r)));
                    }
                    out.push(TuiLine::default());
                }
            }
            i += 1;
            continue;
        }
        if trimmed.is_empty() {
            out.push(TuiLine::default());
            i += 1;
            continue;
        }
        if let Some(h) = trimmed.strip_prefix("#### ") {
            out.push(TuiLine::styled(
                format!("#### {}", strip_atx_closer(h)),
                header_style(),
            ));
            push_heading_gap(&mut out, &lines, i);
        } else if let Some(h) = trimmed.strip_prefix("### ") {
            out.push(TuiLine::styled(
                format!("### {}", strip_atx_closer(h)),
                header_style(),
            ));
            push_heading_gap(&mut out, &lines, i);
        } else if let Some(h) = trimmed.strip_prefix("## ") {
            out.push(TuiLine::styled(
                strip_atx_closer(h).to_string(),
                header_style(),
            ));
            push_heading_gap(&mut out, &lines, i);
        } else if let Some(h) = trimmed.strip_prefix("# ") {
            // h1 is the only level that also underlines
            let style = header_style().add_modifier(Modifier::UNDERLINED);
            out.push(TuiLine::styled(strip_atx_closer(h).to_string(), style));
            push_heading_gap(&mut out, &lines, i);
        } else if trimmed.starts_with(">") {
            let q = trimmed.trim_start_matches('>').trim();
            out.push(TuiLine::from(vec![
                Span::styled("▏ ", palette::BORDER_STYLE),
                Span::styled(q.to_string(), palette::QUOTE),
            ]));
        } else if trimmed.starts_with("- ") || trimmed.starts_with("* ") {
            let indent = list_indent(line, trimmed);
            let rest = &trimmed[2..];
            let mut spans = vec![Span::styled(format!("{indent}• "), palette::ACCENT_STYLE)];
            match task_marker(rest) {
                Some((marker, remainder)) => {
                    spans.push(marker);
                    spans.extend(inline_spans(remainder));
                }
                None => spans.extend(inline_spans(rest)),
            }
            out.push(TuiLine::from(spans));
        } else if is_numbered_item(trimmed) {
            let indent = list_indent(line, trimmed);
            let (num, rest) = trimmed.split_once(". ").unwrap_or(("1", trimmed));
            let mut spans = vec![Span::styled(
                format!("{indent}{num}. "),
                palette::ACCENT_STYLE,
            )];
            spans.extend(inline_spans(rest));
            out.push(TuiLine::from(spans));
        } else if trimmed == "---" || trimmed == "***" {
            out.push(TuiLine::styled(
                "─".repeat(width.min(80) as usize),
                palette::BORDER_STYLE,
            ));
        } else if lines.get(i + 1).and_then(|n| setext_level(n)).is_some() {
            // setext `====` h1 (underlined, like ATX h1)
            let style = header_style().add_modifier(Modifier::UNDERLINED);
            out.push(TuiLine::styled(trimmed.to_string(), style));
            i += 1; // consume the underline
            push_heading_gap(&mut out, &lines, i);
        } else {
            out.push(TuiLine::from(inline_spans(line)));
        }
        i += 1;
    }
    if in_code {
        // unterminated fence: flush what we have
        push_code_block(&mut out, &code_lines, &code_lang);
    }
    wrap_rows(out, width)
}

/// Soft-wrap every row to at most `width` display columns (greedy
/// word wrap, long words split at char boundaries). Rows already at or
/// under the width pass through untouched — tables, fences, rules.
fn wrap_rows(rows: Vec<TuiLine<'static>>, width: u16) -> Vec<TuiLine<'static>> {
    let mut out = Vec::with_capacity(rows.len());
    let w = width as usize;
    if w == 0 {
        return rows;
    }
    for row in rows {
        let used: usize = row.spans.iter().map(|s| s.content.width()).sum();
        if used <= w {
            out.push(row);
        } else {
            out.extend(wrap_spans(row.spans, w).into_iter().map(TuiLine::from));
        }
    }
    out
}

/// Greedy word wrap for styled spans. Breaks at the last space that fits;
/// words wider than the whole width split at char boundaries.
fn wrap_spans(spans: Vec<Span<'static>>, width: usize) -> Vec<Vec<Span<'static>>> {
    let mut chars: Vec<(char, Style)> = Vec::new();
    for s in &spans {
        for c in s.content.chars() {
            chars.push((c, s.style));
        }
    }
    let mut rows: Vec<Vec<(char, Style)>> = Vec::new();
    let mut idx = 0;
    while idx < chars.len() {
        let mut take = 0;
        let mut used = 0;
        let mut last_space: Option<usize> = None;
        while idx + take < chars.len() {
            let cw = chars[idx + take].0.width().unwrap_or(0);
            if used + cw > width {
                break;
            }
            if chars[idx + take].0 == ' ' {
                last_space = Some(take);
            }
            used += cw;
            take += 1;
        }
        if take == 0 {
            take = 1; // a single glyph wider than the width: hard-place it
        }
        let (chunk, next) = if idx + take < chars.len() {
            // prefer breaking at a space over splitting a word; the break
            // space itself is dropped
            match last_space {
                Some(sp) if sp > 0 => (sp, idx + sp + 1),
                _ => (take, idx + take),
            }
        } else {
            (take, idx + take)
        };
        rows.push(chars[idx..idx + chunk].to_vec());
        idx = next;
    }
    rows.into_iter().map(spans_from_chars).collect()
}

/// Merge a styled char stream into the fewest spans.
fn spans_from_chars(chars: Vec<(char, Style)>) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (c, style) in chars {
        match spans.last_mut() {
            Some(last) if last.style == style => last.content.to_mut().push(c),
            _ => spans.push(Span::styled(c.to_string(), style)),
        }
    }
    spans
}

/// Two indent spaces per two leading whitespace characters, capped at four
/// levels so deep nesting stays inside the transcript.
fn list_indent(line: &str, trimmed: &str) -> String {
    let lead = line.chars().count().saturating_sub(trimmed.chars().count());
    "  ".repeat((lead / 2).min(4))
}

/// `- [ ]` / `- [x]` task boxes.
fn task_marker(rest: &str) -> Option<(Span<'static>, &str)> {
    if let Some(r) = rest.strip_prefix("[ ] ") {
        Some((Span::styled("☐ ", Style::new().fg(palette::META)), r))
    } else if let Some(r) = rest
        .strip_prefix("[x] ")
        .or_else(|| rest.strip_prefix("[X] "))
    {
        Some((Span::styled("☑ ", palette::OK), r))
    } else {
        None
    }
}

/// Setext `====` underlines make an h1. `----` is deliberately NOT setext:
/// assistants use `---` as a horizontal rule far more often than as a
/// header underline, and a rule hijacked into a bold header reads as a bug.
fn setext_level(s: &str) -> Option<usize> {
    let t = s.trim();
    if t.len() >= 2 && t.chars().all(|c| c == '=') {
        Some(1)
    } else {
        None
    }
}

/// Strip a closing ATX sequence: `## Header ##` -> `Header`.
fn strip_atx_closer(h: &str) -> &str {
    let h = h.trim_end();
    let stripped = h.trim_end_matches('#');
    if stripped.len() < h.len() && stripped.chars().last().is_some_and(|c| c == ' ') {
        stripped.trim_end()
    } else {
        h
    }
}

/// A candidate table row: pipe-led with at least one inner pipe.
fn is_table_row(trimmed: &str) -> bool {
    trimmed.starts_with('|') && trimmed.matches('|').count() >= 2 && trimmed.chars().count() >= 3
}

/// Split a table row into cell texts, honoring `\|` escapes.
fn split_cells(row: &str) -> Vec<String> {
    let t = row.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = t.strip_suffix('|').unwrap_or(t);
    let mut cells: Vec<String> = vec![String::new()];
    let mut chars = t.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'|') => {
                if let Some(last) = cells.last_mut() {
                    last.push('|');
                }
                chars.next();
            }
            '|' => cells.push(String::new()),
            c => {
                if let Some(last) = cells.last_mut() {
                    last.push(c);
                }
            }
        }
    }
    cells.into_iter().map(|c| c.trim().to_string()).collect()
}

fn is_delim_cell(c: &str) -> bool {
    !c.is_empty() && c.chars().all(|ch| ch == '-' || ch == ':')
}

/// Recognize a GitHub-style table: header row, `---` delimiter row, body.
fn parse_table(rows: &[&str]) -> Option<(Vec<String>, Vec<Vec<String>>)> {
    if rows.len() < 2 {
        return None;
    }
    let header = split_cells(rows[0]);
    let delim = split_cells(rows[1]);
    if header.is_empty() || delim.is_empty() || !delim.iter().all(|c| is_delim_cell(c)) {
        return None;
    }
    let body = rows[2..]
        .iter()
        .map(|r| {
            let mut cells = split_cells(r);
            cells.resize(header.len(), String::new());
            cells
        })
        .collect();
    Some((header, body))
}

/// Render a parsed table as a sharp box-drawn grid (OMP `table` symbols)
/// fitted to `width`. Bars/rules sit in the dim border color; the header is
/// bold on default text. Cells clip with `…`, never overflow. Inner
/// rules separate every body row.
fn render_table(
    header: &[String],
    body: &[Vec<String>],
    width: u16,
) -> Option<Vec<TuiLine<'static>>> {
    let cols = header.len();
    let overhead = 3 * cols + 1; // │ edges + " cell " padding + column bars
    if (width as usize) < overhead + cols {
        return None;
    }
    let mut widths: Vec<usize> = (0..cols)
        .map(|c| {
            let header_w = spans_width(&inline_spans(&header[c]));
            let body_w = body
                .iter()
                .map(|r| spans_width(&inline_spans(r.get(c).map(String::as_str).unwrap_or(""))))
                .max()
                .unwrap_or(0);
            header_w.max(body_w).max(1)
        })
        .collect();

    let available = width as usize - overhead;
    while widths.iter().sum::<usize>() > available {
        let (mi, mw) = widths
            .iter()
            .enumerate()
            .max_by_key(|(_, w)| **w)
            .map(|(i, w)| (i, *w))?;
        if mw <= 1 {
            return None;
        }
        widths[mi] -= 1;
    }

    let mut lines: Vec<TuiLine<'static>> = Vec::new();
    let edge = Span::styled("│", palette::BORDER_STYLE);
    let hrule = |l: char, m: char, r: char| {
        let mut line = String::from(l);
        for (c, w) in widths.iter().enumerate() {
            line.push_str(&"─".repeat(w + 2));
            line.push(if c + 1 == cols { r } else { m });
        }
        TuiLine::styled(line, palette::BORDER_STYLE)
    };

    // top rule
    lines.push(hrule('┌', '┬', '┐'));

    // header row: gold bold
    let mut spans = vec![edge.clone()];
    for (c, h) in header.iter().enumerate() {
        spans.push(Span::styled(" ", Style::default()));
        let fitted: Vec<Span<'static>> = fit_spans(inline_spans(h), widths[c])
            .into_iter()
            .map(|mut s| {
                s.style = Style::default()
                    .fg(palette::ACCENT)
                    .add_modifier(Modifier::BOLD);
                s
            })
            .collect();
        let used = spans_width(&fitted);
        spans.extend(fitted);
        if widths[c] > used {
            spans.push(Span::styled(
                " ".repeat(widths[c] - used),
                Style::default()
                    .fg(palette::ACCENT)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        spans.push(Span::styled(" ", Style::default()));
        if c + 1 < cols {
            spans.push(Span::styled("│", palette::BORDER_STYLE));
        }
    }
    spans.push(edge.clone());
    lines.push(TuiLine::from(spans));

    // header/body rule
    lines.push(hrule('├', '┼', '┤'));

    // body rows (inline markdown intact inside cells), separated by
    // inner rules so every row reads as part of the grid
    for (ri, row) in body.iter().enumerate() {
        let mut spans = vec![edge.clone()];
        for (c, w) in widths.iter().enumerate() {
            spans.push(Span::styled(" ", Style::default()));
            let cell = row.get(c).map(String::as_str).unwrap_or("");
            let fitted = fit_spans(inline_spans(cell), *w);
            let used = spans_width(&fitted);
            spans.extend(fitted);
            if *w > used {
                spans.push(Span::styled(" ".repeat(*w - used), Style::default()));
            }
            spans.push(Span::styled(" ", Style::default()));
            if c + 1 < cols {
                spans.push(Span::styled("│", palette::BORDER_STYLE));
            }
        }
        spans.push(edge.clone());
        lines.push(TuiLine::from(spans));
        if ri + 1 < body.len() {
            lines.push(hrule('├', '┼', '┤'));
        }
    }
    // bottom rule
    lines.push(hrule('└', '┴', '┘'));
    lines.push(TuiLine::default());
    Some(lines)
}

/// Truncate styled spans to `w` terminal columns (unicode display width,
/// so CJK and emoji count as 2), marking a cut with `…`.
fn fit_spans(spans: Vec<Span<'static>>, w: usize) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut left = w;
    for s in spans {
        if left == 0 {
            break;
        }
        let total = s.content.width();
        if total <= left {
            left -= total;
            out.push(s);
            continue;
        }
        // span straddles the boundary: fit char by char
        let mut acc = String::new();
        let mut used = 0;
        for ch in s.content.chars() {
            let cw = ch.width().unwrap_or(0);
            if used + cw > left.saturating_sub(1) {
                break;
            }
            acc.push(ch);
            used += cw;
        }
        if !acc.is_empty() {
            out.push(Span::styled(acc, s.style));
        }
        out.push(Span::styled("…", s.style));
        break;
    }
    out
}

/// Terminal-column width of styled spans.
fn spans_width(spans: &[Span<'static>]) -> usize {
    spans.iter().map(|s| s.content.width()).sum()
}

fn is_numbered_item(s: &str) -> bool {
    let digits = s.chars().take_while(|c| c.is_ascii_digit()).count();
    digits > 0 && s[digits..].starts_with(". ")
}

/// Inline markdown: `**bold**`, `*italic*`, `` `code` ``, `~~strike~~`,
/// `[text](url)` links, `![alt](url)` images, `\`-escapes. Emphasis is pure
/// font modifiers on default text (OMP: `theme.bold`/`theme.italic`).
pub fn inline_spans(s: &str) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut plain = String::new();
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    let flush = |plain: &mut String, spans: &mut Vec<Span<'static>>| {
        if !plain.is_empty() {
            spans.push(Span::raw(std::mem::take(plain)));
        }
    };
    while i < chars.len() {
        let c = chars[i];
        // backslash escape: next char is literal
        if c == '\\' && i + 1 < chars.len() {
            plain.push(chars[i + 1]);
            i += 2;
            continue;
        }
        // image ![alt](url)
        if c == '!' && chars.get(i + 1) == Some(&'[') {
            if let Some((alt, url, next_i)) = parse_link(&chars, i + 1) {
                flush(&mut plain, &mut spans);
                spans.push(Span::styled(
                    format!("🖼 {alt}"),
                    Style::new().fg(palette::FAINT),
                ));
                spans.push(Span::styled(
                    format!(" ({url})"),
                    Style::new().fg(palette::FAINT),
                ));
                i = next_i;
                continue;
            }
        }
        // link [text](url)
        if c == '[' {
            if let Some((text, url, next_i)) = parse_link(&chars, i) {
                flush(&mut plain, &mut spans);
                spans.push(Span::styled(
                    text,
                    Style::new()
                        .fg(palette::TOOL)
                        .add_modifier(Modifier::UNDERLINED),
                ));
                spans.push(Span::styled(
                    format!(" ({url})"),
                    Style::new().fg(palette::FAINT),
                ));
                i = next_i;
                continue;
            }
        }
        // strikethrough ~~text~~
        if c == '~' && chars.get(i + 1) == Some(&'~') {
            if let Some(end) = find_double(&chars, i + 2, '~') {
                flush(&mut plain, &mut spans);
                let struck: String = chars[i + 2..end]
                    .iter()
                    .collect::<String>()
                    .trim()
                    .to_string();
                if !struck.is_empty() {
                    spans.push(Span::styled(
                        struck,
                        Style::default().add_modifier(Modifier::CROSSED_OUT),
                    ));
                }
                i = end + 2;
                continue;
            }
        }
        if c == '`' {
            if let Some(end) = chars[i + 1..].iter().position(|&x| x == '`') {
                flush(&mut plain, &mut spans);
                let code: String = chars[i + 1..i + 1 + end]
                    .iter()
                    .collect::<String>()
                    .trim()
                    .to_string();
                if !code.is_empty() {
                    spans.push(Span::styled(code, Style::new().fg(palette::CODE_INLINE)));
                }
                i += end + 2;
                continue;
            }
        }
        if c == '*' && i + 1 < chars.len() && chars[i + 1] == '*' {
            if let Some(end) = find_double(&chars, i + 2, '*') {
                flush(&mut plain, &mut spans);
                let bold: String = chars[i + 2..end]
                    .iter()
                    .collect::<String>()
                    .trim()
                    .to_string();
                if !bold.is_empty() {
                    spans.push(Span::styled(
                        bold,
                        Style::default()
                            .fg(palette::FG_STRONG)
                            .add_modifier(Modifier::BOLD),
                    ));
                }
                i = end + 2;
                continue;
            }
        }
        if c == '*' && i + 1 < chars.len() && chars[i + 1] != ' ' {
            if let Some(end) = chars[i + 1..].iter().position(|&x| x == '*') {
                flush(&mut plain, &mut spans);
                let italic: String = chars[i + 1..i + 1 + end]
                    .iter()
                    .collect::<String>()
                    .trim()
                    .to_string();
                if !italic.is_empty() {
                    spans.push(Span::styled(
                        italic,
                        Style::default()
                            .fg(palette::FG_STRONG)
                            .add_modifier(Modifier::ITALIC),
                    ));
                }
                i += end + 2;
                continue;
            }
        }
        plain.push(c);
        i += 1;
    }
    flush(&mut plain, &mut spans);
    spans
}

fn find_double(chars: &[char], from: usize, pat: char) -> Option<usize> {
    let mut i = from;
    while i + 1 < chars.len() {
        if chars[i] == pat && chars[i + 1] == pat {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Parse `[text](url)` with `start` at the `[`. Returns
/// `(text, url, index just past the closing paren)`.
fn parse_link(chars: &[char], start: usize) -> Option<(String, String, usize)> {
    let close_text = chars[start + 1..].iter().position(|&c| c == ']')? + start + 1;
    if chars.get(close_text + 1) != Some(&'(') {
        return None;
    }
    let close_url = chars[close_text + 2..].iter().position(|&c| c == ')')? + close_text + 2;
    let text: String = chars[start + 1..close_text]
        .iter()
        .collect::<String>()
        .trim()
        .to_string();
    let url: String = chars[close_text + 2..close_url]
        .iter()
        .collect::<String>()
        .trim()
        .to_string();
    if text.is_empty() || url.is_empty() {
        return None;
    }
    Some((text, url, close_url + 1))
}

/// Code block: faint ` ``` ` fence lines around syntax-colored code, which
/// rides the assistant card surface (BG_OUTPUT).
///
/// Fences tagged with a known language go through [`CodeHighlighter`],
/// which tracks block comments across lines within this block; every
/// other fence keeps the line-local generic `highlight`. Style roles for
/// both paths come from the palette's syntax ramp (consistent across all
/// code surfaces): comments → SYNTAX_COMMENT, strings → SYNTAX_STRING,
/// keywords → SYNTAX_KEYWORD, numbers → SYNTAX_NUMBER, everything else
/// stays CODE_BLOCK.
fn push_code_block(out: &mut Vec<TuiLine<'static>>, lines: &[String], lang: &str) {
    if lines.is_empty() {
        return;
    }
    out.push(TuiLine::styled(
        format!("```{lang}"),
        Style::new().fg(palette::FAINT),
    ));
    let mut hl = code_lang(lang).map(CodeHighlighter::new);
    for line in lines {
        out.push(TuiLine::from(match &mut hl {
            Some(h) => h.line(line),
            None => highlight(line),
        }));
    }
    out.push(TuiLine::styled("```", Style::new().fg(palette::FAINT)));
    out.push(TuiLine::default());
}

const KEYWORDS: &[&str] = &[
    "fn", "let", "mut", "pub", "use", "struct", "enum", "impl", "match", "if", "else", "for",
    "while", "loop", "return", "const", "static", "type", "trait", "where", "async", "await",
    "move", "ref", "as", "in", "true", "false", "None", "Some", "Ok", "Err", "self", "super",
    "crate", "mod", "extern", "unsafe", "dyn", "import", "export", "class", "def", "function",
    "var", "new", "echo", "exit", "then", "fi", "do", "done", "local", "case", "esac", "elif",
    "package", "func", "defer", "go", "chan", "select", "switch", "default", "break", "continue",
    "try", "catch", "finally", "throw", "raise", "yield", "lambda", "pass", "with", "from",
];

/// Tiny generic highlighter with the OMP syntax palette: comments,
/// strings, numbers, keywords. Unclosed quotes and mid-word apostrophes
/// (`don't`) stay literal instead of bleeding color across the line.
pub fn highlight(line: &str) -> Vec<Span<'static>> {
    let base = Style::new().fg(palette::CODE_BLOCK);
    if line.trim().is_empty() {
        return vec![Span::styled(String::new(), base)];
    }
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut rest = line;
    'outer: loop {
        if rest.is_empty() {
            break;
        }
        // line comment anywhere (unless a string opened earlier on the line)
        for marker in ["//", "#"] {
            if let Some(pos) = rest.find(marker) {
                let str_pos = rest.find(['"', '\'']);
                if str_pos.is_none_or(|sp| pos < sp) {
                    if pos > 0 {
                        highlight_plain(&rest[..pos], base, &mut spans);
                    }
                    spans.push(Span::styled(
                        rest[pos..].to_string(),
                        Style::new().fg(palette::SYNTAX_COMMENT),
                    ));
                    return spans;
                }
            }
        }
        // string literal: an opening quote only starts a string when it is
        // not glued to a word (don't, it's — apostrophes) AND closes on the
        // same line; otherwise it is a literal character: keep scanning.
        if let Some(quote_pos) = rest.find(['"', '\'']) {
            let quote = rest.as_bytes()[quote_pos] as char;
            let in_word = quote_pos > 0
                && rest[..quote_pos]
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_alphanumeric() || c == '_');
            let after = &rest[quote_pos + 1..];
            match after.find(quote) {
                Some(end) if !in_word => {
                    if quote_pos > 0 {
                        highlight_plain(&rest[..quote_pos], base, &mut spans);
                    }
                    spans.push(Span::styled(
                        rest[quote_pos..quote_pos + 1 + end + 1].to_string(),
                        Style::new().fg(palette::SYNTAX_STRING),
                    ));
                    rest = &after[end + 1..];
                    continue 'outer;
                }
                _ => {
                    // mid-word apostrophe or unclosed quote: emit the prefix
                    // and the quote itself as plain code, then keep looking
                    if quote_pos > 0 {
                        highlight_plain(&rest[..quote_pos], base, &mut spans);
                    }
                    spans.push(Span::styled(quote.to_string(), base));
                    rest = &rest[quote_pos + 1..];
                    continue 'outer;
                }
            }
        }
        highlight_plain(rest, base, &mut spans);
        break;
    }
    spans
}

fn highlight_plain(chunk: &str, base: Style, spans: &mut Vec<Span<'static>>) {
    for token in chunk.split_inclusive(char::is_whitespace) {
        let bare = token.trim_end_matches(|c: char| !c.is_alphanumeric() && c != '_');
        if KEYWORDS.contains(&bare) {
            spans.push(Span::styled(
                token.to_string(),
                Style::new().fg(palette::SYNTAX_KEYWORD),
            ));
        } else if !bare.is_empty() && bare.chars().next().is_some_and(|c| c.is_ascii_digit()) {
            spans.push(Span::styled(
                token.to_string(),
                Style::new().fg(palette::SYNTAX_NUMBER),
            ));
        } else {
            spans.push(Span::styled(token.to_string(), base));
        }
    }
}

/// Fence-tag languages the per-language highlighter understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CodeLang {
    Rust,
    Python,
    Js,
    Json,
    Toml,
    Shell,
}

/// Map a (lowercased) fence tag to a highlightable language.
fn code_lang(tag: &str) -> Option<CodeLang> {
    match tag {
        "rust" | "rs" => Some(CodeLang::Rust),
        "python" | "py" => Some(CodeLang::Python),
        "js" | "ts" | "javascript" | "typescript" | "jsx" | "tsx" => Some(CodeLang::Js),
        "json" | "jsonc" => Some(CodeLang::Json),
        "toml" => Some(CodeLang::Toml),
        "bash" | "sh" | "shell" | "zsh" => Some(CodeLang::Shell),
        _ => None,
    }
}

impl CodeLang {
    /// `(line-comment marker, block comments exist)`.
    fn comments(self) -> (Option<&'static str>, bool) {
        match self {
            CodeLang::Rust | CodeLang::Js => (Some("//"), true),
            CodeLang::Json => (Some("//"), false),
            CodeLang::Python | CodeLang::Toml | CodeLang::Shell => (Some("#"), false),
        }
    }

    fn keywords(self) -> &'static [&'static str] {
        match self {
            CodeLang::Rust => RUST_KW,
            CodeLang::Python => PYTHON_KW,
            CodeLang::Js => JS_KW,
            CodeLang::Json => JSON_KW,
            CodeLang::Toml => TOML_KW,
            CodeLang::Shell => SHELL_KW,
        }
    }
}

const RUST_KW: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern",
    "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub",
    "ref", "return", "self", "Self", "static", "struct", "super", "trait", "true", "type",
    "unsafe", "use", "where", "while",
];

const PYTHON_KW: &[&str] = &[
    "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del", "elif",
    "else", "except", "False", "finally", "for", "from", "global", "if", "import", "in", "is",
    "lambda", "None", "nonlocal", "not", "or", "pass", "raise", "return", "True", "try", "while",
    "with", "yield",
];

const JS_KW: &[&str] = &[
    "async",
    "await",
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "default",
    "delete",
    "do",
    "else",
    "export",
    "extends",
    "false",
    "finally",
    "for",
    "function",
    "if",
    "import",
    "in",
    "instanceof",
    "let",
    "new",
    "null",
    "of",
    "return",
    "static",
    "super",
    "switch",
    "this",
    "throw",
    "true",
    "try",
    "typeof",
    "undefined",
    "var",
    "void",
    "while",
    "yield",
];

const JSON_KW: &[&str] = &["false", "null", "true"];

const TOML_KW: &[&str] = &["false", "inf", "nan", "true"];

const SHELL_KW: &[&str] = &[
    "break", "case", "continue", "coproc", "do", "done", "elif", "else", "esac", "exit", "export",
    "fi", "for", "function", "if", "in", "local", "return", "select", "set", "then", "time",
    "trap", "until", "while",
];

/// Zero-dependency per-language tokenizer for fenced code. State:
/// `/* */` block comments carry across lines WITHIN one fence; strings
/// are line-scoped, so an unterminated quote colors to end of line and
/// never leaks past the fence (the highlighter is dropped with the
/// block). Apostrophes glued to a word (`don't`, `'a`) never open a
/// string.
struct CodeHighlighter {
    lang: CodeLang,
    /// Nesting depth of an open `/* */` comment (0 = not in one).
    block_depth: usize,
}

impl CodeHighlighter {
    fn new(lang: CodeLang) -> Self {
        Self {
            lang,
            block_depth: 0,
        }
    }

    /// Tokenize one source line into styled spans, carrying block-comment
    /// state across calls.
    fn line(&mut self, src: &str) -> Vec<Span<'static>> {
        let base = Style::new().fg(palette::CODE_BLOCK);
        let comment = Style::new().fg(palette::SYNTAX_COMMENT);
        let string = Style::new().fg(palette::SYNTAX_STRING);
        let number = Style::new().fg(palette::SYNTAX_NUMBER);
        let keyword = Style::new().fg(palette::SYNTAX_KEYWORD);
        if src.trim().is_empty() {
            return vec![Span::styled(String::new(), base)];
        }
        let (line_cmt, block_cmt) = self.lang.comments();
        let keywords = self.lang.keywords();
        let chars: Vec<char> = src.chars().collect();
        let mut out: Vec<(char, Style)> = Vec::with_capacity(chars.len());
        let mut i = 0;
        while i < chars.len() {
            // inside a block comment: scan for the (nesting-aware) close
            if self.block_depth > 0 {
                let start = i;
                while i < chars.len() {
                    if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
                        i += 2;
                        self.block_depth -= 1;
                        if self.block_depth == 0 {
                            break;
                        }
                    } else if chars[i] == '/' && chars.get(i + 1) == Some(&'*') {
                        i += 2;
                        self.block_depth += 1;
                    } else {
                        i += 1;
                    }
                }
                if self.block_depth > 0 {
                    i = chars.len(); // still open at end of line
                }
                for &c in &chars[start..i] {
                    out.push((c, comment));
                }
                continue;
            }
            let c = chars[i];
            // line comment: everything to end of line
            if line_cmt.is_some_and(|m| marker_at(&chars, i, m)) {
                for &c in &chars[i..] {
                    out.push((c, comment));
                }
                break;
            }
            // block comment opener
            if block_cmt && c == '/' && chars.get(i + 1) == Some(&'*') {
                self.block_depth = 1;
                i += 2;
                continue;
            }
            // string literal: closing quote on the same line, `\` escapes;
            // an unterminated string colors the rest of the line
            if matches!(c, '"' | '\'' | '`')
                && !(c == '\'' && i > 0 && (chars[i - 1].is_alphanumeric() || chars[i - 1] == '_'))
            {
                let start = i;
                i += 1;
                while i < chars.len() && chars[i] != c {
                    if chars[i] == '\\' {
                        i += 1;
                    }
                    i += 1;
                }
                if i < chars.len() {
                    i += 1; // the closing quote
                }
                for &ch in &chars[start..i.min(chars.len())] {
                    out.push((ch, string));
                }
                continue;
            }
            // identifier / keyword
            if c.is_alphabetic() || c == '_' {
                let start = i;
                while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                let word: String = chars[start..i].iter().collect();
                let style = if keywords.contains(&word.as_str()) {
                    keyword
                } else {
                    base
                };
                for ch in word.chars() {
                    out.push((ch, style));
                }
                continue;
            }
            // number: digit-led run (0x1F, 3.14, 1_000, 1e5)
            if c.is_ascii_digit() {
                let start = i;
                while i < chars.len()
                    && (chars[i].is_ascii_alphanumeric() || chars[i] == '.' || chars[i] == '_')
                {
                    i += 1;
                }
                for &ch in &chars[start..i] {
                    out.push((ch, number));
                }
                continue;
            }
            out.push((c, base));
            i += 1;
        }
        spans_from_chars(out)
    }
}

/// Whether the char slice has `marker` at position `i`.
fn marker_at(chars: &[char], i: usize, marker: &str) -> bool {
    marker
        .chars()
        .enumerate()
        .all(|(k, mc)| chars.get(i + k) == Some(&mc))
}
#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn line_text(l: &TuiLine) -> String {
        l.spans.iter().map(|s| s.content.to_string()).collect()
    }

    #[test]
    fn renders_headers_and_lists() {
        let md = "# Title\n\n- item one\n- **bold** item\n\n1. first\n2. second\n";
        let lines = render(md, 80);
        assert_eq!(lines.len(), 7);
        assert!(format!("{:?}", lines[0]).contains("Title"));
    }

    #[test]
    fn headings_share_one_accent() {
        let lines = render("# Top\n## Mid\n### Deep\n#### Deepest\n", 80);
        assert!(
            format!("{:?}", lines[0]).contains("underlined()"),
            "h1 underlined"
        );
        assert!(lines[1].spans.is_empty(), "gap after h1");
        assert_eq!(line_text(&lines[2]), "Mid", "h2 has no marker");
        assert!(lines[3].spans.is_empty(), "gap after h2");
        assert_eq!(line_text(&lines[4]), "### Deep", "h3 keeps its hashes");
        assert!(lines[5].spans.is_empty(), "gap after h3");
        assert_eq!(line_text(&lines[6]), "#### Deepest");
        let joined = format!("{:?}", lines);
        assert!(!joined.contains('═'), "no rules: {joined}");
        assert!(!joined.contains('▍'), "no markers: {joined}");
        // single heading color everywhere
        assert!(joined.contains(&format!("{:?}", palette::ACCENT)));
    }

    #[test]
    fn formatted_tokens_keep_single_adjacent_spaces() {
        // inline code and emphasis carry no padding of their own: the
        // source's surrounding spaces are the only ones rendered (one
        // space in, one out, never doubled)
        for (src, want) in [
            (
                "run `cargo build` for **release** builds",
                "run cargo build for release builds",
            ),
            (
                "**bold** and *italic* and `code`",
                "bold and italic and code",
            ),
            ("a **bold** b", "a bold b"),
            ("see [docs](http://x.io) now", "see docs (http://x.io) now"),
            ("** spaced ** and ~~ struck ~~", "spaced and struck"),
            ("x **bold** y", "x bold y"),
        ] {
            let spans = inline_spans(src);
            let joined: String = spans.iter().map(|s| s.content.to_string()).collect();
            assert_eq!(joined, want, "src: {src}");
        }
    }

    #[test]
    fn code_block_uses_dim_fences_and_syntax_colors() {
        let md = "```rust\nfn main() { let x = \"hi\"; } // done\n```\n";
        let lines = render(md, 80);
        assert!(lines.len() >= 3);
        assert_eq!(line_text(&lines[0]), "```rust", "literal fence kept");
        let rendered = format!("{:?}", lines);
        assert!(
            rendered.contains(&format!("{:?}", palette::OK)),
            "string: {rendered}"
        );
        assert!(
            rendered.contains(&format!("{:?}", palette::SYNTAX_KEYWORD)),
            "keyword: {rendered}"
        );
        assert!(
            rendered.contains(&format!("{:?}", palette::META)),
            "comment: {rendered}"
        );
    }

    #[test]
    fn inline_code_and_bold() {
        let spans = inline_spans("run `cargo build` for **release** builds");
        let joined = format!("{spans:?}");
        assert!(joined.contains("cargo build"), "{joined}");
        assert!(
            joined.contains(&format!("{:?}", palette::CODE_INLINE)),
            "inline code tint: {joined}"
        );
        assert!(joined.contains("bold()"), "{joined}");
    }

    /// Bold and italic read as bright warm-white (weight + slant over
    /// the soft gray prose); strike-through stays a pure modifier.
    #[test]
    fn emphasis_is_bright_and_strike_is_plain() {
        let spans = inline_spans("**loud** and *soft* and ~~gone~~");
        let joined = format!("{spans:?}");
        assert!(joined.contains("bold()"), "{joined}");
        assert!(joined.contains("italic()"), "{joined}");
        assert!(joined.contains("crossed_out()"), "{joined}");
        let loud_is_tinted = spans
            .iter()
            .any(|s| s.content == "loud" && s.style.fg == Some(palette::FG_STRONG));
        let soft_is_tinted = spans
            .iter()
            .any(|s| s.content == "soft" && s.style.fg == Some(palette::FG_STRONG));
        let gone_is_plain = spans
            .iter()
            .any(|s| s.content == "gone" && s.style.fg.is_none());
        assert!(loud_is_tinted && soft_is_tinted, "{spans:?}");
        assert!(gone_is_plain, "{spans:?}");
    }

    #[test]
    fn unterminated_fence_flushes() {
        let lines = render("```\nsome code", 80);
        assert!(format!("{lines:?}").contains("code"), "{lines:?}");
    }

    #[test]
    fn highlight_numbers_and_keywords() {
        let spans = highlight("let count = 42; // note");
        let joined = format!("{spans:?}");
        assert!(
            joined.contains(&format!("{:?}", palette::SYNTAX_KEYWORD)),
            "keyword: {joined}"
        );
        assert!(
            joined.contains(&format!("{:?}", palette::WARN)),
            "number: {joined}"
        );
        assert!(
            joined.contains(&format!("{:?}", palette::META)),
            "comment: {joined}"
        );
    }

    #[test]
    fn apostrophes_do_not_open_code_strings() {
        let spans = highlight("greet(\"hi\"); // don't panic");
        let joined = format!("{spans:?}");
        assert!(
            joined.contains(&format!("{:?}", palette::OK)),
            "real string colored: {joined}"
        );
        // an unclosed quote must not color the rest of the line
        let spans = highlight("let s = 'abc;");
        assert!(
            !format!("{spans:?}").contains(&format!("{:?}", palette::OK)),
            "no phantom string"
        );
    }

    #[test]
    fn tables_render_grid_within_width() {
        let md = "| Name | Qty |\n|---|---|\n| alpha | 3 |\n| beta | 12 |\n";
        let lines = render(md, 40);
        let texts: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
            .collect();
        assert!(
            texts[1].contains("Name") && texts[1].contains("Qty"),
            "{texts:?}"
        );
        assert!(
            texts[1].starts_with('│') && texts[1].ends_with('│'),
            "{:?}",
            texts[1]
        );
        assert!(texts[0].starts_with('┌') && texts[0].ends_with('┐') && texts[0].contains('┬'));
        assert!(texts[2].contains('┼') && texts[2].contains('├') && texts[2].contains('┤'));
        assert!(texts[6].starts_with('└') && texts[6].ends_with('┘'));
        // every body row is separated by an inner rule
        assert!(texts[4].starts_with('├') && texts[4].contains('┼'));
        assert!(texts.iter().any(|t| t.contains("alpha")));
        assert!(texts.iter().any(|t| t.contains("12")));
        for t in &texts {
            assert!(t.chars().count() <= 40, "row overflows: {t}");
        }
    }

    #[test]
    fn table_rows_align_to_column_width() {
        let md = "| Language | Year |\n|---|---|\n| Rust | 2010 |\n| Go | 2009 |\n";
        let lines = render(md, 60);
        let bar_cols: Vec<Vec<usize>> = lines
            .iter()
            .filter_map(|l| {
                let mut cols = Vec::new();
                let mut n = 0;
                for s in &l.spans {
                    if s.content.contains('│') {
                        cols.push(n);
                    }
                    n += s.content.chars().count();
                }
                (!cols.is_empty()).then_some(cols)
            })
            .collect();
        assert!(bar_cols.len() >= 3, "{bar_cols:?}");
        assert!(bar_cols.windows(2).all(|w| w[0] == w[1]), "{bar_cols:?}");
    }

    #[test]
    fn table_has_top_and_bottom_rules() {
        let md = "| a | b |\n|---|---|\n| 1 | 2 |\n";
        let lines = render(md, 40);
        let texts: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
            .collect();
        assert_eq!(texts.len(), 6, "{texts:?}");
        assert_eq!(texts[0], "┌───┬───┐");
        assert_eq!(texts[1], "│ a │ b │");
        assert_eq!(texts[2], "├───┼───┤");
        assert_eq!(texts[3], "│ 1 │ 2 │");
        assert_eq!(texts[4], "└───┴───┘");
        assert_eq!(texts[5], "");
    }

    #[test]
    fn table_cells_clip_never_overflow() {
        let md = "| Alpha | Beta |\n|---|---|\n| unexpectedlylongvalue | x |\n";
        let lines = render(md, 16);
        let texts: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
            .collect();
        assert!(texts.iter().any(|t| t.contains('…')), "clip marker present");
        for t in &texts {
            assert!(t.chars().count() <= 16, "row overflows: {t}");
        }
    }

    #[test]
    fn degenerate_width_falls_back_to_plain_rows() {
        let md = "| A | B |\n|---|---|\n| one | two |\n";
        let lines = render(md, 0);
        let joined = format!("{lines:?}");
        assert!(joined.contains("one") && joined.contains("two"), "{joined}");
        assert!(!joined.contains('┼'), "no grid at zero width");
    }

    #[test]
    fn malformed_table_renders_as_plain_text() {
        let md = "| a |\n| b |\n"; // no delimiter row -> not a table
        let lines = render(md, 80);
        let joined = format!("{lines:?}");
        assert!(joined.contains('a') && joined.contains('b'));
        assert!(!joined.contains('┼'));
    }

    #[test]
    fn links_show_text_and_url() {
        let spans = inline_spans("see [docs](http://x.io) now");
        let joined = format!("{spans:?}");
        assert!(joined.contains("docs"), "{joined}");
        assert!(joined.contains("http://x.io"), "{joined}");
        assert!(joined.contains("underlined()"), "{joined}");
        assert!(
            joined.contains(&format!("{:?}", palette::TOOL)),
            "link steel: {joined}"
        );
    }

    #[test]
    fn images_render_alt_and_url() {
        let spans = inline_spans("![logo](http://x.io/l.png)");
        let joined = format!("{spans:?}");
        assert!(joined.contains("🖼 logo"), "{joined}");
        assert!(joined.contains("http://x.io/l.png"), "{joined}");
    }

    #[test]
    fn strikethrough_renders_crossed_out() {
        let spans = inline_spans("~~gone~~");
        let joined = format!("{spans:?}");
        assert!(
            joined.contains("gone") && joined.contains("crossed_out()"),
            "{joined}"
        );
    }

    #[test]
    fn escapes_render_literal_marks() {
        let spans = inline_spans("\\*not italic\\*");
        let joined = format!("{spans:?}");
        assert!(joined.contains("not italic"), "{joined}");
        assert_eq!(joined.matches('\\').count(), 0, "{joined}");
        assert!(!joined.contains("italic()"), "{joined}");
    }

    #[test]
    fn setext_equals_makes_header_dash_stays_rule() {
        let h1 = render("Title\n=====\n", 80);
        let joined = format!("{h1:?}");
        assert!(joined.contains("underlined()"), "{joined}");
        let out = render("Sub\n---\n", 80);
        let joined = format!("{out:?}");
        assert!(joined.contains("Sub"), "{joined}");
        assert!(joined.contains("─"), "rule preserved: {joined}");
        assert!(!joined.contains("bold()"), "no bold header: {joined}");
    }

    #[test]
    fn atx_closing_hashes_are_stripped() {
        let lines = render("## Heading ##\n", 80);
        let joined = format!("{lines:?}");
        assert!(joined.contains("Heading"), "{joined}");
        assert!(!joined.contains("##"), "closing hashes consumed: {joined}");
    }

    #[test]
    fn task_lists_render_boxes() {
        let lines = render("- [ ] todo\n- [x] done\n", 80);
        let joined = format!("{lines:?}");
        assert!(joined.contains('☐'), "{joined}");
        assert!(joined.contains('☑'), "{joined}");
    }

    #[test]
    fn indented_lists_nest() {
        let lines = render("- a\n  - b\n    - c\n", 80);
        let texts: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
            .collect();
        assert!(texts[0].starts_with("• "), "{texts:?}");
        assert!(texts[1].starts_with("  • "), "{texts:?}");
        assert!(texts[2].starts_with("    • "), "{texts:?}");
    }

    #[test]
    fn bullets_use_accent_butter() {
        let lines = render("- item\n", 80);
        assert!(
            format!("{lines:?}").contains(&format!("{:?}", palette::ACCENT)),
            "butter bullet"
        );
    }

    #[test]
    fn long_prose_wraps_within_width() {
        let md = "one two three four five six seven eight nine ten eleven twelve thirteen fourteen fifteen sixteen\n";
        let lines = render(md, 40);
        assert!(lines.len() >= 2, "wrapped into multiple rows");
        for l in &lines {
            let used: usize = l.spans.iter().map(|s| s.content.width()).sum();
            assert!(used <= 40, "row overflows: {:?}", l);
        }
        // no word lost across the wrap
        let all: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect::<Vec<_>>()
            .join(" ");
        for w in ["one", "seven", "twelve", "sixteen"] {
            assert!(all.contains(w), "{w} missing: {all}");
        }
    }

    #[test]
    fn wrapped_rows_stay_single_spans_per_style() {
        let md = "**bold start** then plain text that continues well past the wrap boundary here\n";
        let lines = render(md, 30);
        for l in &lines {
            let used: usize = l.spans.iter().map(|s| s.content.width()).sum();
            assert!(used <= 30, "row overflows: {:?}", l);
        }
        // the bold run stays bold after re-merging spans
        let joined = format!("{:?}", lines);
        assert!(joined.contains("bold()"), "{joined}");
    }

    #[test]
    fn quote_uses_quiet_gutter() {
        let lines = render("> deep thought\n", 60);
        let text = line_text(&lines[0]);
        assert!(text.starts_with("▏ "), "{text}");
        assert!(format!("{:?}", lines[0]).contains("italic()"));
        assert!(
            format!("{:?}", lines[0]).contains(&format!("{:?}", palette::META)),
            "muted gray"
        );
    }
    #[test]
    fn rust_fence_uses_the_language_tokenizer() {
        let mut out: Vec<TuiLine> = Vec::new();
        push_code_block(
            &mut out,
            &["let x = 42; // note".to_string(), "fn f() {}".to_string()],
            "rust",
        );
        let joined = format!("{out:?}");
        assert!(
            joined.contains(&format!("{:?}", palette::SYNTAX_KEYWORD)),
            "keyword: {joined}"
        );
        assert!(
            joined.contains(&format!("{:?}", palette::WARN)),
            "number: {joined}"
        );
        assert!(
            joined.contains(&format!("{:?}", palette::META)),
            "comment: {joined}"
        );
    }

    #[test]
    fn json_and_toml_fences_highlight_basics() {
        let mut out: Vec<TuiLine> = Vec::new();
        push_code_block(&mut out, &[r#"{"k": true, "n": 1}"#.to_string()], "json");
        let joined = format!("{out:?}");
        assert!(
            joined.contains(&format!("{:?}", palette::OK)),
            "json string: {joined}"
        );
        assert!(
            joined.contains(&format!("{:?}", palette::SYNTAX_KEYWORD)),
            "true keyword: {joined}"
        );
        assert!(
            joined.contains(&format!("{:?}", palette::WARN)),
            "json number: {joined}"
        );

        let mut out: Vec<TuiLine> = Vec::new();
        push_code_block(&mut out, &["rate = 0.5 # capped".to_string()], "toml");
        let joined = format!("{out:?}");
        assert!(
            joined.contains(&format!("{:?}", palette::WARN)),
            "toml number: {joined}"
        );
        assert!(
            joined.contains(&format!("{:?}", palette::META)),
            "toml comment: {joined}"
        );

        let mut out: Vec<TuiLine> = Vec::new();
        push_code_block(&mut out, &["for f in *.md; do echo $f; done".into()], "sh");
        let joined = format!("{out:?}");
        assert!(
            joined
                .matches(&format!("{:?}", palette::SYNTAX_KEYWORD))
                .count()
                >= 3,
            "shell keywords (for/in/do/done): {joined}"
        );
    }

    #[test]
    fn block_comments_carry_across_lines_and_strings_do_not_leak() {
        // the comment swallows line one and the head of line two; the
        // tokens after `*/` come back
        let mut out: Vec<TuiLine> = Vec::new();
        push_code_block(
            &mut out,
            &[
                "/* start of".to_string(),
                "still comment */ let x = 1;".to_string(),
            ],
            "rust",
        );
        let second = &out[2];
        let text: String = second.spans.iter().map(|s| s.content.to_string()).collect();
        assert_eq!(text, "still comment */ let x = 1;");
        assert!(
            format!("{second:?}").contains(&format!("{:?}", palette::SYNTAX_KEYWORD)),
            "let styled after the close: {second:?}"
        );

        // unterminated string: rest of line colored, next line unaffected
        let mut out: Vec<TuiLine> = Vec::new();
        push_code_block(
            &mut out,
            &["let s = \"oops".to_string(), "fn after() {}".to_string()],
            "rust",
        );
        assert!(
            format!("{:?}", out[2]).contains(&format!("{:?}", palette::SYNTAX_KEYWORD)),
            "next line keywords styled: {:?}",
            out[2]
        );

        // unterminated block comment swallows the rest of the block, no panic
        let mut out: Vec<TuiLine> = Vec::new();
        push_code_block(
            &mut out,
            &["/* never closed".to_string(), "let x = 1;".to_string()],
            "rust",
        );
        assert!(
            format!("{:?}", out[2]).contains(&format!("{:?}", palette::META)),
            "rest of the fence stays comment: {:?}",
            out[2]
        );
    }

    #[test]
    fn unknown_and_untagged_fences_stay_generic() {
        for tag in ["", "text", "output"] {
            assert!(code_lang(tag).is_none(), "{tag:?} must not highlight");
        }
        for (tag, lang) in [
            ("rust", CodeLang::Rust),
            ("rs", CodeLang::Rust),
            ("python", CodeLang::Python),
            ("py", CodeLang::Python),
            ("js", CodeLang::Js),
            ("ts", CodeLang::Js),
            ("javascript", CodeLang::Js),
            ("typescript", CodeLang::Js),
            ("json", CodeLang::Json),
            ("toml", CodeLang::Toml),
            ("bash", CodeLang::Shell),
            ("sh", CodeLang::Shell),
            ("shell", CodeLang::Shell),
        ] {
            assert_eq!(code_lang(tag), Some(lang), "{tag:?}");
        }
        // untagged fences keep the generic highlighter (keywords still styled)
        let mut out: Vec<TuiLine> = Vec::new();
        push_code_block(&mut out, &["let x = 1".to_string()], "");
        assert!(
            format!("{out:?}").contains(&format!("{:?}", palette::SYNTAX_KEYWORD)),
            "generic keyword: {out:?}"
        );
        // apostrophes glued to a word never open a string in rust fences
        let mut out: Vec<TuiLine> = Vec::new();
        push_code_block(&mut out, &["don't panic;".to_string()], "rust");
        assert!(
            !format!("{out:?}").contains(&format!("{:?}", palette::OK)),
            "no phantom string: {out:?}"
        );
    }
}
