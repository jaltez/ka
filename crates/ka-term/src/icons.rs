//! Every emoji/symbol glyph in one place — terminals without emoji
//! fonts degrade here, and only here. All emoji are emoji-presentation
//! (width 2 under unicode-width 0.2); the effort battery and ✕ are width 1.

use ka_protocol::Mode;

/// 🧠 thinking blocks + effort segment lead
pub const THOUGHT: &str = "\u{1f9e0}";
/// 🌿 git branch
pub const BRANCH: &str = "\u{1f33f}";
/// ✕ popup close chip
pub const CLOSE: &str = "\u{2715}";

/// 📋 strip todos button
pub const STRIP_TODOS: &str = "\u{1f4cb}";
/// ⚡ strip skills button
pub const STRIP_SKILLS: &str = "\u{26a1}";
/// 💡 strip info button
pub const STRIP_INFO: &str = "\u{1f4a1}";

pub fn mode_icon(mode: Mode) -> &'static str {
    match mode {
        Mode::Guarded => "\u{1f512}",     // 🔒
        Mode::AcceptEdits => "\u{1f4dd}", // 📝
        Mode::Free => "\u{1f680}",        // 🚀
        Mode::Plan => "\u{1f9ed}",        // 🧭
    }
}

pub fn mode_word(mode: Mode) -> &'static str {
    match mode {
        Mode::Guarded => "guarded",
        Mode::AcceptEdits => "accept edits",
        Mode::Free => "full access",
        Mode::Plan => "plan",
    }
}

/// Effort intensity battery — ordinal circle fill, width 1.
pub fn effort_glyph(level: &str) -> Option<&'static str> {
    match level {
        "off" => Some("\u{25cb}"),    // ○
        "low" => Some("\u{25d4}"),    // ◔
        "medium" => Some("\u{25d1}"), // ◑
        "high" => Some("\u{25d5}"),   // ◕
        "max" => Some("\u{25cf}"),    // ●
        _ => None,                    // unknown string: caller hides the segment
    }
}

/// Per-tool card icons; prefix match, first hit wins.
pub fn tool_icon(tool: &str) -> &'static str {
    if tool.starts_with("mcp.") {
        "\u{1f50c}" // 🔌
    } else if tool.starts_with("lsp_") {
        "\u{1f9f0}" // 🧰
    } else {
        match tool {
            "bash" => "\u{1f41a}",             // 🐚
            "read" => "\u{1f4d6}",             // 📖
            "grep" => "\u{1f50d}",             // 🔍
            "glob" => "\u{1f4c1}",             // 📁
            "edit" => "\u{1f527}",             // 🔧
            "write" => "\u{1f4c4}",            // 📄
            "web" => "\u{1f310}",              // 🌐
            "delegate" => "\u{1f6f0}\u{fe0f}", // 🛰️ (VS16: emoji presentation, width 2)
            "todo" => "\u{1f4cb}",             // 📋
            "remember" => "\u{1f4cc}",         // 📌
            "tasks" => "\u{1f9f5}",            // 🧵
            "debug" => "\u{1f41b}",            // 🐛
            "undo" => "\u{23ea}",              // ⏪
            _ => "\u{1f9e9}",                  // 🧩 fallback
        }
    }
}

/// Modal title icons (Model picker title is vendor-derived — no icon).
pub const MODAL_ICONS: &[(&str, &str)] = &[
    ("sessions", "\u{1f4dc}"),        // 📜
    ("settings", "\u{2699}\u{fe0f}"), // ⚙️
    ("providers", "\u{1f4e1}"),       // 📡
    ("api key", "\u{1f511}"),         // 🔑
    ("spills", "\u{1f4a7}"),          // 💧
    ("prompts", "\u{1f9fe}"),         // 🧾
    ("rewind", "\u{23ea}"),           // ⏪
    ("memory", "\u{1f4be}"),          // 💾
    ("usage", "\u{1f4ca}"),           // 📊
    ("context", "\u{1f5fa}\u{fe0f}"), // 🗺️
    ("tree", "\u{1f333}"),            // 🌳
    ("todos", "\u{1f4cb}"),           // 📋
    ("skills", "\u{26a1}"),           // ⚡
    ("info", "\u{1f4a1}"),            // 💡
    ("help", "\u{2753}"),             // ❓
];

