//! TUI palette: sunset night, rotated — the hues slide a step down the
//! dusk ramp (peach-gold accent, salmon warnings, teal-mint strings,
//! periwinkle tools, crimson errors, hot magenta keywords, pink
//! selection) over a deeper indigo ground. The ground carries almost
//! no chroma so the saturated accents read as light sources rather
//! than glare; user turns stay the one warm stratum (dark red-gold)
//! against violet-tinted assistant cards. Every text tier clears 4.2:1
//! on the surface it actually sits on (syntax roles ≥ 4.5:1 on output
//! cards, the quiet tier ≥ 4.8:1 on panels); the app paints its own
//! canvas and surfaces, so it reads identically on any terminal theme.
use ratatui::style::{Color, Modifier, Style};

// ── the ten source colors (sunset night, rotated; kept as named
// sources for provenance) plus violet for keywords ────────────────────
// The five pairs: prose ground, attention/deep, error/warm-faint,
// chrome/success, tooling/warning. Hue families follow the dusk ramp
// with a magenta-violet lean: peach-gold, salmon, crimson, mauve,
// periwinkle, teal-mint.
pub const CREAM: Color = Color::Rgb(237, 231, 244); // #EDE7F4 dusk lavender-white prose
pub const CHARCOAL: Color = Color::Rgb(15, 12, 22); // #0F0C16 deep indigo night ground
pub const BUTTER: Color = Color::Rgb(255, 192, 105); // #FFC069 peach-gold: attention
pub const PETROL: Color = Color::Rgb(11, 94, 135); // #0B5E87 retired from surfaces; kept for provenance
pub const CORAL: Color = Color::Rgb(255, 83, 112); // #FF5370 crimson-pink: errors
pub const UMBER: Color = Color::Rgb(143, 130, 166); // #8F82A6 mauve-gray: quiet tier
pub const KHAKI: Color = Color::Rgb(185, 168, 206); // #B9A8CE mauve chrome
pub const SAGE: Color = Color::Rgb(35, 213, 171); // #23D5AB teal-mint: strings/success
pub const STEEL: Color = Color::Rgb(122, 162, 247); // #7AA2F7 periwinkle: functions/tooling
pub const FLAME: Color = Color::Rgb(255, 138, 92); // #FF8A5C salmon-orange: warn
/// Hot magenta: keywords (complementary to the teal strings).
pub const MAGENTA: Color = Color::Rgb(255, 122, 198); // #FF7AC6

// ── backgrounds: deeper indigo night, near-flat — panels and modals
// recede below the canvas, output cards lift one violet step, user
// turns are the one warm (dark red-gold) stratum, tools ride deep
// violet ──
pub const BG: Color = CHARCOAL; // #0F0C16 canvas
pub const BG_PANEL: Color = Color::Rgb(11, 8, 18); // #0B0812 input box, sidebar (recedes)
pub const BG_OUTPUT: Color = Color::Rgb(24, 17, 37); // #181125 assistant cards
pub const BG_SURFACE: Color = Color::Rgb(18, 14, 28); // #120E1C modal/picker boxes
// user turns are the warm stratum: dark red-gold, the only warm
// surface, against the indigo assistant cards; the peach ❯ lead marks
// the row
pub const BG_USER: Color = Color::Rgb(51, 32, 22); // #332016
// tool activity rides deep violet, one step below the canvas
pub const BG_TOOL: Color = Color::Rgb(23, 16, 41); // #171029
/// Tool band: `→ tool` text on the violet stratum.
pub const TOOL_BAND_STYLE: Style = Style::new().fg(TOOL).bg(BG_TOOL);

// ── text: dusk lavender-white ramp over the indigo ground ─────────
pub const FG: Color = CREAM; // #EDE7F4 primary prose
pub const FG_STRONG: Color = Color::Rgb(252, 249, 255); // #FCF9FF emphasis
pub const META: Color = KHAKI; // #B9A8CE mauve chrome
pub const FAINT: Color = UMBER; // #8F82A6 quietest text tier (≥4.8:1 on panels)
pub const BORDER: Color = Color::Rgb(46, 39, 64); // #2E2740 block borders
pub const BORDER_QUIET: Color = Color::Rgb(34, 27, 49); // #221B31 transcript top border

