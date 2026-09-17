//! Feature contracts: the documented, user-facing behavior of ka's
//! engine, encoded as tests so future changes cannot silently drop a
//! feature — a failing test here means the change was intentional and
//! the contract (and docs) must be updated with it.
//!
//! Contracts covered: the tool registry, the permission gate matrix
//! (modes × rules × hardstops × protected paths × read ledger), hook
//! blocking + steering, the verify lint loop, the config schema
//! surface, model selectors, glob/rule matching semantics, and the
//! background-delegate task lifecycle. Pure end-to-end engine commands
//! live in `engine_e2e.rs`; safe mode in `bare_mode_contract.rs`.

#![allow(clippy::unwrap_used, clippy::expect_used)]
use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;

use ka_dialect::dialects::Catalog;
use ka_dialect::speaker::{SpeakFuture, SpeakRequest, Speaker, StreamEvent, ToolCall};
use ka_engine::hands::{self, Hand};
use ka_engine::voice::{GuardRuntime, Voice};
use ka_protocol::{Command, Event};

fn test_catalog() -> Catalog {
    // unreachable base_url: any accidental real-provider attempt fails fast
    Catalog::parse(
        "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000000\n",
    )
    .unwrap()
}

/// A speaker that emits one scripted tool call per request round, then
/// a final text reply (the FakeSpeaker contract from voice's own tests).
struct Scripted {
    calls: parking_lot::Mutex<VecDeque<(&'static str, serde_json::Value)>>,
}

impl Scripted {
    fn calls(calls: Vec<(&'static str, serde_json::Value)>) -> Arc<dyn Speaker> {
        Arc::new(Scripted {
            calls: parking_lot::Mutex::new(calls.into()),
        })
    }
}

impl Speaker for Scripted {
    fn speak<'a>(
        &'a self,
        _req: SpeakRequest,
        out: tokio::sync::mpsc::Sender<StreamEvent>,
    ) -> SpeakFuture<'a> {
        Box::pin(async move {
            // bind outside the if-let: edition-2024 temporaries would
            // otherwise hold the parking_lot guard across the awaits
            let next = self.calls.lock().pop_front();
            if let Some((tool, args)) = next {
                out.send(StreamEvent::Call(ToolCall {
                    id: "c1".into(),
                    tool: tool.into(),
                    arguments: args,
                }))
                .await
                .ok();
            } else {
                out.send(StreamEvent::Text("done".into())).await.ok();
            }
            out.send(StreamEvent::Finished {
                stop: ka_protocol::Stop::Done,
                usage: ka_protocol::Usage {
                    input: 1,
                    output: 1,
                    ..Default::default()
                },
            })
            .await
            .ok();
        })
    }
}

/// Drive one turn, auto-answering permission asks with `answer`
/// (1 = always, 2 = deny; None leaves asks unanswered — do not do that
/// in tests). Returns every event the turn emitted.
async fn run_turn(voice: &mut Voice, prompt: &str, answer: Option<usize>) -> Vec<Event> {
    // the engine points the snapshot journal at the strand on attach;
    // mirror that here so mutating hands behave as they do in a session
    voice.snapshot_sink().lock().set_strand("contract-test");
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel::<Command>(16);
    let (evt_tx, mut evt_rx) = tokio::sync::mpsc::channel::<Event>(256);
    let mut interjections = Vec::new();
    let mut deferrals = VecDeque::new();
    let mut guards = GuardRuntime::default();
    let fut = voice.turn(
        "test/m",
        prompt.to_string(),
        &mut cmd_rx,
        &evt_tx,
        &mut interjections,
        &mut deferrals,
        &mut guards,
        None,
        Vec::new(),
    );
    tokio::pin!(fut);
    let mut events = Vec::new();
    loop {
        tokio::select! {
            biased;
            evt = evt_rx.recv() => match evt {
                Some(e) => {
                    if let Event::Ask { id, .. } = &e {
                        if let Some(choice) = answer {
                            cmd_tx
                                .send(Command::Answer {
                                    question: id.clone(),
                                    choice,
                                })
                                .await
                                .ok();
                        }
                    }
                    events.push(e);
                }
                None => break,
            },
            _ = &mut fut => {
                // the turn finished; collect what is still buffered.
                // (A recv().await drain would deadlock: evt_tx lives in
                // this frame — turn only borrows it.)
                while let Ok(e) = evt_rx.try_recv() {
                    events.push(e);
                }
                break;
            }
        }
    }
    events
}

fn asks_of(events: &[Event]) -> Vec<&AskQuestion> {
    events
        .iter()
        .flat_map(|e| match e {
            Event::Ask { questions, .. } => questions.iter().collect::<Vec<_>>(),
            _ => Vec::new(),
        })
        .collect()
}

use ka_protocol::AskQuestion;

fn outputs_of<'a>(events: &'a [Event], tool: &str) -> Vec<&'a str> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::CallOutput {
                tool: t,
                excerpt,
                is_error,
                ..
            } if t == tool => Some((&**excerpt, *is_error)),
            _ => None,
        })
        .map(|(e, _)| e)
        .collect()
}

fn errored_outputs<'a>(events: &'a [Event], tool: &str) -> Vec<&'a str> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::CallOutput {
                tool: t,
                excerpt,
                is_error,
                ..
            } if t == tool && *is_error => Some(&**excerpt),
            _ => None,
        })
        .collect()
}

