//! TUI palette: black & gold — bright variant inspired by the "Bearded
//! Theme Black & Gold Soft" editor theme (BeardedBear/bearded-theme,
//! `black.ts`: base #221F1D, primary gold #C7910C), tuned for punch.
//! The ground is a neutral near-black with only a whisper of the
//! theme's warmth (brown lives in the chrome sand and the gold, not in
//! every surface), and the accents are lifted past the theme's own
//! values so they pop: bright gold for every highlight (titles, key
//! hints, the user ❯, the selection bar), hot orange warnings, mint
//! success, cyan-blue tooling, coral errors. Prose is near-white for a
//! 15.6:1 contrast on the canvas, bold stays weight-only, and surfaces
//! stay near-flat the way bearded does — panels recede below the
//! canvas, output cards lift a single step. Every text tier clears
//! 4.2:1 on the surface it sits on (syntax roles ≥ 4.5:1 on output
//! cards, the quiet tier ≥ 4.8:1 on panels); the app paints its own
//! canvas and surfaces, so it reads identically on any terminal theme.
use ratatui::style::{Color, Modifier, Style};

// ── the ten source colors (bright black & gold; kept as named sources
// for provenance) ─────────────────────────────────────────────────────
// The five pairs: prose ground, attention/deep, error/warm-faint,
// chrome/success, tooling/warning. Hue families are bearded's; every
// value was lifted for brightness on the neutral ground.
pub const CREAM: Color = Color::Rgb(237, 234, 228); // #EDEAE4 bright warm-white prose
pub const CHARCOAL: Color = Color::Rgb(20, 18, 16); // #141210 neutral near-black, whisper-warm ground
pub const BUTTER: Color = Color::Rgb(240, 180, 41); // #F0B429 bright gold: attention
pub const PETROL: Color = Color::Rgb(11, 94, 135); // #0B5E87 retired from surfaces; kept for provenance
pub const CORAL: Color = Color::Rgb(255, 107, 82); // #FF6B52 hot coral: errors
pub const UMBER: Color = Color::Rgb(154, 146, 132); // #9A9284 warm gray: quiet tier
pub const KHAKI: Color = Color::Rgb(198, 188, 168); // #C6BCA8 bright sand: chrome
pub const SAGE: Color = Color::Rgb(15, 208, 156); // #0FD09C bright mint: strings/success
pub const STEEL: Color = Color::Rgb(44, 199, 232); // #2CC7E8 bright cyan-blue: functions/tooling
pub const FLAME: Color = Color::Rgb(255, 160, 46); // #FFA02E bright orange: warn

// ── backgrounds: neutral near-black, near-flat — panels and modals
// recede below the canvas, output cards lift one step, and the
// user/tool bands keep only a hint of warmth ──
pub const BG: Color = CHARCOAL; // #141210 canvas
pub const BG_PANEL: Color = Color::Rgb(26, 24, 22); // #1A1816 input box, sidebar (recedes)
pub const BG_OUTPUT: Color = Color::Rgb(33, 30, 27); // #211E1B assistant cards
pub const BG_SURFACE: Color = Color::Rgb(29, 26, 24); // #1D1A18 modal/picker boxes
// user turns sit on a whisper-warm lift of the ground; the gold ❯
// lead marks the row
pub const BG_USER: Color = Color::Rgb(38, 34, 30); // #26221E
// tool activity rides its own stratum, one step below the canvas
pub const BG_TOOL: Color = Color::Rgb(27, 24, 21); // #1B1815
/// Tool band: `→ tool` text on the tool stratum.
pub const TOOL_BAND_STYLE: Style = Style::new().fg(TOOL).bg(BG_TOOL);

// ── text: bright warm-white ramp over the neutral ground ──────────
pub const FG: Color = CREAM; // #EDEAE4 primary prose
pub const FG_STRONG: Color = Color::Rgb(253, 252, 250); // #FDFCFA emphasis
pub const META: Color = KHAKI; // #C6BCA8 warm sand chrome
pub const FAINT: Color = UMBER; // #9A9284 quietest text tier (≥4.8:1 on panels)
pub const BORDER: Color = Color::Rgb(53, 48, 42); // #35302A block borders (subtle)
pub const BORDER_QUIET: Color = Color::Rgb(38, 34, 30); // #262220 transcript top border