// ── accents ──────────────────────────────────────────────────────
pub const ACCENT: Color = BUTTER; // titles, cursor, key hints, user ❯, spinner
pub const WARN: Color = FLAME; // #FF8A5C
pub const OK: Color = SAGE; // #23D5AB success
pub const ERR: Color = CORAL; // #FF5370 errors
pub const CHERRY: Color = Color::Rgb(255, 61, 109); // #FF3D6D hot pink: raw danger
pub const TOOL: Color = STEEL; // #7AA2F7 → tool headers
pub const SEL_BG: Color = Color::Rgb(220, 117, 181); // #DC75B5 pink selection bar
pub const SEL_FG: Color = Color::Rgb(27, 14, 24); // #1B0E18 near-black on the pink bar

// markdown roles
pub const HEADING: Color = ACCENT; // every heading level
pub const CODE_INLINE: Color = FLAME; // #FF8A5C warm `code`
pub const CODE_BLOCK: Color = Color::Rgb(220, 213, 234); // #DCD5EA lavender-dim: fenced code base
// syntax palette in palette harmony; every role ≥ 4.5:1 on BG_OUTPUT
pub const SYNTAX_COMMENT: Color = META; // #B9A8CE
pub const SYNTAX_KEYWORD: Color = MAGENTA; // #FF7AC6
pub const SYNTAX_STRING: Color = OK; // #23D5AB
pub const SYNTAX_NUMBER: Color = WARN; // #FF8A5C

// ── derived styles ───────────────────────────────────────────────
pub const META_STYLE: Style = Style::new().fg(META); // footer, titles, hints
pub const BORDER_STYLE: Style = Style::new().fg(BORDER); // block borders
pub const BORDER_QUIET_STYLE: Style = Style::new().fg(BORDER_QUIET); // transcript border
pub const ACCENT_STYLE: Style = Style::new().fg(ACCENT);
pub const ACCENT_BOLD: Style = Style::new().fg(ACCENT).add_modifier(Modifier::BOLD);
pub const PLACEHOLDER: Style = Style::new().fg(META).add_modifier(Modifier::ITALIC);
// thinking sits a tier below normal chrome: mauve, italic
pub const THOUGHT: Style = Style::new().fg(FAINT).add_modifier(Modifier::ITALIC);
pub const QUOTE: Style = Style::new().fg(META).add_modifier(Modifier::ITALIC);
/// Canvas fill: dusk lavender-white prose on the indigo night ground,
/// painted over the whole frame before layout so the app owns its look
/// on any theme.
pub const CANVAS: Style = Style::new().fg(FG).bg(BG);

#[cfg(test)]
mod tests {
    use super::*;

    /// The ten source colors + violet, byte-exact (sunset night,
    /// rotated).
    #[test]
    fn source_colors_match_user_palette() {
        for (got, want, name) in [
            (CREAM, (0xED, 0xE7, 0xF4), "cream"),
            (CHARCOAL, (0x0F, 0x0C, 0x16), "charcoal"),
            (BUTTER, (0xFF, 0xC0, 0x69), "butter"),
            (PETROL, (0x0B, 0x5E, 0x87), "petrol"),
            (CORAL, (0xFF, 0x53, 0x70), "coral"),
            (UMBER, (0x8F, 0x82, 0xA6), "umber"),
            (KHAKI, (0xB9, 0xA8, 0xCE), "khaki"),
            (SAGE, (0x23, 0xD5, 0xAB), "sage"),
            (STEEL, (0x7A, 0xA2, 0xF7), "steel"),
            (FLAME, (0xFF, 0x8A, 0x5C), "flame"),
            (MAGENTA, (0xFF, 0x7A, 0xC6), "magenta"),
        ] {
            assert_eq!(got, Color::Rgb(want.0, want.1, want.2), "{name}");
        }
    }
}