fn tmp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("ka-contract-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

// ---------------------------------------------------------------- registry

/// The documented built-in tool set: names, clearance tiers, read-only
/// flags. Changing these changes the model-facing surface — deliberate
/// changes must update this test and the docs together.
#[test]
fn registry_core_tools_contract() {
    let registry = hands::registry_with_pathfinder(
        Arc::new(parking_lot::RwLock::new(
            hands::pathfinder::PathfinderSource::default(),
        )),
        ka_engine::hands::todo::slot(),
        Arc::new(hands::jobs::JobTable::new()),
    );
    let mut names: Vec<String> = registry.iter().map(|h| h.def().name).collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            "bash",
            "edit",
            "glob",
            "grep",
            "jobs",
            "pathfinder",
            "read",
            "todo",
            "write"
        ],
        "the built-in tool registry changed — update docs + this contract"
    );
    let by_name = |n: &str| {
        registry
            .iter()
            .find(|h| h.def().name == n)
            .unwrap_or_else(|| panic!("missing hand {n}"))
            .def()
    };
    assert_eq!(by_name("read").clearance, hands::Clearance::Read);
    assert_eq!(by_name("glob").clearance, hands::Clearance::Read);
    assert_eq!(by_name("grep").clearance, hands::Clearance::Read);
    assert_eq!(by_name("pathfinder").clearance, hands::Clearance::Read);
    assert_eq!(by_name("todo").clearance, hands::Clearance::Read);
    assert!(by_name("todo").read_only, "todo must auto-allow");
    assert_eq!(by_name("edit").clearance, hands::Clearance::Write);
    assert_eq!(by_name("write").clearance, hands::Clearance::Write);
    assert_eq!(by_name("bash").clearance, hands::Clearance::Exec);
    assert!(
        !by_name("bash").read_only,
        "bash is the exec primitive; it must never be marked read-only"
    );
}

