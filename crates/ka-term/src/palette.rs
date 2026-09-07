//! TUI palette: a cream-on-espresso complementary scheme grown from five
//! complementary pairs (light accent + its deep complement), tuned for
//! contrast and chroma: warm near-black surfaces lift cream prose to
//! 12+:1, every accent and text tier clears 4.2:1 on the surface it
//! actually sits on, and hues carry saturation instead of gray mud. The
//! app paints its own canvas and surfaces, so it reads identically on
//! any terminal theme.
use ratatui::style::{Color, Modifier, Style};

// ── the ten source colors (user palette, tuned; kept for provenance) ─
// The five pairs: prose ground, attention/deep, error/warm-faint,
// chrome/success, tooling/warning. Each was lifted in lightness or
// chroma from the user's original hexes so text tiers read crisply on
// the dark ground.
pub const CREAM: Color = Color::Rgb(251, 247, 238); // #FBF7EE prose ground
pub const CHARCOAL: Color = Color::Rgb(32, 29, 26); // #201D1A warm near-black ground
pub const BUTTER: Color = Color::Rgb(241, 206, 121); // #F1CE79 attention
pub const PETROL: Color = Color::Rgb(11, 94, 135); // #0B5E87 deep complement of butter
pub const CORAL: Color = Color::Rgb(240, 118, 107); // #F0766B errors
pub const UMBER: Color = Color::Rgb(156, 141, 114); // #9C8D72 lifted umber: quiet tier
pub const KHAKI: Color = Color::Rgb(201, 184, 148); // #C9B894 chrome
pub const SAGE: Color = Color::Rgb(156, 203, 158); // #9CCB9E complement of khaki: ok
pub const STEEL: Color = Color::Rgb(93, 155, 180); // #5D9BB4 tooling
pub const FLAME: Color = Color::Rgb(224, 132, 47); // #E0842F complement of steel: warn

// ── backgrounds: charcoal lifted into a four-step warm espresso
// ladder (never flat gray — each step warms toward cream); petrol
// strata mark user rows and tool bands ──
pub const BG: Color = CHARCOAL; // #201D1A canvas
pub const BG_PANEL: Color = Color::Rgb(38, 35, 31); // #26231F input box, sidebar
pub const BG_OUTPUT: Color = Color::Rgb(44, 41, 37); // #2C2925 assistant cards
pub const BG_SURFACE: Color = Color::Rgb(51, 48, 43); // #33302B modal/picker boxes
// user rows ride the petrol complement of the butter ❯ lead
pub const BG_USER: Color = PETROL; // #0B5E87
// tool activity rides petrol darkened toward charcoal: its own stratum
pub const BG_TOOL: Color = Color::Rgb(10, 49, 69); // #0A3145
/// Tool band: `→ tool` text on the darkened petrol stratum.
pub const TOOL_BAND_STYLE: Style = Style::new().fg(TOOL).bg(BG_TOOL);

// ── text: cream ramp over the espresso ground ────────────────────
pub const FG: Color = CREAM; // #FBF7EE primary prose
pub const FG_STRONG: Color = Color::Rgb(255, 252, 245); // #FFFCF5 emphasis
pub const META: Color = KHAKI; // #C9B894 chrome
pub const FAINT: Color = UMBER; // #9C8D72 quietest text tier (≥4.8:1 on panels)
pub const BORDER: Color = Color::Rgb(70, 61, 48); // #463D30 block borders
pub const BORDER_QUIET: Color = Color::Rgb(57, 50, 41); // #393229 transcript top border

// ── accents ──────────────────────────────────────────────────────
pub const ACCENT: Color = BUTTER; // titles, cursor, key hints, user ❯, spinner
pub const WARN: Color = FLAME; // #E0842F
pub const OK: Color = SAGE; // #9CCB9E complement of khaki: success
pub const ERR: Color = CORAL; // #F0766B bright coral: errors
pub const CHERRY: Color = Color::Rgb(207, 91, 80); // #CF5B50 coral deepened: raw danger
pub const TOOL: Color = STEEL; // #5D9BB4 → tool headers
pub const SEL_BG: Color = PETROL; // #0B5E87 selection bar
pub const SEL_FG: Color = CREAM; // #FBF7EE selection text

// markdown roles
pub const HEADING: Color = ACCENT; // every heading level
pub const CODE_INLINE: Color = FLAME; // #E0842F warm `code`
pub const CODE_BLOCK: Color = Color::Rgb(221, 211, 191); // #DDD3BF cream-dim: fenced code base
// syntax palette in palette harmony; every role ≥ 4.5:1 on BG_OUTPUT
pub const SYNTAX_COMMENT: Color = META; // #C9B894
pub const SYNTAX_KEYWORD: Color = ACCENT; // #F1CE79
pub const SYNTAX_STRING: Color = OK; // #9CCB9E
pub const SYNTAX_NUMBER: Color = WARN; // #E0842F

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
            (CHARCOAL, (0x20, 0x1D, 0x1A), "charcoal"),
            (BUTTER, (0xF1, 0xCE, 0x79), "butter"),
            (PETROL, (0x0B, 0x5E, 0x87), "petrol"),
            (CORAL, (0xF0, 0x76, 0x6B), "coral"),
            (UMBER, (0x9C, 0x8D, 0x72), "umber"),
            (KHAKI, (0xC9, 0xB8, 0x94), "khaki"),
            (SAGE, (0x9C, 0xCB, 0x9E), "sage"),
            (STEEL, (0x5D, 0x9B, 0xB4), "steel"),
            (FLAME, (0xE0, 0x84, 0x2F), "flame"),
        ] {
            assert_eq!(got, Color::Rgb(want.0, want.1, want.2), "{name}");
        }
    }
}
