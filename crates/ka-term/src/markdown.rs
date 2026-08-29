//! Markdown renderer for the TUI transcript (tables, inline styling,
//! code highlighting) plus a tiny generic syntax highlighter. The only
//! dependency is unicode-width, already in the tree via ratatui.

use crate::palette::{self, WARN};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line as TuiLine, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

fn code_bg() -> Style {
    Style::default().bg(palette::BG_CODE)
}

fn header_style(level: usize) -> Style {
    match level {
        1 => Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
        2 => palette::ACCENT_BOLD,
        _ => palette::ACCENT,
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
        if let Some(h) = trimmed.strip_prefix("### ") {
            out.push(TuiLine::styled(
                strip_atx_closer(h).to_string(),
                header_style(3),
            ));
        } else if let Some(h) = trimmed.strip_prefix("## ") {
            out.push(TuiLine::styled(
                strip_atx_closer(h).to_string(),
                header_style(2),
            ));
        } else if let Some(h) = trimmed.strip_prefix("# ") {
            out.push(TuiLine::styled(
                strip_atx_closer(h).to_string(),
                header_style(1),
            ));
        } else if trimmed.starts_with(">") {
            let q = trimmed.trim_start_matches('>').trim();
            out.push(TuiLine::styled(
                format!("│ {q}"),
                palette::META.add_modifier(Modifier::ITALIC),
            ));
        } else if trimmed.starts_with("- ") || trimmed.starts_with("* ") {
            let indent = list_indent(line, trimmed);
            let rest = &trimmed[2..];
            let mut spans = vec![Span::styled(format!("{indent}• "), WARN)];
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
            let mut spans = vec![Span::styled(format!("{indent}{num}. "), WARN)];
            spans.extend(inline_spans(rest));
            out.push(TuiLine::from(spans));
        } else if trimmed == "---" || trimmed == "***" {
            out.push(TuiLine::styled("─".repeat(40), palette::META));
        } else if let Some(level) = lines.get(i + 1).and_then(|n| setext_level(n)) {
            out.push(TuiLine::styled(trimmed.to_string(), header_style(level)));
            i += 1; // consume the underline
        } else {
            out.push(TuiLine::from(inline_spans(line)));
        }
        i += 1;
    }
    if in_code {
        // unterminated fence: flush what we have
        push_code_block(&mut out, &code_lines, &code_lang);
    }
    out
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
        Some((Span::styled("☐ ", palette::META), r))
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
/// header underline, and a rule hijacked into a bold h2 reads as a bug.
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
    let mut cells = vec![String::new()];
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

/// Render a parsed table as a box-drawn grid fitted to `width`. Column
/// order of width is preserved while shrinking; cells clip with `…`.
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
    let edge = Span::styled("│", palette::META);

    // header row (accent bold, no markdown inside)
    let mut spans = vec![edge.clone()];
    for (c, h) in header.iter().enumerate() {
        spans.push(Span::styled(" ", palette::META));
        let fitted: Vec<Span<'static>> = fit_spans(inline_spans(h), widths[c])
            .into_iter()
            .map(|mut s| {
                s.style = palette::ACCENT_BOLD;
                s
            })
            .collect();
        let used = spans_width(&fitted);
        spans.extend(fitted);
        if widths[c] > used {
            spans.push(Span::styled(
                " ".repeat(widths[c] - used),
                palette::ACCENT_BOLD,
            ));
        }
        spans.push(Span::styled(" ", palette::META));
        if c + 1 < cols {
            spans.push(Span::styled("│", palette::META));
        }
    }
    spans.push(edge.clone());
    lines.push(TuiLine::from(spans));

    // header/body rule
    let mut sep = String::from("├");
    for (c, w) in widths.iter().enumerate() {
        sep.push_str(&"─".repeat(w + 2));
        sep.push(if c + 1 == cols { '┤' } else { '┼' });
    }
    lines.push(TuiLine::styled(sep, palette::META));

    // body rows (inline markdown intact inside cells)
    for row in body {
        let mut spans = vec![edge.clone()];
        for (c, w) in widths.iter().enumerate() {
            spans.push(Span::styled(" ", palette::META));
            let cell = row.get(c).map(String::as_str).unwrap_or("");
            let fitted = fit_spans(inline_spans(cell), *w);
            let used = spans_width(&fitted);
            spans.extend(fitted);
            if *w > used {
                spans.push(Span::styled(" ".repeat(*w - used), Style::default()));
            }
            spans.push(Span::styled(" ", palette::META));
            if c + 1 < cols {
                spans.push(Span::styled("│", palette::META));
            }
        }
        spans.push(edge.clone());
        lines.push(TuiLine::from(spans));
    }
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
/// `[text](url)` links, `![alt](url)` images, `\`-escapes.
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
                spans.push(Span::styled(format!("🖼 {alt}"), palette::META));
                spans.push(Span::styled(format!(" ({url})"), palette::META));
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
                    palette::ACCENT.add_modifier(Modifier::UNDERLINED),
                ));
                spans.push(Span::styled(format!(" ({url})"), palette::META));
                i = next_i;
                continue;
            }
        }
        // strikethrough ~~text~~
        if c == '~' && chars.get(i + 1) == Some(&'~') {
            if let Some(end) = find_double(&chars, i + 2, '~') {
                flush(&mut plain, &mut spans);
                let struck: String = chars[i + 2..end].iter().collect();
                spans.push(Span::styled(
                    struck,
                    Style::default()
                        .add_modifier(Modifier::DIM)
                        .add_modifier(Modifier::CROSSED_OUT),
                ));
                i = end + 2;
                continue;
            }
        }
        if c == '`' {
            if let Some(end) = chars[i + 1..].iter().position(|&x| x == '`') {
                flush(&mut plain, &mut spans);
                let code: String = chars[i + 1..i + 1 + end].iter().collect();
                spans.push(Span::styled(
                    format!(" {code} "),
                    WARN.bg(palette::BG_CODE_INLINE),
                ));
                i += end + 2;
                continue;
            }
        }
        if c == '*' && i + 1 < chars.len() && chars[i + 1] == '*' {
            if let Some(end) = find_double(&chars, i + 2, '*') {
                flush(&mut plain, &mut spans);
                let bold: String = chars[i + 2..end].iter().collect();
                spans.push(Span::styled(
                    bold,
                    Style::default().add_modifier(Modifier::BOLD),
                ));
                i = end + 2;
                continue;
            }
        }
        if c == '*' && i + 1 < chars.len() && chars[i + 1] != ' ' {
            if let Some(end) = chars[i + 1..].iter().position(|&x| x == '*') {
                flush(&mut plain, &mut spans);
                let italic: String = chars[i + 1..i + 1 + end].iter().collect();
                spans.push(Span::styled(
                    italic,
                    Style::default().add_modifier(Modifier::ITALIC),
                ));
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
    let text: String = chars[start + 1..close_text].iter().collect();
    let url: String = chars[close_text + 2..close_url].iter().collect();
    if text.is_empty() || url.is_empty() {
        return None;
    }
    Some((text, url, close_url + 1))
}

fn push_code_block(out: &mut Vec<TuiLine<'static>>, lines: &[String], lang: &str) {
    if lines.is_empty() {
        return;
    }
    let label = if lang.is_empty() {
        String::new()
    } else {
        format!("╾─ {lang} ")
    };
    out.push(TuiLine::styled(
        format!("{label}{}", "─".repeat(12)),
        palette::META.bg(palette::BG_CODE),
    ));
    for line in lines {
        let mut spans = vec![Span::styled(" ", code_bg())];
        spans.extend(highlight(line));
        spans.push(Span::styled(" ", code_bg()));
        out.push(TuiLine::from(spans));
    }
    out.push(TuiLine::styled(
        "╾────────────",
        palette::META.bg(palette::BG_CODE),
    ));
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

/// Tiny generic highlighter: comments, strings, numbers, keywords.
pub fn highlight(line: &str) -> Vec<Span<'static>> {
    let bg = code_bg();
    if line.trim().is_empty() {
        return vec![Span::styled(String::new(), bg)];
    }
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut rest = line;
    'outer: loop {
        if rest.is_empty() {
            break;
        }
        // line comment anywhere
        for marker in ["//", "#"] {
            if let Some(pos) = rest.find(marker) {
                // quote check: comment inside a string? treat first string first
                let str_pos = rest.find(['"', '\'']);
                if str_pos.is_none_or(|sp| pos < sp) {
                    if pos > 0 {
                        highlight_plain(&rest[..pos], bg, &mut spans);
                    }
                    spans.push(Span::styled(
                        rest[pos..].to_string(),
                        palette::META.bg(palette::BG_CODE),
                    ));
                    return spans;
                }
            }
        }
        // string literal. An opening quote only starts a string when it is
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
                        highlight_plain(&rest[..quote_pos], bg, &mut spans);
                    }
                    spans.push(Span::styled(
                        rest[quote_pos..quote_pos + 1 + end + 1].to_string(),
                        palette::OK.bg(palette::BG_CODE),
                    ));
                    rest = &after[end + 1..];
                    continue 'outer;
                }
                _ => {
                    // mid-word apostrophe or unclosed quote: emit the prefix
                    // and the quote itself as plain code, then keep looking
                    if quote_pos > 0 {
                        highlight_plain(&rest[..quote_pos], bg, &mut spans);
                    }
                    spans.push(Span::styled(quote.to_string(), bg));
                    rest = &rest[quote_pos + 1..];
                    continue 'outer;
                }
            }
        }
        highlight_plain(rest, bg, &mut spans);
        break;
    }
    spans
}

