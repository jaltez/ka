//! Central dark-mode palette. Every TUI color decision lives here.
use ratatui::style::{Color, Modifier, Style};

pub const TEXT: Style = Style::new().fg(Color::Gray); // base body text
// fixed RGB (not the DarkGray palette slot): several terminal schemes map
// ANSI bright-black near the background, which rendered chrome invisible
pub const META: Style = Style::new().fg(Color::Rgb(120, 120, 128)); // footer, titles, hints, rules
pub const BORDER: Style = Style::new().fg(Color::Rgb(120, 120, 128)); // all block borders
pub const ACCENT: Style = Style::new().fg(Color::Cyan); // selection, filter, cursor, spinner
pub const ACCENT_BOLD: Style = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
pub const WARN: Style = Style::new().fg(Color::Yellow); // busy/attention
pub const OK: Style = Style::new().fg(Color::Green);
pub const ERR: Style = Style::new().fg(Color::LightRed);

// role labels (soft pastels) + user band fill
pub const LABEL_USER: Style = Style::new().fg(Color::LightBlue);
pub const BG_USER: Color = Color::Rgb(32, 44, 66); // subtle navy (was 30,54,96)
pub const LABEL_ASSISTANT: Style = Style::new()
    .fg(Color::LightCyan)
    .add_modifier(Modifier::BOLD);
pub const LABEL_THOUGHT: Style = Style::new().fg(Color::Gray).add_modifier(Modifier::ITALIC);
pub const LABEL_TOOL: Style = Style::new().fg(Color::LightYellow);
pub const LABEL_NOTE: Style = Style::new().fg(Color::LightRed);

// markdown surfaces
pub const BG_CODE: Color = Color::Rgb(28, 30, 36); // fenced code fill (kept)
pub const BG_CODE_INLINE: Color = Color::Rgb(48, 44, 30); // inline code fill (kept)

// syntax highlighting
pub const KEYWORD: Style = Style::new().fg(Color::LightMagenta);

pub const PLACEHOLDER: Style = Style::new()
    .fg(Color::Rgb(120, 120, 128))
    .add_modifier(Modifier::ITALIC);

// text emphasis: bold pops as near-white, italic shifts to a steel tint —
// both stay distinguishable on terminals where the font renders subtly
pub const STRONG: Style = Style::new()
    .fg(Color::Rgb(236, 238, 242))
    .add_modifier(Modifier::BOLD);
pub const EM: Style = Style::new()
    .fg(Color::Rgb(152, 186, 208))
    .add_modifier(Modifier::ITALIC);

// tinted surfaces for padded regions
pub const BG_QUOTE: Color = Color::Rgb(28, 32, 42); // blockquote band
pub const QUOTE: Style = Style::new()
    .fg(Color::Rgb(168, 178, 198))
    .add_modifier(Modifier::ITALIC);
pub const BG_INPUT: Color = Color::Rgb(20, 24, 32); // input box fill
pub const BG_ASSIST_LABEL: Color = Color::Rgb(22, 38, 44); // assistant chip