/// Conditional hands exist with the documented names/tiers: web tools,
/// LSP navigation, MCP call-front, remember, tasks, delegate.
#[test]
fn conditional_hands_contract() {
    // web: fetch always, search only with a provider
    let fetch_only = hands::web::hands(None, false);
    assert_eq!(
        fetch_only.iter().map(|h| h.def().name).collect::<Vec<_>>(),
        vec!["web_fetch"]
    );
    let both = hands::web::hands(
        Some(ka_engine::config::SearchProvider {
            provider: "tavily".into(),
            api_key_env: "K".into(),
            base_url: None,
        }),
        false,
    );
    assert_eq!(
        both.iter().map(|h| h.def().name).collect::<Vec<_>>(),
        vec!["web_search", "web_fetch"]
    );

    // LSP navigation quad
    let lsp = hands::lsp_tools::hands(Arc::new(ka_engine::lsp::LspManager::new(
        Path::new("/tmp"),
        &ka_engine::config::Lsp {
            enable: Some(true),
            commands: None,
            write_through: None,
        },
    )));
    let mut lsp_names: Vec<String> = lsp.iter().map(|h| h.def().name).collect();
    lsp_names.sort();
    assert_eq!(
        lsp_names,
        vec!["definition", "diagnostics", "references", "symbols"]
    );
    for h in &lsp {
        let d = h.def();
        assert_eq!(
            d.clearance,
            hands::Clearance::Read,
            "LSP hands are read-only"
        );
        assert!(d.read_only);
    }

    // LSP write-through pair ([lsp] write_through = true only)
    let lsp_w = hands::lsp_write::hands(Arc::new(ka_engine::lsp::LspManager::new(
        Path::new("/tmp"),
        &ka_engine::config::Lsp {
            enable: Some(true),
            commands: None,
            write_through: Some(true),
        },
    )));
    let mut lsp_w_names: Vec<String> = lsp_w.iter().map(|h| h.def().name).collect();
    lsp_w_names.sort();
    assert_eq!(lsp_w_names, vec!["lsp_actions", "lsp_format", "lsp_rename"]);
    let rename = lsp_w.iter().find(|h| h.def().name == "lsp_rename").unwrap();
    assert_eq!(rename.def().clearance, hands::Clearance::Write);
    assert!(!rename.def().read_only);
    let format = lsp_w.iter().find(|h| h.def().name == "lsp_format").unwrap();
    assert_eq!(
        format.def().clearance,
        hands::Clearance::Write,
        "formatting rewrites the file"
    );
    assert!(!format.def().read_only);
    let actions = lsp_w
        .iter()
        .find(|h| h.def().name == "lsp_actions")
        .unwrap();
    let d = actions.def();
    assert_eq!(d.clearance, hands::Clearance::Read, "static tier = listing");
    assert!(!d.read_only);
    assert_eq!(
        actions.clearance_for(&serde_json::json!({"file": "a.rs", "line": 1})),
        hands::Clearance::Read,
        "listing reads"
    );
    assert_eq!(
        actions.clearance_for(&serde_json::json!({
            "file": "a.rs", "line": 1, "mode": "run", "pick": 1
        })),
        hands::Clearance::Write,
        "running a code action mutates"
    );

    // the engine's registration decision: write-through joins only under
    // write_through = true; the navigation set is identical either way
    let off = ka_engine::config::Lsp {
        enable: Some(true),
        commands: None,
        write_through: None,
    };
    let on = ka_engine::config::Lsp {
        enable: Some(true),
        commands: None,
        write_through: Some(true),
    };
    let nav = ka_engine::lsp_hands(
        Arc::new(ka_engine::lsp::LspManager::new(Path::new("/tmp"), &off)),
        &off,
    );
    let both = ka_engine::lsp_hands(
        Arc::new(ka_engine::lsp::LspManager::new(Path::new("/tmp"), &on)),
        &on,
    );
    assert_eq!(
        nav.len() + 3,
        both.len(),
        "write-through adds exactly three hands"
    );
    assert!(
        nav.iter()
            .all(|h| h.def().name != "lsp_rename" && h.def().name != "lsp_actions")
    );
    assert!(both.iter().any(|h| h.def().name == "lsp_rename"));
    assert!(both.iter().any(|h| h.def().name == "lsp_actions"));
    assert!(both.iter().any(|h| h.def().name == "lsp_format"));

    // the debug probe ([debug] enable = true only): control flow runs at
    // Exec, inspection reads, unknown actions fail closed at Exec
    let dbg = hands::debug::DebugHand::new(Arc::new(ka_engine::dap::DebugManager::new(None)));
    let d = dbg.def();
    assert_eq!(d.name, "debug");
    assert_eq!(d.clearance, hands::Clearance::Exec);
    assert!(!d.read_only);
    for action in [
        "start",
        "break",
        "clear",
        "continue",
        "next",
        "step_in",
        "step_out",
        "pause",
        "disconnect",
    ] {
        assert_eq!(
            dbg.clearance_for(&serde_json::json!({"action": action})),
            hands::Clearance::Exec,
            "{action} drives the debuggee"
        );
    }
    for action in [
        "sessions", "breaks", "threads", "stack", "vars", "eval", "output",
    ] {
        assert_eq!(
            dbg.clearance_for(&serde_json::json!({"action": action})),
            hands::Clearance::Read,
            "{action} only inspects"
        );
    }
    assert_eq!(
        dbg.clearance_for(&serde_json::json!({"action": "wat"})),
        hands::Clearance::Exec,
        "unknown actions fail closed"
    );

    // the tasks hand: listing/reading are Read; steering a running
    // agent and merging its branch mutate (Write); cancel stays Read
    let tasks_hand = hands::tasks::TasksHand::new(ka_engine::hands::tasks::AgentTaskTable::new());
    assert_eq!(tasks_hand.def().name, "tasks");
    assert_eq!(tasks_hand.def().clearance, hands::Clearance::Read);
    assert!(!tasks_hand.def().read_only, "send/merge mutate");
    assert_eq!(
        tasks_hand.clearance_for(&serde_json::json!({"action": "send"})),
        hands::Clearance::Write
    );
    assert_eq!(
        tasks_hand.clearance_for(&serde_json::json!({"action": "merge"})),
        hands::Clearance::Write
    );
    assert_eq!(
        tasks_hand.clearance_for(&serde_json::json!({"action": "list"})),
        hands::Clearance::Read
    );

    // MCP lazy front-hand
    let mcp = ka_engine::mcp::McpCallHand::new(Vec::new());
    let d = mcp.def();
    assert_eq!(d.name, "mcp_call");
    assert_eq!(d.clearance, hands::Clearance::Exec);

    // memory inbox + tasks
    let d = hands::memory::RememberHand.def();
    assert_eq!(d.name, "remember");
    assert_eq!(d.clearance, hands::Clearance::Write);
    let d = hands::tasks::TasksHand::new(hands::tasks::AgentTaskTable::new()).def();
    assert_eq!(d.name, "tasks");
    assert_eq!(d.clearance, hands::Clearance::Read);

    // delegate
    let (evt_tx, _evt_rx) = tokio::sync::mpsc::channel(16);
    let d = hands::delegate::DelegateHand::new(
        vec![ka_engine::agents::AgentDef {
            name: "a".into(),
            description: String::new(),
            system: "s".into(),
            max_steps: 4,
            isolate: false,
            model: None,
            effort: None,
            tools: None,
            output: None,
        }],
        Arc::new(parking_lot::RwLock::new(
            hands::pathfinder::PathfinderSource::default(),
        )),
        ka_protocol::Mode::Free,
        hands::tasks::AgentTaskTable::new(),
        evt_tx,
    )
    .def();
    assert_eq!(d.name, "delegate");
    assert_eq!(d.clearance, hands::Clearance::Read);
    assert!(
        d.description.contains("background: true"),
        "delegate must advertise the background escape hatch"
    );
}

// ------------------------------------------------------------ config/schema