fn highlight_plain(chunk: &str, bg: Style, spans: &mut Vec<Span<'static>>) {
    for token in chunk.split_inclusive(char::is_whitespace) {
        let bare = token.trim_end_matches(|c: char| !c.is_alphanumeric() && c != '_');
        if KEYWORDS.contains(&bare) {
            spans.push(Span::styled(
                token.to_string(),
                palette::KEYWORD.bg(palette::BG_CODE),
            ));
        } else if !bare.is_empty() && bare.chars().next().is_some_and(|c| c.is_ascii_digit()) {
            spans.push(Span::styled(token.to_string(), WARN.bg(palette::BG_CODE)));
        } else {
            spans.push(Span::styled(token.to_string(), bg));
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn renders_headers_and_lists() {
        let md = "# Title\n\n- item one\n- **bold** item\n\n1. first\n2. second\n";
        let lines = render(md, 80);
        assert_eq!(lines.len(), 7);
        assert!(format!("{:?}", lines[0]).contains("Title"));
    }

    #[test]
    fn code_block_gets_highlighted_spans() {
        let md = "```rust\nfn main() { let x = \"hi\"; } // done\n```\n";
        let lines = render(md, 80);
        assert!(lines.len() >= 3);
        let rendered = format!("{:?}", lines);
        assert!(rendered.contains("fn"), "code preserved");
        assert!(
            rendered.contains("green()") || rendered.contains("light_magenta()"),
            "colors applied"
        );
    }

    #[test]
    fn inline_code_and_bold() {
        let spans = inline_spans("run `cargo build` for **release** builds");
        let joined = format!("{spans:?}");
        assert!(joined.contains("cargo build"), "{joined}");
        assert!(joined.contains("release"), "{joined}");
        assert!(joined.contains("bold()"), "{joined}");
    }

    #[test]
    fn unterminated_fence_flushes() {
        let lines = render("```\nsome code", 80);
        assert!(format!("{lines:?}").contains("code"), "{lines:?}");
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
            texts[0].contains("Name") && texts[0].contains("Qty"),
            "{texts:?}"
        );
        assert!(
            texts[0].starts_with('│') && texts[0].ends_with('│'),
            "{:?}",
            texts[0]
        );
        assert!(texts[1].contains('┼') && texts[1].contains('├') && texts[1].contains('┤'));
        assert!(texts.iter().any(|t| t.contains("alpha")));
        assert!(texts.iter().any(|t| t.contains("12")));
        assert!(
            lines.last().is_some_and(|l| l.spans.is_empty()),
            "trailing blank"
        );
        for t in &texts {
            assert!(t.chars().count() <= 40, "row overflows: {t}");
        }
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
        assert!(format!("{h1:?}").contains("bold()"), "{h1:?}");
        // `---` after text is a horizontal rule, never a hijacked header
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
    fn apostrophes_do_not_open_code_strings() {
        // `don't` must not open a green string that swallows the line
        let spans = highlight("greet(\"hi\"); // don't panic");
        let joined = format!("{spans:?}");
        assert!(
            joined.contains("Rgb(120, 120, 128)"),
            "comment colored: {joined}"
        );
        // an unclosed quote must not color the rest of the line
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
        // header + 2 body rows: every bar-bearing row sits at identical columns
        assert!(bar_cols.len() >= 3, "{bar_cols:?}");
        assert!(bar_cols.windows(2).all(|w| w[0] == w[1]), "{bar_cols:?}");
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
    fn highlight_numbers_and_keywords() {
        let spans = highlight("let count = 42; // note");
        let joined = format!("{spans:?}");
        assert!(
            joined.contains("light_magenta()"),
            "keyword colored: {joined}"
        );
        assert!(joined.contains("yellow()"), "number colored: {joined}");
        assert!(
            joined.contains("Rgb(120, 120, 128)"),
            "comment colored: {joined}"
        );
    }
}
