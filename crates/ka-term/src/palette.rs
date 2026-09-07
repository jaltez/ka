//! TUI palette: a cream-on-charcoal complementary scheme grown from five
//! user-provided complementary pairs (light accent + its dark
//! complement). The app paints its own canvas and surfaces, so it reads
//! identically on any terminal theme; accents carry semantics (butter
//! attention on petrol, steel tooling, coral errors, flame warnings,
//! sage success).
use ratatui::style::{Color, Modifier, Style};

// ── the ten source colors (user palette, kept for provenance) ────
pub const CREAM: Color = Color::Rgb(246, 240, 224); // #F6F0E0 prose ground
pub const CHARCOAL: Color = Color::Rgb(45, 45, 47); // #2D2D2F canvas ground
pub const BUTTER: Color = Color::Rgb(245, 225, 162); // #F5E1A2 attention
pub const PETROL: Color = Color::Rgb(2, 83, 113); // #025371 deep complement of butter
pub const CORAL: Color = Color::Rgb(239, 114, 102); // #EF7266 errors
pub const UMBER: Color = Color::Rgb(109, 89, 59); // #6D593B deep complement of coral
pub const KHAKI: Color = Color::Rgb(155, 145, 117); // #9B9175 chrome
pub const SAGE: Color = Color::Rgb(170, 194, 168); // #AAC2A8 complement of khaki: ok
pub const STEEL: Color = Color::Rgb(89, 137, 157); // #59899D tooling
pub const FLAME: Color = Color::Rgb(198, 85, 41); // #C65529 complement of steel: warn

// ── backgrounds: charcoal lifted into a four-step neutral surface
// ladder; cream rides the top, petrol strata mark user and tool bands ──
pub const BG: Color = CHARCOAL; // #2D2D2F canvas
pub const BG_PANEL: Color = Color::Rgb(52, 52, 56); // #343438 input box, sidebar
pub const BG_OUTPUT: Color = Color::Rgb(58, 58, 63); // #3A3A3F assistant cards
pub const BG_SURFACE: Color = Color::Rgb(65, 65, 71); // #414147 modal/picker boxes
// user rows ride the petrol complement of the butter ❯ lead
pub const BG_USER: Color = PETROL; // #025371
// tool activity rides petrol darkened toward charcoal: its own stratum
pub const BG_TOOL: Color = Color::Rgb(11, 44, 59); // #0B2C3B
/// Tool band: `→ tool` text on the darkened petrol stratum.
pub const TOOL_BAND_STYLE: Style = Style::new().fg(TOOL).bg(BG_TOOL);

// ── text: cream ramp over the charcoal ground ────────────────────
pub const FG: Color = CREAM; // #F6F0E0 primary prose
pub const FG_STRONG: Color = Color::Rgb(251, 247, 238); // #FBF7EE emphasis
pub const META: Color = KHAKI; // #9B9175 muted chrome
pub const FAINT: Color = UMBER; // #6D593B quietest text tier
pub const BORDER: Color = Color::Rgb(76, 76, 83); // #4C4C53 block borders
pub const BORDER_QUIET: Color = Color::Rgb(58, 58, 64); // #3A3A40 transcript top border

// ── accents ──────────────────────────────────────────────────────
pub const ACCENT: Color = BUTTER; // titles, cursor, key hints, user ❯, spinner
pub const WARN: Color = FLAME; // #C65529
pub const OK: Color = SAGE; // #AAC2A8 complement of khaki: success
pub const ERR: Color = CORAL; // #EF7266 bright coral: errors
pub const CHERRY: Color = Color::Rgb(179, 86, 77); // #B3564D coral deepened: raw danger
pub const TOOL: Color = STEEL; // #59899D → tool headers
pub const SEL_BG: Color = PETROL; // #025371 selection bar
pub const SEL_FG: Color = CREAM; // #F6F0E0 selection text

// markdown roles
pub const HEADING: Color = ACCENT; // every heading level
pub const CODE_INLINE: Color = FLAME; // #C65529 warm `code`
pub const CODE_BLOCK: Color = Color::Rgb(223, 216, 197); // #DFD8C5 cream-dim: fenced code base
// syntax palette in palette harmony; every role ≥ 4:1 on BG_OUTPUT
pub const SYNTAX_COMMENT: Color = META; // #9B9175
pub const SYNTAX_KEYWORD: Color = ACCENT; // #F5E1A2
pub const SYNTAX_STRING: Color = OK; // #AAC2A8
pub const SYNTAX_NUMBER: Color = WARN; // #C65529

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
/// Canvas fill: cream prose on the charcoal ground, painted over the
/// whole frame before layout so the app owns its look on any theme.
pub const CANVAS: Style = Style::new().fg(FG).bg(BG);

#[cfg(test)]
mod tests {
    use super::*;

    /// The ten user-provided complementary source colors, byte-exact.
    #[test]
    fn source_colors_match_user_palette() {
        for (got, want, name) in [
            (CREAM, (0xF6, 0xF0, 0xE0), "cream"),
            (CHARCOAL, (0x2D, 0x2D, 0x2F), "charcoal"),
            (BUTTER, (0xF5, 0xE1, 0xA2), "butter"),
            (PETROL, (0x02, 0x53, 0x71), "petrol"),
            (CORAL, (0xEF, 0x72, 0x66), "coral"),
            (UMBER, (0x6D, 0x59, 0x3B), "umber"),
            (KHAKI, (0x9B, 0x91, 0x75), "khaki"),
            (SAGE, (0xAA, 0xC2, 0xA8), "sage"),
            (STEEL, (0x59, 0x89, 0x9D), "steel"),
            (FLAME, (0xC6, 0x55, 0x29), "flame"),
        ] {
            assert_eq!(got, Color::Rgb(want.0, want.1, want.2), "{name}");
        }
    }
}
