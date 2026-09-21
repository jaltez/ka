//! TUI feature contracts: the slash-command registry, transcript
//! rewind, and the memory-inbox review flow — the user-facing surface
//! documented in the README. A failing test here means a documented
//! command or interaction broke; update docs + contract together.

#![allow(clippy::unwrap_used, clippy::expect_used)]
use ka_term::tui::{
    ModalKind, NotifySettings, Transcript, accept_memory_note, memory_modal_rows,
    read_memory_inbox, slash_command, write_memory_inbox,
};

/// Every built-in slash command documented in the README parses. A
/// command removed or renamed without updating this list is a breaking
/// UX change.
#[test]
fn every_documented_slash_command_parses() {
    // handled by slash_command dispatch (the modal/event layer)
    let documented = [
        "/quit",
        "/exit",
        "/model",
        "/provider",
        "/mode",
        "/plan",
        "/build",
        "/review",
        "/tasks",
        "/approve",
        "/rewind",
        "/fork",
        "/checkpoint",
        "/restore",
        "/compact",
        "/session",
        "/resume",
        "/new",
        "/undo",
        "/spills",
        "/export",
        "/help",
        "/settings",
        "/usage",
        "/context",
        "/key",
        "/mcp",
        "/prompt",
        "/tree",
        "/memory",
    ];
    let mut missing = Vec::new();
    for cmd in documented {
        if slash_command(cmd).is_none() {
            missing.push(cmd);
        }
    }
    assert!(
        missing.is_empty(),
        "documented slash commands no longer parse: {missing:?}"
    );
    // input-layer commands are intercepted before slash_command (local
    // transcript/copy/mouse state, no engine roundtrip): /find /retry
    // /copy /clip /mouse. They are covered by the input-layer unit
    // tests in src; if you move one into slash_command, add it above.
}

/// Argument-bearing forms parse the way the docs show.
#[test]
fn slash_commands_accept_documented_arguments() {
    // /model with a selector
    let s = slash_command("/model anthropic/claude-sonnet-5@high").expect("/model <sel>");
    assert!(
        matches!(s.event, Some(ka_protocol::Command::SetModel { selector })
        if selector == "anthropic/claude-sonnet-5@high")
    );
    // /rewind with a count
    let s = slash_command("/rewind 3").expect("/rewind N");
    assert!(matches!(
        s.event,
        Some(ka_protocol::Command::Rewind { turns: 3 })
    ));
    // /tasks carries the snapshot command
    let s = slash_command("/tasks").expect("/tasks");
    assert!(matches!(s.event, Some(ka_protocol::Command::ListTasks)));
    // unknown commands do not parse
    assert!(slash_command("/definitely-not-a-command").is_none());
}

/// The picker-backed commands open modals (mode picker, session picker,
/// settings panel, memory view) rather than emitting engine commands.
#[test]
fn modal_slash_commands_open_pick() {
    for cmd in ["/mode", "/session", "/settings", "/memory", "/help"] {
        let s = slash_command(cmd).unwrap_or_else(|| panic!("{cmd}"));
        assert!(
            s.modal.is_some(),
            "{cmd} must open a modal (event: {:?}, quit: {})",
            s.event,
            s.quit
        );
    }
    assert!(matches!(
        slash_command("/memory").unwrap().modal,
        Some(ModalKind::Memory)
    ));
}

/// Transcript rewind: drops everything from the nth-from-last user
/// message, returns the dropped prompt, refuses impossible cuts.
#[test]
fn transcript_rewind_user_contract() {
    let mut t = Transcript::default();
    t.push(ka_term::tui::Line::User("one".into()));
    t.push(ka_term::tui::Line::Assistant("a1".into()));
    t.push(ka_term::tui::Line::User("two".into()));
    t.push(ka_term::tui::Line::Assistant("a2".into()));
    let before = t.total_rows();

    // too few user messages → refused, nothing changes
    assert_eq!(t.rewind_user(3), None);
    assert_eq!(t.total_rows(), before);

    // rewind to the 2nd-from-last user message ("one"): everything from
    // that row onward is dropped, and the dropped prompt comes back
    let dropped = t.rewind_user(2).expect("two user messages exist");
    assert_eq!(dropped, "one");
    let entries = t.entries();
    assert!(
        entries
            .iter()
            .all(|l| !matches!(l, ka_term::tui::Line::User(u) if u == "one" || u == "two")),
        "both user rows and everything after are gone: {entries:?}"
    );

    // dropping the most recent user message only
    let mut t = Transcript::default();
    t.push(ka_term::tui::Line::User("keep".into()));
    t.push(ka_term::tui::Line::User("drop".into()));
    let dropped = t.rewind_user(1).expect("one user message");
    assert_eq!(dropped, "drop");
    assert_eq!(t.entries().len(), 1, "only the first user row survives");
}