/// 🤖 skills-popup section header
pub const AGENTS: &str = "\u{1f916}";

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

    fn assert_emoji(s: &str) {
        assert_eq!(UnicodeWidthStr::width(s), 2, "expected width 2: {s:?}");
        for c in s.chars() {
            assert!(UnicodeWidthChar::width(c).is_some(), "unprintable: {s:?}");
        }
    }

    #[test]
    fn emoji_constants_have_emoji_width_and_exact_strings() {
        let emojis: &[(&str, &str)] = &[
            ("THOUGHT", THOUGHT),
            ("BRANCH", BRANCH),
            ("STRIP_TODOS", STRIP_TODOS),
            ("STRIP_SKILLS", STRIP_SKILLS),
            ("STRIP_INFO", STRIP_INFO),
            ("AGENTS", AGENTS),
        ];
        for (name, s) in emojis {
            assert_emoji(s);
            assert!(
                s.chars()
                    .all(|c| (c as u32) >= 0x1f000 || matches!(c, '\u{26a1}')),
                "{name} must be an emoji-presentation glyph: {s:?}"
            );
        }
        assert_eq!(THOUGHT, "\u{1f9e0}");
        assert_eq!(BRANCH, "\u{1f33f}");
        assert_eq!(STRIP_TODOS, "\u{1f4cb}");
        assert_eq!(STRIP_SKILLS, "\u{26a1}");
        assert_eq!(STRIP_INFO, "\u{1f4a1}");
        assert_eq!(AGENTS, "\u{1f916}");
    }

    #[test]
    fn close_and_effort_battery_are_narrow() {
        assert_eq!(CLOSE, "\u{2715}");
        assert_eq!(UnicodeWidthStr::width(CLOSE), 1);
        assert_eq!(effort_glyph("off"), Some("\u{25cb}"));
        assert_eq!(effort_glyph("low"), Some("\u{25d4}"));
        assert_eq!(effort_glyph("medium"), Some("\u{25d1}"));
        assert_eq!(effort_glyph("high"), Some("\u{25d5}"));
        assert_eq!(effort_glyph("max"), Some("\u{25cf}"));
        assert_eq!(effort_glyph("nope"), None);
        for (level, g) in [
            ("off", "\u{25cb}"),
            ("low", "\u{25d4}"),
            ("medium", "\u{25d1}"),
            ("high", "\u{25d5}"),
            ("max", "\u{25cf}"),
        ] {
            assert_eq!(effort_glyph(level), Some(g));
            assert_eq!(
                UnicodeWidthStr::width(g),
                1,
                "battery must be width 1: {g:?}"
            );
        }
    }

    #[test]
    fn mode_icons_and_words_are_pinned() {
        let cases: &[(Mode, &str, &str)] = &[
            (Mode::Guarded, "\u{1f512}", "guarded"),
            (Mode::AcceptEdits, "\u{1f4dd}", "accept edits"),
            (Mode::Free, "\u{1f680}", "full access"),
            (Mode::Plan, "\u{1f9ed}", "plan"),
        ];
        for (mode, icon, word) in cases {
            assert_emoji(mode_icon(*mode));
            assert_eq!(mode_icon(*mode), *icon);
            assert_eq!(mode_word(*mode), *word);
        }
    }

    #[test]
    fn tool_icons_cover_table_and_prefixes() {
        let expected: &[(&str, &str)] = &[
            ("bash", "\u{1f41a}"),
            ("read", "\u{1f4d6}"),
            ("grep", "\u{1f50d}"),
            ("glob", "\u{1f4c1}"),
            ("edit", "\u{1f527}"),
            ("write", "\u{1f4c4}"),
            ("web", "\u{1f310}"),
            ("delegate", "\u{1f6f0}\u{fe0f}"),
            ("todo", "\u{1f4cb}"),
            ("remember", "\u{1f4cc}"),
            ("tasks", "\u{1f9f5}"),
            ("debug", "\u{1f41b}"),
            ("undo", "\u{23ea}"),
            ("whatever-unknown", "\u{1f9e9}"),
        ];
        for (tool, icon) in expected {
            assert_eq!(tool_icon(tool), *icon, "tool {tool}");
            assert_emoji(tool_icon(tool));
        }
        assert_eq!(tool_icon("mcp.github"), "\u{1f50c}");
        assert_eq!(tool_icon("lsp_hover"), "\u{1f9f0}");
        assert_emoji(tool_icon("mcp.x"));
        assert_emoji(tool_icon("lsp_y"));
    }

    #[test]
    fn modal_icon_table_is_pinned() {
        let expected: &[(&str, &str)] = &[
            ("sessions", "\u{1f4dc}"),
            ("settings", "\u{2699}\u{fe0f}"),
            ("providers", "\u{1f4e1}"),
            ("api key", "\u{1f511}"),
            ("spills", "\u{1f4a7}"),
            ("prompts", "\u{1f9fe}"),
            ("rewind", "\u{23ea}"),
            ("memory", "\u{1f4be}"),
            ("usage", "\u{1f4ca}"),
            ("context", "\u{1f5fa}\u{fe0f}"),
            ("tree", "\u{1f333}"),
            ("todos", "\u{1f4cb}"),
            ("skills", "\u{26a1}"),
            ("info", "\u{1f4a1}"),
            ("help", "\u{2753}"),
        ];
        assert_eq!(MODAL_ICONS.len(), expected.len());
        for ((name, icon), (ename, eicon)) in MODAL_ICONS.iter().zip(expected) {
            assert_eq!(name, ename);
            assert_eq!(icon, eicon);
            assert_eq!(UnicodeWidthStr::width(*icon), 2, "icon for {name}");
        }
    }
}
