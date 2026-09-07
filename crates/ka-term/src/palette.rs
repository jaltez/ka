//! TUI palette: black & gold, adapted from the "Bearded Theme Black &
//! Gold Soft" editor theme (BeardedBear/bearded-theme, `black.ts`:
//! base #221F1D, primary gold #C7910C, with its orange/green/blue/red
//! support hues). Warm near-black surfaces carry warm-white prose at
//! 15:1, every accent and text tier clears 4.2:1 on the surface it
//! actually sits on (syntax roles ≥ 4.5:1 on output cards), the gold
//! primary reads soft rather than neon, and the navy strata stay on as
//! the deep complement of the gold. The
//! app paints its own canvas and surfaces, so it reads identically on
//! any terminal theme.
use ratatui::style::{Color, Modifier, Style};

// ── the ten source colors (adapted from Bearded Black & Gold Soft;
// kept as named sources for provenance) ──────────────────────────────
// The five pairs: prose ground, attention/deep, error/warm-faint,
// chrome/success, tooling/warning. Gold, orange, green, red and blue
// are the theme's own hues; umber and charcoal were lifted until every
// text tier cleared its contrast floor on the new ground.
pub const CREAM: Color = Color::Rgb(251, 247, 238); // #FBF7EE prose ground
pub const CHARCOAL: Color = Color::Rgb(34, 31, 29); // #221F1D bearded soft base: warm near-black ground
pub const BUTTER: Color = Color::Rgb(199, 145, 12); // #C7910C bearded gold: attention (soft)
pub const PETROL: Color = Color::Rgb(11, 94, 135); // #0B5E87 deep complement of gold
pub const CORAL: Color = Color::Rgb(227, 85, 53); // #E35535 bearded red: errors
pub const UMBER: Color = Color::Rgb(162, 147, 119); // #A29377 lifted warm gray: quiet tier
pub const KHAKI: Color = Color::Rgb(214, 197, 159); // #D6C59F chrome (warm sand)
pub const SAGE: Color = Color::Rgb(0, 168, 132); // #00A884 bearded green: strings/success
pub const STEEL: Color = Color::Rgb(17, 183, 212); // #11B7D4 bearded blue: functions/tooling
pub const FLAME: Color = Color::Rgb(221, 129, 16); // #DD8110 bearded orange lifted: warn

// ── backgrounds: charcoal lifted into a four-step warm espresso
// ladder (never flat gray — each step warms toward cream); petrol
// strata mark user rows and tool bands ──
pub const BG: Color = CHARCOAL; // #221F1D canvas
pub const BG_PANEL: Color = Color::Rgb(40, 36, 32); // #282420 input box, sidebar
pub const BG_OUTPUT: Color = Color::Rgb(46, 42, 38); // #2E2A26 assistant cards
pub const BG_SURFACE: Color = Color::Rgb(53, 48, 43); // #35302B modal/picker boxes
// user rows ride the petrol complement of the gold ❯ lead
pub const BG_USER: Color = PETROL; // #0B5E87
// tool activity rides petrol darkened toward charcoal: its own stratum
pub const BG_TOOL: Color = Color::Rgb(10, 49, 69); // #0A3145
/// Tool band: `→ tool` text on the darkened petrol stratum.
pub const TOOL_BAND_STYLE: Style = Style::new().fg(TOOL).bg(BG_TOOL);

// ── text: cream ramp over the espresso ground ────────────────────
pub const FG: Color = CREAM; // #FBF7EE primary prose
pub const FG_STRONG: Color = Color::Rgb(255, 252, 245); // #FFFCF5 emphasis
pub const META: Color = KHAKI; // #D6C59F chrome
pub const FAINT: Color = UMBER; // #A29377 quietest text tier (≥4.8:1 on panels)
pub const BORDER: Color = Color::Rgb(70, 60, 51); // #463C33 block borders
pub const BORDER_QUIET: Color = Color::Rgb(58, 50, 43); // #3A322B transcript top border

// ── accents ──────────────────────────────────────────────────────
pub const ACCENT: Color = BUTTER; // titles, cursor, key hints, user ❯, spinner
pub const WARN: Color = FLAME; // #DD8110
pub const OK: Color = SAGE; // #00A884 bearded green: success
pub const ERR: Color = CORAL; // #E35535 bearded red: errors
pub const CHERRY: Color = Color::Rgb(224, 79, 107); // #E04F6B bearded salmon lifted: raw danger
pub const TOOL: Color = STEEL; // #11B7D4 → tool headers
pub const SEL_BG: Color = BUTTER; // #C7910C gold selection bar (bearded cursor/suggest gold)
pub const SEL_FG: Color = Color::Rgb(29, 26, 23); // #1D1A17 near-black on the gold selection bar

// markdown roles
pub const HEADING: Color = ACCENT; // every heading level
pub const CODE_INLINE: Color = FLAME; // #DD8110 warm `code`
pub const CODE_BLOCK: Color = Color::Rgb(221, 211, 191); // #DDD3BF cream-dim: fenced code base
// syntax palette in palette harmony; every role ≥ 4.5:1 on BG_OUTPUT
pub const SYNTAX_COMMENT: Color = META; // #D6C59F
pub const SYNTAX_KEYWORD: Color = ACCENT; // #C7910C
pub const SYNTAX_STRING: Color = OK; // #00A884
pub const SYNTAX_NUMBER: Color = WARN; // #DD8110

// ── derived styles ───────────────────────────────────────────────
pub const META_STYLE: Style = Style::new().fg(META); // footer, titles, hints
pub const BORDER_STYLE: Style = Style::new().fg(BORDER); // block borders
pub const BORDER_QUIET_STYLE: Style = Style::new().fg(BORDER_QUIET); // transcript border
pub const ACCENT_STYLE: Style = Style::new().fg(ACCENT);
pub const ACCENT_BOLD: Style = Style::new().fg(ACCENT).add_modifier(Modifier::BOLD);
pub const PLACEHOLDER: Style = Style::new().fg(META).add_modifier(Modifier::ITALIC);
// thinking sits a tier below normal chrome: umber, italic
pub const THOUGHT: Style = Style::new().fg(FAINT).add_modifier(Modifier::ITALIC);
pub const QUOTE: Style = Style::new().fg(META).add_modifier(Modifier::ITALIC);
/// Canvas fill: cream prose on the espresso ground, painted over the
/// whole frame before layout so the app owns its look on any theme.
pub const CANVAS: Style = Style::new().fg(FG).bg(BG);

#[cfg(test)]
mod tests {
    use super::*;

    /// The ten user-provided complementary source colors, byte-exact
    /// (v2: lifted in lightness/chroma for contrast).
    #[test]
    fn source_colors_match_user_palette() {
        for (got, want, name) in [
            (CREAM, (0xFB, 0xF7, 0xEE), "cream"),
            (CHARCOAL, (0x22, 0x1F, 0x1D), "charcoal"),
            (BUTTER, (0xC7, 0x91, 0x0C), "butter"),
            (PETROL, (0x0B, 0x5E, 0x87), "petrol"),
            (CORAL, (0xE3, 0x55, 0x35), "coral"),
            (UMBER, (0xA2, 0x93, 0x77), "umber"),
            (KHAKI, (0xD6, 0xC5, 0x9F), "khaki"),
            (SAGE, (0x00, 0xA8, 0x84), "sage"),
            (STEEL, (0x11, 0xB7, 0xD4), "steel"),
            (FLAME, (0xDD, 0x81, 0x10), "flame"),
        ] {
            assert_eq!(got, Color::Rgb(want.0, want.1, want.2), "{name}");
        }
    }
}
