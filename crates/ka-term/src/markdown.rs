//! Minimal markdown renderer for the TUI transcript, plus a tiny generic
//! syntax highlighter for fenced code blocks. No dependencies — the
//! footprint contract stays intact.

use crate::palette::{self, WARN};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line as TuiLine, Span};

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

/// Render markdown text into styled terminal lines.
pub fn render(text: &str) -> Vec<TuiLine<'static>> {
    let mut out: Vec<TuiLine<'static>> = Vec::new();
    let mut in_code = false;
    let mut code_lang = String::new();
    let mut code_lines: Vec<String> = Vec::new();

    for line in text.lines() {
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
            continue;
        }
        if in_code {
            code_lines.push(line.to_string());
            continue;
        }
        if trimmed.is_empty() {
            out.push(TuiLine::default());
            continue;
        }
        if let Some(h) = trimmed.strip_prefix("### ") {
            out.push(TuiLine::styled(h.to_string(), header_style(3)));
        } else if let Some(h) = trimmed.strip_prefix("## ") {
            out.push(TuiLine::styled(h.to_string(), header_style(2)));
        } else if let Some(h) = trimmed.strip_prefix("# ") {
            out.push(TuiLine::styled(h.to_string(), header_style(1)));
        } else if trimmed.starts_with(">") {
            let q = trimmed.trim_start_matches('>').trim();
            out.push(TuiLine::styled(
                format!("│ {q}"),
                palette::META.add_modifier(Modifier::ITALIC),
            ));
        } else if trimmed.starts_with("- ") || trimmed.starts_with("* ") {
            let rest = &trimmed[2..];
            let mut spans = vec![Span::styled("• ", WARN)];
            spans.extend(inline_spans(rest));
            out.push(TuiLine::from(spans));
        } else if is_numbered_item(trimmed) {
            let (num, rest) = trimmed.split_once(". ").unwrap_or(("1", trimmed));
            let mut spans = vec![Span::styled(format!("{num}. "), WARN)];
            spans.extend(inline_spans(rest));
            out.push(TuiLine::from(spans));
        } else if trimmed == "---" || trimmed == "***" {
            out.push(TuiLine::styled("─".repeat(40), palette::META));
        } else {
            out.push(TuiLine::from(inline_spans(line)));
        }
    }
    if in_code {
        // unterminated fence: flush what we have
        push_code_block(&mut out, &code_lines, &code_lang);
    }
    out
}

fn is_numbered_item(s: &str) -> bool {
    let digits = s.chars().take_while(|c| c.is_ascii_digit()).count();
    digits > 0 && s[digits..].starts_with(". ")
}

/// Inline markdown: `**bold**`, `*italic*`, `` `code` ``.
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
        // string literal
        if let Some(quote_pos) = rest.find(['"', '\'']) {
            let quote = rest.as_bytes()[quote_pos];
            if quote_pos > 0 {
                highlight_plain(&rest[..quote_pos], bg, &mut spans);
            }
            let after = &rest[quote_pos + 1..];
            match after.find(quote as char) {
                Some(end) => {
                    spans.push(Span::styled(
                        rest[quote_pos..quote_pos + 1 + end + 1].to_string(),
                        palette::OK.bg(palette::BG_CODE),
                    ));
                    rest = &after[end + 1..];
                    continue 'outer;
                }
                None => {
                    spans.push(Span::styled(
                        rest[quote_pos..].to_string(),
                        palette::OK.bg(palette::BG_CODE),
                    ));
                    break;
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
        let lines = render(md);
        assert_eq!(lines.len(), 7);
        assert!(format!("{:?}", lines[0]).contains("Title"));
    }

    #[test]
    fn code_block_gets_highlighted_spans() {
        let md = "```rust\nfn main() { let x = \"hi\"; } // done\n```\n";
        let lines = render(md);
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
        let lines = render("```\nsome code");
        assert!(format!("{lines:?}").contains("code"), "{lines:?}");
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
