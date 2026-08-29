//! TUI palette, mirroring the Oh My Pi dark theme. Text stays on the
//! terminal default; color is spent on structure and accents only.
use ratatui::style::{Color, Modifier, Style};

// core accents (OMP dark.json: accent / cyan / gray / dimGray / darkGray)
pub const ACCENT: Color = Color::Rgb(254, 188, 56); // #febc38 amber: bullets, cursor, spinner
pub const CYAN: Color = Color::Rgb(0, 136, 250); // #0088fa links
pub const MUTED: Color = Color::Rgb(119, 125, 136); // #777d88: quotes, chrome hints
pub const DIM: Color = Color::Rgb(95, 102, 115); // #5f6673: link urls
pub const BORDER_DIM: Color = Color::Rgb(61, 66, 74); // #3d424a: borders, rules, fences
pub const WARN: Color = Color::Rgb(228, 192, 15); // #e4c00f
pub const OK: Color = Color::Rgb(137, 210, 129); // #89d281
pub const ERR: Color = Color::Rgb(252, 58, 75); // #fc3a4b

// markdown roles
pub const HEADING: Color = Color::Rgb(254, 188, 56); // #febc38: every heading level
pub const CODE_INLINE: Color = Color::Rgb(229, 193, 255); // #e5c1ff: `code`
pub const CODE_BLOCK: Color = Color::Rgb(157, 205, 254); // #9cdcfe: fenced code base

// user messages are the only surface with a background (OMP userMsgBg)
pub const BG_USER: Color = Color::Rgb(34, 29, 26); // #221d1a warm dark

// derived styles
pub const META: Style = Style::new().fg(MUTED); // footer, titles, hints
pub const BORDER: Style = Style::new().fg(BORDER_DIM); // block borders
pub const ACCENT_STYLE: Style = Style::new().fg(ACCENT);
pub const ACCENT_BOLD: Style = Style::new().fg(ACCENT).add_modifier(Modifier::BOLD);
pub const PLACEHOLDER: Style = Style::new().fg(MUTED).add_modifier(Modifier::ITALIC);
pub const THOUGHT: Style = Style::new().fg(MUTED).add_modifier(Modifier::ITALIC); // streaming/final thoughts

// syntax palette (OMP dark.json syntax*)
pub const SYNTAX_COMMENT: Color = Color::Rgb(106, 153, 85); // #6a9955
pub const SYNTAX_KEYWORD: Color = Color::Rgb(86, 156, 214); // #569cd6
pub const SYNTAX_STRING: Color = Color::Rgb(206, 145, 120); // #ce9178
pub const SYNTAX_NUMBER: Color = Color::Rgb(181, 206, 168); // #b5cea8