// ── accents ──────────────────────────────────────────────────────
pub const ACCENT: Color = BUTTER; // titles, cursor, key hints, user ❯, spinner
pub const WARN: Color = FLAME; // #FFA02E
pub const OK: Color = SAGE; // #0FD09C success
pub const ERR: Color = CORAL; // #FF6B52 errors
pub const CHERRY: Color = Color::Rgb(255, 92, 122); // #FF5C7A raw danger
pub const TOOL: Color = STEEL; // #2CC7E8 → tool headers
pub const SEL_BG: Color = BUTTER; // #F0B429 gold selection bar (bearded cursor/suggest gold)
pub const SEL_FG: Color = Color::Rgb(26, 20, 10); // #1A140A near-black on the gold selection bar

// markdown roles
pub const HEADING: Color = ACCENT; // every heading level
pub const CODE_INLINE: Color = FLAME; // #FFA02E warm `code`
pub const CODE_BLOCK: Color = Color::Rgb(228, 224, 216); // #E4E0D8 bright-dim: fenced code base
// syntax palette in palette harmony; every role ≥ 4.5:1 on BG_OUTPUT
pub const SYNTAX_COMMENT: Color = META; // #C6BCA8
pub const SYNTAX_KEYWORD: Color = ACCENT; // #F0B429
pub const SYNTAX_STRING: Color = OK; // #0FD09C
pub const SYNTAX_NUMBER: Color = WARN; // #FFA02E

// ── derived styles ───────────────────────────────────────────────
pub const META_STYLE: Style = Style::new().fg(META); // footer, titles, hints
pub const BORDER_STYLE: Style = Style::new().fg(BORDER); // block borders
pub const BORDER_QUIET_STYLE: Style = Style::new().fg(BORDER_QUIET); // transcript border
pub const ACCENT_STYLE: Style = Style::new().fg(ACCENT);
pub const ACCENT_BOLD: Style = Style::new().fg(ACCENT).add_modifier(Modifier::BOLD);
pub const PLACEHOLDER: Style = Style::new().fg(META).add_modifier(Modifier::ITALIC);
// thinking sits a tier below normal chrome: warm gray, italic
pub const THOUGHT: Style = Style::new().fg(FAINT).add_modifier(Modifier::ITALIC);
pub const QUOTE: Style = Style::new().fg(META).add_modifier(Modifier::ITALIC);
/// Canvas fill: bright prose on the neutral black ground, painted over
/// the whole frame before layout so the app owns its look on any theme.
pub const CANVAS: Style = Style::new().fg(FG).bg(BG);

#[cfg(test)]
mod tests {
    use super::*;

    /// The ten source colors, byte-exact (bright black & gold,
    /// adapted from Bearded Black & Gold Soft).
    #[test]
    fn source_colors_match_user_palette() {
        for (got, want, name) in [
            (CREAM, (0xED, 0xEA, 0xE4), "cream"),
            (CHARCOAL, (0x14, 0x12, 0x10), "charcoal"),
            (BUTTER, (0xF0, 0xB4, 0x29), "butter"),
            (PETROL, (0x0B, 0x5E, 0x87), "petrol"),
            (CORAL, (0xFF, 0x6B, 0x52), "coral"),
            (UMBER, (0x9A, 0x92, 0x84), "umber"),
            (KHAKI, (0xC6, 0xBC, 0xA8), "khaki"),
            (SAGE, (0x0F, 0xD0, 0x9C), "sage"),
            (STEEL, (0x2C, 0xC7, 0xE8), "steel"),
            (FLAME, (0xFF, 0xA0, 0x2E), "flame"),
        ] {
            assert_eq!(got, Color::Rgb(want.0, want.1, want.2), "{name}");
        }
    }
}
