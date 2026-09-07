//! TUI palette: black & gold, adapted from the "Bearded Theme Black &
//! Gold Soft" editor theme (BeardedBear/bearded-theme, `black.ts`:
//! base #221F1D, primary gold #C7910C) — following its *usage*, not
//! just its hues. Surfaces are near-flat: panels and modals recede
//! below the canvas (bearded's `uibackgroundalt` darkens too), output
//! cards lift a step, nothing glows. Prose is the theme's soft warm
//! gray rather than bright cream, chrome is dim warm gray, and gold
//! carries every accent: titles, key hints, the user ❯, the selection
//! bar. Support hues are bearded's own: orange warnings, green
//! strings/success, blue functions, red errors. Bold is weight-only
//! over the soft ground, the way bearded treats emphasis. Every text
//! tier clears 4.2:1 on the surface it sits on (syntax roles ≥ 4.5:1
//! on output cards, the quiet tier ≥ 4.8:1 on panels); the app paints
//! its own canvas and surfaces, so it reads identically on any
//! terminal theme.
use ratatui::style::{Color, Modifier, Style};

// ── the ten source colors (adapted from Bearded Black & Gold Soft;
// kept as named sources for provenance) ──────────────────────────────
// The five pairs: prose ground, attention/deep, error/warm-faint,
// chrome/success, tooling/warning. Gold, orange, green, red and blue
// are the theme's own hues; prose, chrome and the quiet tier follow
// bearded's soft warm grays, lifted to clear ka's contrast floors.
pub const CREAM: Color = Color::Rgb(199, 193, 187); // #C7C1BB bearded soft gray-white prose
pub const CHARCOAL: Color = Color::Rgb(34, 31, 29); // #221F1D bearded soft base: warm near-black ground
pub const BUTTER: Color = Color::Rgb(199, 145, 12); // #C7910C bearded gold: attention (soft)
pub const PETROL: Color = Color::Rgb(11, 94, 135); // #0B5E87 retired from surfaces; kept for provenance
pub const CORAL: Color = Color::Rgb(227, 85, 53); // #E35535 bearded red: errors
pub const UMBER: Color = Color::Rgb(148, 139, 131); // #948B83 dim warm gray: quiet tier
pub const KHAKI: Color = Color::Rgb(173, 164, 159); // #ADA49F bearded chrome gray
pub const SAGE: Color = Color::Rgb(0, 168, 132); // #00A884 bearded green: strings/success
pub const STEEL: Color = Color::Rgb(17, 183, 212); // #11B7D4 bearded blue: functions/tooling
pub const FLAME: Color = Color::Rgb(221, 129, 16); // #DD8110 bearded orange lifted: warn

// ── backgrounds: bearded's near-flat ladder — the panel/modal
// surfaces recede BELOW the canvas (its `uibackgroundalt` darkens),
// output cards lift one warm step, and the user/tool bands are warm,
// not colored ──
pub const BG: Color = CHARCOAL; // #221F1D canvas
pub const BG_PANEL: Color = Color::Rgb(30, 27, 25); // #1E1B19 input box, sidebar (recedes)
pub const BG_OUTPUT: Color = Color::Rgb(39, 35, 32); // #272320 assistant cards
pub const BG_SURFACE: Color = Color::Rgb(33, 30, 28); // #211E1C modal/picker boxes
// user turns sit on a warm lift of the ground (bearded keeps bands
// monochrome; the gold ❯ lead marks the row)
pub const BG_USER: Color = Color::Rgb(42, 37, 33); // #2A2521
// tool activity rides its own warm stratum, one step below the canvas
pub const BG_TOOL: Color = Color::Rgb(36, 31, 27); // #241F1B
/// Tool band: `→ tool` text on the warm tool stratum.
pub const TOOL_BAND_STYLE: Style = Style::new().fg(TOOL).bg(BG_TOOL);

// ── text: soft gray-white ramp over the near-black ground ─────────
pub const FG: Color = CREAM; // #C7C1BB primary prose
pub const FG_STRONG: Color = Color::Rgb(239, 233, 225); // #EFE9E1 emphasis (warm white)
pub const META: Color = KHAKI; // #ADA49F bearded chrome gray
pub const FAINT: Color = UMBER; // #948B83 quietest text tier (≥4.8:1 on panels)
pub const BORDER: Color = Color::Rgb(58, 52, 46); // #3A342E block borders (subtle)
pub const BORDER_QUIET: Color = Color::Rgb(43, 39, 35); // #2B2723 transcript top border

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

    /// The ten source colors, byte-exact (adapted from Bearded Black
    /// & Gold Soft).
    #[test]
    fn source_colors_match_user_palette() {
        for (got, want, name) in [
            (CREAM, (0xC7, 0xC1, 0xBB), "cream"),
            (CHARCOAL, (0x22, 0x1F, 0x1D), "charcoal"),
            (BUTTER, (0xC7, 0x91, 0x0C), "butter"),
            (PETROL, (0x0B, 0x5E, 0x87), "petrol"),
            (CORAL, (0xE3, 0x55, 0x35), "coral"),
            (UMBER, (0x94, 0x8B, 0x83), "umber"),
            (KHAKI, (0xAD, 0xA4, 0x9F), "khaki"),
            (SAGE, (0x00, 0xA8, 0x84), "sage"),
            (STEEL, (0x11, 0xB7, 0xD4), "steel"),
            (FLAME, (0xDD, 0x81, 0x10), "flame"),
        ] {
            assert_eq!(got, Color::Rgb(want.0, want.1, want.2), "{name}");
        }
    }
}