/// Every documented config key must survive `ka config schema` — a key
/// disappearing from the schema is a breaking config change.
#[test]
fn config_schema_carries_every_documented_key() {
    let schema = ka_engine::config::Config::schema_json().unwrap();
    for key in [
        "model",
        "effort",
        "mode",
        "max_steps",
        "cwd",
        "rules",
        "hooks",
        "mcp",
        "permissions",
        "allow",
        "guards",
        "spend_usd",
        "context_pct",
        "fallback",
        "models",
        "update",
        "repo",
        "context",
        "promote",
        "search",
        "provider",
        "api_key_env",
        "base_url",
        "sandbox",
        "lsp",
        "enable",
        "commands",
        "write_through",
        "debug",
        "enable",
        "adapters",
        "tui",
        "header_glyph",
        "bell",
        "notify",
        "tools",
        "bash",
        "background_after_ms",
        "read",
        "max_image_mb",
        "web",
        "allow_private_hosts",
        "discovery",
        "git",
        "auto_commit",
        "verify",
        "test",
        "lints",
        "pattern",
        "command",
        "roles",
        "default",
        "fast",
    ] {
        assert!(
            schema.contains(&format!("\"{key}\"")),
            "config schema lost documented key {key:?}"
        );
    }
}

/// Selector grammar: `vendor/model@effort`, colon-bearing local model
/// ids, effort-only override shape.
#[test]
fn selectors_parse_vendor_model_effort_and_colons() {
    let s = ka_dialect::parse_selector("anthropic/claude-sonnet-5@high").unwrap();
    assert_eq!(s.vendor, "anthropic");
    assert_eq!(s.model, "claude-sonnet-5");
    assert_eq!(s.effort.as_deref(), Some("high"));
    assert_eq!(s.model_id(), "anthropic/claude-sonnet-5");

    let s = ka_dialect::parse_selector("ollama/qwen3.5:9b").unwrap();
    assert_eq!(s.vendor, "ollama");
    assert_eq!(s.model, "qwen3.5:9b", "colons in model ids must survive");
    assert_eq!(s.model_id(), "ollama/qwen3.5:9b");

    // @ splits on the LAST occurrence
    let s = ka_dialect::parse_selector("vendor/m@o@high").unwrap();
    assert_eq!(s.model, "m@o");
    assert_eq!(s.effort.as_deref(), Some("high"));

    assert!(ka_dialect::parse_selector("no-slash").is_err());
}

/// ka glob semantics (rules, lint patterns, scoped rules): `*` crosses
/// directory separators, `?` matches one char.
#[test]
fn glob_semantics_star_crosses_directories() {
    assert!(ka_engine::voice::glob_match("*.rs", "src/deep/a.rs"));
    assert!(ka_engine::voice::glob_match("**/*.ts", "src/x/a.ts"));
    assert!(ka_engine::voice::glob_match(
        "cargo *",
        "cargo build --release"
    ));
    assert!(!ka_engine::voice::glob_match("cargo *", "rustc build"));
    assert!(ka_engine::voice::glob_match("a?c", "abc"));
    assert!(!ka_engine::voice::glob_match("a?c", "ac"));
}

/// Web-tool rules match hosts with domain semantics; other tools use
/// plain globs on the primary argument.
#[test]
fn web_domain_rule_semantics() {
    use ka_engine::voice::rule_pattern_matches;
    let url = "https://api.example.com/v1/x?y=1";
    assert!(
        rule_pattern_matches("example.com", "web_fetch", url),
        "apex covers subdomains"
    );
    assert!(rule_pattern_matches("*.example.com", "web_fetch", url));
    assert!(
        !rule_pattern_matches("*.example.com", "web_fetch", "https://example.com/"),
        "star rule is subdomains only"
    );
    assert!(
        !rule_pattern_matches("example.com", "web_fetch", "https://notexample.com/"),
        "suffix must be domain-bound"
    );
    assert!(rule_pattern_matches(
        "example.com",
        "web_search",
        "https://example.com"
    ));
    // non-web tools keep plain glob semantics
    assert!(rule_pattern_matches("cargo *", "bash", "cargo test"));
    assert!(!rule_pattern_matches("example.com", "bash", "anything"));
}

// ------------------------------------------------------------ gate matrix