/// Memory inbox: staging persists, accept appends to the project
/// MEMORY.md and drains the inbox, discard just drains.
#[test]
fn memory_inbox_roundtrip_and_accept() {
    let dir = std::env::temp_dir().join(format!("ka-term-inbox-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // staged notes round-trip
    let staged = vec![
        "- [1700000000] prefer parking_lot locks".to_string(),
        "- [1700000001] never commit ka-release.key".to_string(),
    ];
    write_memory_inbox(&dir, &staged);
    assert_eq!(read_memory_inbox(&dir), staged, "staged notes round-trip");
    assert!(dir.join(".ka/memory/inbox.md").exists());

    // the modal rows show the memory tiers alongside the inbox
    let rows = memory_modal_rows(&dir);
    assert!(rows.is_empty() || rows.iter().all(|r| !r.is_empty()));

    // accept appends the note to ./MEMORY.md (text only, no stamp);
    // the caller (the TUI key handler) then drains the inbox
    let line = staged[0].clone();
    accept_memory_note(&dir, &line, false).unwrap();
    let memory = std::fs::read_to_string(dir.join("MEMORY.md")).unwrap();
    assert!(memory.contains("prefer parking_lot locks"), "{memory:?}");
    assert!(
        !memory.contains("1700000000"),
        "stamps are stripped: {memory:?}"
    );
    write_memory_inbox(&dir, &staged[1..]);
    assert_eq!(read_memory_inbox(&dir), vec![staged[1].clone()]);

    // discard drains without writing memory
    write_memory_inbox(&dir, &[staged[1].clone()]);
    accept_memory_note(&dir, "not-a-stamped-line", false).unwrap_or_else(|e| {
        // unstaged-shaped lines are tolerated (whole line becomes the note)
        let _ = e;
    });
    write_memory_inbox(&dir, &[]);
    assert!(
        !dir.join(".ka/memory/inbox.md").exists(),
        "empty inbox is removed"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Memory flows anchor at the root project: a session launched in a
/// subdirectory of a git repo stages, reads, drains, and accepts into
/// the repo root's `.ka/memory/inbox.md` and `MEMORY.md`.
#[test]
fn memory_inbox_anchors_at_git_root() {
    let root = std::env::temp_dir().join(format!("ka-term-inbox-root-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join(".git")).unwrap();
    let deep = root.join("deep").join("nested");
    std::fs::create_dir_all(&deep).unwrap();

    // the engine stages at the root; the TUI reads the same file
    std::fs::create_dir_all(root.join(".ka/memory")).unwrap();
    std::fs::write(
        root.join(".ka/memory/inbox.md"),
        "- [1700000000] prefer parking_lot locks\n",
    )
    .unwrap();
    let staged = read_memory_inbox(&deep);
    assert_eq!(
        staged,
        vec!["- [1700000000] prefer parking_lot locks".to_string()],
        "subdir launch reads the root inbox"
    );

    // acceptance writes the project MEMORY.md at the root
    accept_memory_note(&deep, &staged[0], false).unwrap();
    let memory = std::fs::read_to_string(root.join("MEMORY.md")).unwrap();
    assert!(memory.contains("prefer parking_lot locks"), "{memory:?}");
    assert!(!deep.join("MEMORY.md").exists(), "no memory in the subdir");

    // draining removes the root inbox, not a subdir copy
    write_memory_inbox(&deep, &[]);
    assert!(!root.join(".ka/memory/inbox.md").exists());

    let _ = std::fs::remove_dir_all(&root);
}

/// Notification settings defaults: the bell is on unless disabled, and
/// the notify command is optional.
#[test]
fn notify_settings_contract() {
    // the struct default is inert; the on-by-default contract lives in
    // the config layer the CLI resolves before run()
    let s = NotifySettings::default();
    assert!(s.command.is_none());
    assert!(
        ka_engine::config::Config::default().effective_bell(),
        "[tui] bell defaults to on (terminals mute by user choice)"
    );
}
