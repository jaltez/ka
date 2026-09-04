//! TUI palette: a warm camel/mocha scheme grown from six user-provided
//! source colors. The app paints its own canvas and surfaces, so it reads
//! identically on any terminal theme; accents carry semantics (indigo
//! selection, cherry errors, camel attention).
use ratatui::style::{Color, Modifier, Style};

// ── the six source colors (user palette, kept for provenance) ────
pub const CAMEL: Color = Color::Rgb(199, 158, 101); // #C79E65
pub const REDDISH_BROWN: Color = Color::Rgb(135, 72, 54); // #874836
pub const DEEP_MOCHA: Color = Color::Rgb(76, 53, 43); // #4C352B
pub const STONE_BROWN: Color = Color::Rgb(101, 87, 71); // #655747
pub const TWILIGHT_INDIGO: Color = Color::Rgb(44, 53, 97); // #2C3561
pub const INTENSE_CHERRY: Color = Color::Rgb(192, 58, 66); // #C03A42

// ── backgrounds: deep mocha darkened into a four-step surface ladder ──
pub const BG: Color = Color::Rgb(23, 18, 16); // #171210 canvas
pub const BG_PANEL: Color = Color::Rgb(28, 22, 17); // #1C1611 input box, sidebar
pub const BG_OUTPUT: Color = Color::Rgb(32, 25, 19); // #201913 assistant cards
pub const BG_SURFACE: Color = Color::Rgb(36, 28, 22); // #241C16 modal/picker boxes
// user rows ride a twilight-indigo band lightened toward dusk
pub const BG_USER: Color = Color::Rgb(38, 43, 69); // #262B45
// tool activity rides twilight-indigo darkened: its own stratum
pub const BG_TOOL: Color = Color::Rgb(27, 30, 44); // #1B1E2C
/// Tool band: `→ tool` text on the darkened indigo stratum.
pub const TOOL_BAND_STYLE: Style = Style::new().fg(TOOL).bg(BG_TOOL);

// ── text: cream ramp over the warm ground ────────────────────────
pub const FG: Color = Color::Rgb(232, 220, 201); // #E8DCC9 primary prose
pub const FG_STRONG: Color = Color::Rgb(242, 233, 218); // #F2E9DA emphasis
pub const META: Color = Color::Rgb(165, 145, 123); // #A5917B stone-brown lifted: chrome
pub const FAINT: Color = Color::Rgb(107, 91, 74); // #6B5B4A quietest text tier
pub const BORDER: Color = Color::Rgb(85, 70, 58); // #55463A block borders
pub const BORDER_QUIET: Color = Color::Rgb(61, 49, 40); // #3D3128 transcript top border

// ── accents ──────────────────────────────────────────────────────
pub const ACCENT: Color = CAMEL; // titles, cursor, key hints, user ❯, spinner
pub const WARN: Color = Color::Rgb(217, 142, 74); // #D98E4A
pub const OK: Color = Color::Rgb(163, 179, 108); // #A3B36C harmonized sage
pub const ERR: Color = Color::Rgb(217, 86, 87); // #D95657 bright cherry: errors
pub const CHERRY: Color = INTENSE_CHERRY; // #C03A42 raw: strong danger
pub const TOOL: Color = Color::Rgb(122, 134, 194); // #7A86C2 twilight-indigo lifted: → tool headers
pub const SEL_BG: Color = TWILIGHT_INDIGO; // #2C3561 selection bar
pub const SEL_FG: Color = Color::Rgb(237, 227, 210); // #EDE3D2 selection text

// markdown roles
pub const HEADING: Color = ACCENT; // every heading level
pub const CODE_INLINE: Color = Color::Rgb(201, 123, 93); // #C97B5D reddish-brown lifted: `code`
pub const CODE_BLOCK: Color = Color::Rgb(222, 205, 178); // #DECDB2 parchment: fenced code base
// syntax palette in palette harmony; every role ≥ 4:1 on BG_OUTPUT
pub const SYNTAX_COMMENT: Color = META; // #A5917B
pub const SYNTAX_KEYWORD: Color = ACCENT; // #C79E65
pub const SYNTAX_STRING: Color = OK; // #A3B36C
pub const SYNTAX_NUMBER: Color = WARN; // #D98E4A

// ── derived styles ───────────────────────────────────────────────
pub const META_STYLE: Style = Style::new().fg(META); // footer, titles, hints
pub const BORDER_STYLE: Style = Style::new().fg(BORDER); // block borders
pub const BORDER_QUIET_STYLE: Style = Style::new().fg(BORDER_QUIET); // transcript border
pub const ACCENT_STYLE: Style = Style::new().fg(ACCENT);
pub const ACCENT_BOLD: Style = Style::new().fg(ACCENT).add_modifier(Modifier::BOLD);
pub const PLACEHOLDER: Style = Style::new().fg(META).add_modifier(Modifier::ITALIC);
// thinking sits a tier below normal chrome: faintest brown, italic
pub const THOUGHT: Style = Style::new().fg(FAINT).add_modifier(Modifier::ITALIC);
pub const QUOTE: Style = Style::new().fg(META).add_modifier(Modifier::ITALIC);
/// Canvas fill: cream prose on the warm mocha ground, painted over the
/// whole frame before layout so the app owns its look on any theme.
pub const CANVAS: Style = Style::new().fg(FG).bg(BG);

#[cfg(test)]
mod tests {
    use super::*;

    /// The six user-provided source colors, byte-exact.
    #[test]
    fn source_colors_match_user_palette() {
        for (got, want, name) in [
            (CAMEL, (0xC7, 0x9E, 0x65), "camel"),
            (REDDISH_BROWN, (0x87, 0x48, 0x36), "reddish-brown"),
            (DEEP_MOCHA, (0x4C, 0x35, 0x2B), "deep-mocha"),
            (STONE_BROWN, (0x65, 0x57, 0x47), "stone-brown"),
            (TWILIGHT_INDIGO, (0x2C, 0x35, 0x61), "twilight-indigo"),
            (INTENSE_CHERRY, (0xC0, 0x3A, 0x42), "intense-cherry"),
        ] {
            assert_eq!(got, Color::Rgb(want.0, want.1, want.2), "{name}");
        }
    }
}