/// free mode: writes run without an ask (the documented default tier).
#[tokio::test]
async fn free_mode_writes_without_ask() {
    let dir = tmp_dir("free-write");
    let mut voice = Voice::new(test_catalog(), dir.clone(), ka_protocol::Mode::Free, 5)
        .with_speaker(
            ka_dialect::Wire::OpenaiChat,
            Scripted::calls(vec![(
                "write",
                serde_json::json!({"path": "new.txt", "content": "hello"}),
            )]),
        );
    let events = run_turn(&mut voice, "make a file", None).await;
    assert!(
        asks_of(&events).is_empty(),
        "free mode must not ask for writes: {events:?}"
    );
    assert_eq!(errored_outputs(&events, "write").len(), 0, "{events:?}");
    assert!(
        dir.join("new.txt").exists(),
        "write hand executed: {events:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// guarded mode: writes ask; a deny answer blocks the call without
/// executing it.
#[tokio::test]
async fn guarded_write_asks_and_deny_blocks() {
    let dir = tmp_dir("guarded-write");
    let mut voice = Voice::new(test_catalog(), dir.clone(), ka_protocol::Mode::Guarded, 5)
        .with_speaker(
            ka_dialect::Wire::OpenaiChat,
            Scripted::calls(vec![(
                "write",
                serde_json::json!({"path": "guarded.txt", "content": "x"}),
            )]),
        );
    let events = run_turn(&mut voice, "make a file", Some(2)).await;
    let asks = asks_of(&events);
    assert_eq!(asks.len(), 1, "guarded write must ask once: {events:?}");
    assert!(asks[0].text.contains("modify files"), "{}", asks[0].text);
    let errs = errored_outputs(&events, "write");
    assert!(
        errs.iter().any(|e| e.contains("permission denied by user")),
        "{errs:?}"
    );
    assert!(
        !dir.join("guarded.txt").exists(),
        "denied write must not execute"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Hardstops are unbypassable-by-mode: they ask even in free mode, and
/// a deny answer keeps the catastrophic command from ever running.
#[tokio::test]
async fn hardstop_asks_even_in_free() {
    let dir = tmp_dir("hardstop");
    let mut voice = Voice::new(test_catalog(), dir.clone(), ka_protocol::Mode::Free, 5)
        .with_speaker(
            ka_dialect::Wire::OpenaiChat,
            Scripted::calls(vec![("bash", serde_json::json!({"command": "rm -rf /"}))]),
        );
    let events = run_turn(&mut voice, "clean the disk", Some(2)).await;
    let asks = asks_of(&events);
    assert_eq!(asks.len(), 1, "hardstop must ask in free mode: {events:?}");
    assert!(asks[0].text.contains("HARDSTOP"), "{}", asks[0].text);
    assert!(
        asks[0].text.contains("recursive delete"),
        "{}",
        asks[0].text
    );
    let errs = errored_outputs(&events, "bash");
    assert!(
        errs.iter().any(|e| e.contains("permission denied by user")),
        "{errs:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Protected paths ask even in free mode, even with an allow rule and
/// a session allowlist — the injected-prompt defense.
#[tokio::test]
async fn protected_path_asks_even_with_allow_rule() {
    let dir = tmp_dir("protected-ask");
    let mut voice = Voice::new(test_catalog(), dir.clone(), ka_protocol::Mode::Free, 5)
        .with_speaker(
            ka_dialect::Wire::OpenaiChat,
            Scripted::calls(vec![(
                "write",
                serde_json::json!({"path": ".git/hooks/pre-commit", "content": "#!/bin/sh"}),
            )]),
        );
    voice.set_rules(vec![ka_engine::config::Rule {
        tool: "write".into(),
        pattern: None,
        verdict: ka_engine::config::Verdict::Allow,
    }]);
    voice.set_allowed_tools(vec!["write".into()]);
    let events = run_turn(&mut voice, "add a hook", Some(2)).await;
    let asks = asks_of(&events);
    assert_eq!(asks.len(), 1, "protected path must ask: {events:?}");
    assert!(asks[0].text.contains("PROTECTED"), "{}", asks[0].text);
    assert!(asks[0].text.contains("git internals"), "{}", asks[0].text);
    assert!(
        !dir.join(".git/hooks/pre-commit").exists(),
        "denied protected write must not execute"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Plan mode matrix: writes inside .ka/plans/ run; everything else
/// denies with the documented message; exec asks.
#[tokio::test]
async fn plan_mode_matrix() {
    let dir = tmp_dir("plan-mode");
    std::fs::create_dir_all(dir.join(".ka/plans")).unwrap();

    // allowed: the plans directory
    let mut voice = Voice::new(test_catalog(), dir.clone(), ka_protocol::Mode::Plan, 5)
        .with_speaker(
            ka_dialect::Wire::OpenaiChat,
            Scripted::calls(vec![(
                "write",
                serde_json::json!({"path": ".ka/plans/plan.md", "content": "# plan"}),
            )]),
        );
    let events = run_turn(&mut voice, "draft the plan", None).await;
    assert!(asks_of(&events).is_empty(), "{events:?}");
    assert_eq!(errored_outputs(&events, "write").len(), 0);
    assert!(dir.join(".ka/plans/plan.md").exists());
    drop(voice);

    // denied: anything outside the plans directory
    let mut voice = Voice::new(test_catalog(), dir.clone(), ka_protocol::Mode::Plan, 5)
        .with_speaker(
            ka_dialect::Wire::OpenaiChat,
            Scripted::calls(vec![(
                "write",
                serde_json::json!({"path": "src/main.rs", "content": "x"}),
            )]),
        );
    let events = run_turn(&mut voice, "implement", None).await;
    let errs = errored_outputs(&events, "write");
    assert!(
        errs.iter().any(|e| e.contains("plan mode is read-only")),
        "{errs:?}"
    );
    assert!(!dir.join("src/main.rs").exists());
    drop(voice);

    // exec: read-only commands auto-allow (research), everything else asks
    let mut voice = Voice::new(test_catalog(), dir.clone(), ka_protocol::Mode::Plan, 5)
        .with_speaker(
            ka_dialect::Wire::OpenaiChat,
            Scripted::calls(vec![("bash", serde_json::json!({"command": "ls"}))]),
        );
    let events = run_turn(&mut voice, "look around", None).await;
    assert!(
        asks_of(&events).is_empty(),
        "read-only exec must auto-allow in plan mode: {events:?}"
    );
    drop(voice);
    let mut voice = Voice::new(test_catalog(), dir.clone(), ka_protocol::Mode::Plan, 5)
        .with_speaker(
            ka_dialect::Wire::OpenaiChat,
            Scripted::calls(vec![(
                "bash",
                serde_json::json!({"command": "touch probe.txt"}),
            )]),
        );
    let events = run_turn(&mut voice, "make a file", Some(2)).await;
    let asks = asks_of(&events);
    assert_eq!(
        asks.len(),
        1,
        "non-readonly plan-mode exec must ask: {events:?}"
    );
    assert!(asks[0].text.contains("plan mode"), "{}", asks[0].text);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Rules: first match wins, before mode logic; verdicts allow/ask/deny.
#[tokio::test]
async fn rules_first_match_allow_ask_deny() {
    let dir = tmp_dir("rules");
    // allow rule: `echo *` runs ungated even in guarded mode
    let mut voice = Voice::new(test_catalog(), dir.clone(), ka_protocol::Mode::Guarded, 5)
        .with_speaker(
            ka_dialect::Wire::OpenaiChat,
            Scripted::calls(vec![(
                "bash",
                serde_json::json!({"command": "echo rule-allow"}),
            )]),
        );
    voice.set_rules(vec![ka_engine::config::Rule {
        tool: "bash".into(),
        pattern: Some("echo *".into()),
        verdict: ka_engine::config::Verdict::Allow,
    }]);
    let events = run_turn(&mut voice, "greet", None).await;
    assert!(
        asks_of(&events).is_empty(),
        "allow rule must skip the ask: {events:?}"
    );
    assert!(
        outputs_of(&events, "bash")
            .iter()
            .any(|o| o.contains("rule-allow")),
        "{events:?}"
    );
    drop(voice);

    // deny rule: `git *` refuses without asking
    let mut voice = Voice::new(test_catalog(), dir.clone(), ka_protocol::Mode::Guarded, 5)
        .with_speaker(
            ka_dialect::Wire::OpenaiChat,
            Scripted::calls(vec![("bash", serde_json::json!({"command": "git status"}))]),
        );
    voice.set_rules(vec![ka_engine::config::Rule {
        tool: "bash".into(),
        pattern: Some("git *".into()),
        verdict: ka_engine::config::Verdict::Deny,
    }]);
    let events = run_turn(&mut voice, "check git", None).await;
    assert!(asks_of(&events).is_empty(), "deny must not ask: {events:?}");
    assert!(
        errored_outputs(&events, "bash")
            .iter()
            .any(|e| e.contains("denied by rule")),
        "{events:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Session "always allow": answering an ask with choice 1 auto-allows
/// every later call of that tool for the session.
#[tokio::test]
async fn session_always_allow_skips_later_asks() {
    let dir = tmp_dir("always-allow");
    let mut voice = Voice::new(test_catalog(), dir.clone(), ka_protocol::Mode::Guarded, 5)
        .with_speaker(
            ka_dialect::Wire::OpenaiChat,
            Scripted::calls(vec![
                (
                    "write",
                    serde_json::json!({"path": "a.txt", "content": "1"}),
                ),
                (
                    "write",
                    serde_json::json!({"path": "b.txt", "content": "2"}),
                ),
            ]),
        );
    let events = run_turn(&mut voice, "two files", Some(1)).await;
    assert_eq!(
        asks_of(&events).len(),
        1,
        "exactly one ask: the second write rides the always-allow: {events:?}"
    );
    assert!(dir.join("a.txt").exists());
    assert!(dir.join("b.txt").exists());
    assert!(
        dir.join(".ka/ka.toml").exists(),
        "always-allow persists to the project layer"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The read ledger: edits refuse files that were never read, and files
/// changed since the read; read-then-edit succeeds.
#[tokio::test]
async fn read_ledger_contract() {
    let dir = tmp_dir("ledger");
    std::fs::write(dir.join("f.txt"), "alpha beta").unwrap();

    // unread edit refuses
    let mut voice = Voice::new(test_catalog(), dir.clone(), ka_protocol::Mode::Free, 5)
        .with_speaker(
            ka_dialect::Wire::OpenaiChat,
            Scripted::calls(vec![(
                "edit",
                serde_json::json!({"path": "f.txt", "old": "alpha", "new": "ALPHA"}),
            )]),
        );
    let events = run_turn(&mut voice, "edit it", None).await;
    assert!(
        errored_outputs(&events, "edit")
            .iter()
            .any(|e| e.contains("has not been read yet")),
        "{events:?}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("f.txt")).unwrap(),
        "alpha beta"
    );
    drop(voice);

    // read-then-edit succeeds
    let mut voice = Voice::new(test_catalog(), dir.clone(), ka_protocol::Mode::Free, 5)
        .with_speaker(
            ka_dialect::Wire::OpenaiChat,
            Scripted::calls(vec![
                ("read", serde_json::json!({"path": "f.txt"})),
                (
                    "edit",
                    serde_json::json!({"path": "f.txt", "old": "alpha", "new": "ALPHA"}),
                ),
            ]),
        );
    let events = run_turn(&mut voice, "edit it", None).await;
    assert_eq!(errored_outputs(&events, "edit").len(), 0, "{events:?}");
    assert_eq!(
        std::fs::read_to_string(dir.join("f.txt")).unwrap(),
        "ALPHA beta"
    );

    // changed-since-read refuses: the mutation lands between turns, the
    // ledger keeps the old stamp, and the next-turn edit is refused
    let mut voice = Voice::new(test_catalog(), dir.clone(), ka_protocol::Mode::Free, 5)
        .with_speaker(
            ka_dialect::Wire::OpenaiChat,
            Scripted::calls(vec![("read", serde_json::json!({"path": "f.txt"}))]),
        );
    let _ = run_turn(&mut voice, "read it", None).await;
    std::fs::write(dir.join("f.txt"), "CHANGED behind the read").unwrap();
    let mut voice = Voice::new(test_catalog(), dir.clone(), ka_protocol::Mode::Free, 5)
        .with_speaker(
            ka_dialect::Wire::OpenaiChat,
            Scripted::calls(vec![(
                "edit",
                serde_json::json!({"path": "f.txt", "old": "CHANGED", "new": "EDITED"}),
            )]),
        );
    let events = run_turn(&mut voice, "edit it", None).await;
    assert!(
        errored_outputs(&events, "edit")
            .iter()
            .any(|e| e.contains("has not been read yet")),
        "a fresh voice has an empty ledger: {events:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------- hooks

/// Config hooks: pre_tool_use exit 2 blocks the call with the hook's
/// stderr as the reason — the ecosystem-compatible contract.
#[tokio::test]
async fn hook_exit2_blocks_pre_tool_use() {
    let dir = tmp_dir("hook-block");
    let mut voice = Voice::new(test_catalog(), dir.clone(), ka_protocol::Mode::Free, 5)
        .with_speaker(
            ka_dialect::Wire::OpenaiChat,
            Scripted::calls(vec![("bash", serde_json::json!({"command": "echo hi"}))]),
        );
    voice.set_hooks(vec![ka_engine::config::Hook {
        event: ka_engine::config::HookEvent::PreToolUse,
        tool: Some("bash".into()),
        command: "echo no-way >&2; exit 2".into(),
    }]);
    let events = run_turn(&mut voice, "run it", None).await;
    assert!(
        errored_outputs(&events, "bash")
            .iter()
            .any(|e| e.contains("blocked by hook") && e.contains("no-way")),
        "{events:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Hook steering: a clean-exit pre_tool_use hook emitting
/// `{"mode":"plan","note":"…"}` switches the permission mode mid-turn.
#[tokio::test]
async fn hook_stdout_steers_mode() {
    let dir = tmp_dir("hook-steer");
    let mut voice = Voice::new(test_catalog(), dir.clone(), ka_protocol::Mode::Free, 5)
        .with_speaker(
            ka_dialect::Wire::OpenaiChat,
            Scripted::calls(vec![("bash", serde_json::json!({"command": "echo hi"}))]),
        );
    voice.set_hooks(vec![ka_engine::config::Hook {
        event: ka_engine::config::HookEvent::PreToolUse,
        tool: None,
        command: "printf '{\"mode\":\"plan\",\"note\":\"steered\"}'".into(),
    }]);
    let events = run_turn(&mut voice, "run it", None).await;
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::ModeChanged {
                mode: ka_protocol::Mode::Plan
            }
        )),
        "steering must switch the mode: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::Note { message } if message.contains("steered"))),
        "the steering note surfaces: {events:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------- verify

/// [verify] lints ride successful edit results as informational
/// context (never errors), and the edited-file list is tracked for the
/// test loop.
#[tokio::test]
async fn verify_lint_rides_edit_result() {
    let dir = tmp_dir("verify-lint");
    std::fs::write(dir.join("a.rs"), "fn main() {}\n").unwrap();
    let mut voice = Voice::new(test_catalog(), dir.clone(), ka_protocol::Mode::Free, 5)
        .with_speaker(
            ka_dialect::Wire::OpenaiChat,
            Scripted::calls(vec![
                ("read", serde_json::json!({"path": "a.rs"})),
                ("edit", serde_json::json!({"path": "a.rs", "old": "fn main() {}", "new": "fn main() -> () {}"})),
            ]),
        );
    voice.set_verify(ka_engine::config::Verify {
        test: None,
        lints: vec![ka_engine::config::LintRule {
            pattern: "*.rs".into(),
            command: "echo LINT-FAIL >&2; exit 3 {file}".into(),
        }],
    });
    let events = run_turn(&mut voice, "refactor", None).await;
    let edit_outputs = outputs_of(&events, "edit");
    assert!(
        edit_outputs
            .iter()
            .any(|o| o.contains("<lint note") && o.contains("LINT-FAIL") && o.contains("exit 3")),
        "lint block must ride the edit result: {edit_outputs:?}"
    );
    // the tool result itself stays non-error
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::CallOutput { tool, is_error: true, .. } if tool == "edit")),
        "lint must never flip the tool error flag: {events:?}"
    );
    assert_eq!(voice.take_edited(), vec!["a.rs".to_string()]);
    let _ = std::fs::remove_dir_all(&dir);
}

// ------------------------------------------------- background delegates

/// delegate {background: true} returns immediately, registers a task,
/// and the runner's outcome lands in the table (here: a fast provider
/// failure, since the scripted speaker cannot reach the nested voice).
#[tokio::test]
async fn background_delegate_registers_and_reports() {
    let dir = tmp_dir("bg-delegate");
    let mut voice = Voice::new(test_catalog(), dir.clone(), ka_protocol::Mode::Free, 5)
        .with_speaker(
            ka_dialect::Wire::OpenaiChat,
            Scripted::calls(vec![(
                "delegate",
                serde_json::json!({
                    "agent": "scout",
                    "task": "look around",
                    "background": true
                }),
            )]),
        );
    let table = hands::tasks::AgentTaskTable::new();
    let (note_tx, _note_rx) = tokio::sync::mpsc::channel(16);
    {
        let slot = voice.pathfinder_slot();
        slot.write().catalog = test_catalog();
        slot.write().model = Some("test/m".into());
    }
    voice.push_hand(Arc::new(hands::delegate::DelegateHand::new(
        vec![ka_engine::agents::AgentDef {
            name: "scout".into(),
            description: "explores".into(),
            system: "you scout".into(),
            max_steps: 4,
            isolate: false,
            model: None,
            effort: None,
            tools: None,
            output: None,
        }],
        voice.pathfinder_slot(),
        ka_protocol::Mode::Free,
        table.clone(),
        note_tx,
    )));
    let events = run_turn(&mut voice, "delegate it", None).await;
    let outs = outputs_of(&events, "delegate");
    assert!(
        outs.iter()
            .any(|o| o.contains("background task t-1 started")),
        "background delegate returns immediately: {outs:?}"
    );
    // the task is visible and running; cancel really aborts the runner
    // (the nested voice retries provider errors, so the deterministic
    // lifecycle check is cancel — finish() is unit-tested in tasks.rs)
    let live = table.result(1).expect("task registered");
    assert!(live.contains("t-1") && live.contains("running"), "{live:?}");
    let cancelled = {
        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel::<Command>(16);
        let (evt_tx, evt_rx) = tokio::sync::mpsc::channel::<Event>(64);
        let tasks_hand = hands::tasks::TasksHand::new(table.clone());
        let out = tasks_hand
            .execute(
                &serde_json::json!({"action": "cancel", "id": 1}),
                &ka_engine::hands::HandContext {
                    cwd: dir.clone(),
                    ledger: Arc::new(parking_lot::Mutex::new(ka_engine::hands::Ledger::default())),
                    spill: Arc::new(ka_engine::hands::Spill::new()),
                    snapshots: Arc::new(parking_lot::Mutex::new(
                        ka_engine::hands::snapshots::Snapshots::inert(),
                    )),
                    jobs: Arc::new(ka_engine::hands::jobs::JobTable::new()),
                    bash_background_ms: 0,
                    max_image_mb: 5,
                    web_allow_private: false,
                    sandbox: ka_sandbox::Policy::Off,
                },
            )
            .await;
        drop((cmd_tx, cmd_rx, evt_tx, evt_rx));
        out
    };
    assert!(
        !cancelled.is_error && cancelled.content.contains("t-1 cancelled"),
        "{}",
        cancelled.content
    );
    assert!(
        table.result(1).unwrap().contains("cancelled"),
        "cancel sticks: {:?}",
        table.result(1)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------- secrets

/// One-way secret redaction: values disappear, names stay, longest
/// value wins, non-secrets untouched.
#[test]
fn secrets_redact_pairs_contract() {
    let pairs = vec![
        (
            "ANTHROPIC_API_KEY".to_string(),
            "sk-ant-super-secret".to_string(),
        ),
        ("SHORT".to_string(), "abc".to_string()),
    ];
    let text = "key is sk-ant-super-secret and abc and normal text";
    let out = ka_engine::hands::secrets::redact_from(text, &pairs);
    assert!(!out.contains("sk-ant-super-secret"), "{out}");
    assert!(!out.contains("abc"), "{out}");
    assert!(
        out.contains("ANTHROPIC_API_KEY") || out.contains("[REDACTED"),
        "{out}"
    );
    assert!(out.contains("normal text"), "{out}");
    assert_eq!(
        ka_engine::hands::secrets::redact_from("nothing secret here", &pairs),
        "nothing secret here"
    );
}

// ------------------------------------------------------------- snapshots

/// Snapshot/undo: the write hand parks pre-mutation bytes; undo
/// restores the previous content; creation-undos delete.
#[tokio::test]
async fn snapshot_undo_contract() {
    let dir = tmp_dir("snapshots");
    std::fs::write(dir.join("existing.txt"), "original").unwrap();
    let mut voice = Voice::new(test_catalog(), dir.clone(), ka_protocol::Mode::Free, 5)
        .with_speaker(
            ka_dialect::Wire::OpenaiChat,
            Scripted::calls(vec![
                ("read", serde_json::json!({"path": "existing.txt"})),
                ("edit", serde_json::json!({"path": "existing.txt", "old": "original", "new": "REWRITTEN"})),
                ("write", serde_json::json!({"path": "created.txt", "content": "fresh"})),
            ]),
        );
    let events = run_turn(&mut voice, "mutate", None).await;
    assert_eq!(errored_outputs(&events, "edit").len(), 0, "{events:?}");
    assert_eq!(
        std::fs::read_to_string(dir.join("existing.txt")).unwrap(),
        "REWRITTEN"
    );
    assert!(dir.join("created.txt").exists());

    // undo 1: the creation is removed
    let sink = voice.snapshot_sink();
    sink.lock().undo().unwrap();
    assert!(
        !dir.join("created.txt").exists(),
        "creation-undo must delete the file"
    );
    // undo 2: the edit is restored
    sink.lock().undo().unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.join("existing.txt")).unwrap(),
        "original",
        "edit-undo must restore the pre-mutation bytes"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
