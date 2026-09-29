//! End-to-end engine contracts over the real `spawn_full` loop: the
//! surface-facing commands every documented feature rides on. If one of
//! these breaks, a user-visible command broke — deliberate changes must
//! update this file and the docs together.

#![allow(clippy::unwrap_used, clippy::expect_used)]
use std::time::Duration;

use ka_dialect::Catalog;
use ka_protocol::{Command, Event, Stop};

fn test_catalog() -> ka_dialect::Catalog {
    Catalog::parse(
        "[dialects.\"test/m\"]\nwire = \"openai_chat\"\nbase_url = \"http://127.0.0.1:1\"\ncontext = 1000000\n",
    )
    .unwrap()
}

fn tmp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("ka-e2e-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Send a command and collect events until `done` matches (or the
/// stream closes). Most commands do NOT emit `Idle` — only the prompt
/// cycle does — so the caller states what completes this exchange.
/// Fails loudly on timeout so a stuck engine fails, never hangs.
async fn settle(
    handle: &mut ka_engine::EngineHandle,
    cmd: Command,
    done: impl Fn(&Event) -> bool,
) -> Vec<Event> {
    settle_answering(handle, cmd, &done, None).await
}

/// Send a command and collect events until `done` matches, then drain
/// until the engine goes quiet (300 ms without events). The quiet drain
/// is load-bearing: some arms emit trailing events (Idle), and sending
/// the next command while a turn is still live would let the voice's
/// mid-turn command select silently swallow it. Permission asks are
/// auto-answered with `answer` when given.
async fn settle_answering(
    handle: &mut ka_engine::EngineHandle,
    cmd: Command,
    done: &dyn Fn(&Event) -> bool,
    answer: Option<usize>,
) -> Vec<Event> {
    handle.commands.send(cmd).await.expect("engine alive");
    let mut out = Vec::new();
    loop {
        match tokio::time::timeout(Duration::from_secs(15), handle.events.recv()).await {
            Ok(Some(e)) => {
                if let (Event::Ask { id, .. }, Some(choice)) = (&e, answer) {
                    handle
                        .commands
                        .send(Command::Answer {
                            question: id.clone(),
                            choice,
                        })
                        .await
                        .ok();
                }
                let complete = done(&e);
                out.push(e);
                if complete {
                    break;
                }
            }
            Ok(None) => return out,
            Err(_) => panic!("engine event did not arrive within 15s; got {out:?}"),
        }
    }
    // quiet drain: the engine idles only after the arm fully returned
    loop {
        match tokio::time::timeout(Duration::from_millis(300), handle.events.recv()).await {
            Ok(Some(e)) => out.push(e),
            _ => return out,
        }
    }
}

/// The prompt cycle completes with `Idle` (the one event that always
/// terminates a prompt exchange).
fn on_idle(e: &Event) -> bool {
    matches!(e, Event::Idle)
}

use std::collections::VecDeque;

use ka_dialect::speaker::{
    SpeakFuture, SpeakRequest, Speaker, StreamEvent, ToolCall, TurnMessage, TurnRole,
};

/// A speaker that emits one scripted tool call per request round and a
/// final text reply once the queue is empty, recording every request —
/// so tests can assert what the model actually received (e.g. the
/// [verify] fix-round prompt).
struct Scripted {
    calls: parking_lot::Mutex<VecDeque<(&'static str, serde_json::Value)>>,
    requests: parking_lot::Mutex<Vec<Vec<TurnMessage>>>,
    models: parking_lot::Mutex<Vec<String>>,
    efforts: parking_lot::Mutex<Vec<Option<String>>>,
    tool_counts: parking_lot::Mutex<Vec<usize>>,
}

impl Scripted {
    fn new(calls: Vec<(&'static str, serde_json::Value)>) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            calls: parking_lot::Mutex::new(calls.into()),
            requests: parking_lot::Mutex::new(Vec::new()),
            models: parking_lot::Mutex::new(Vec::new()),
            efforts: parking_lot::Mutex::new(Vec::new()),
            tool_counts: parking_lot::Mutex::new(Vec::new()),
        })
    }
}

impl Speaker for Scripted {
    fn speak<'a>(
        &'a self,
        req: SpeakRequest,
        out: tokio::sync::mpsc::Sender<StreamEvent>,
    ) -> SpeakFuture<'a> {
        Box::pin(async move {
            self.requests.lock().push(req.messages.clone());
            self.models.lock().push(req.model_id.clone());
            self.efforts.lock().push(req.effort.clone());
            self.tool_counts.lock().push(req.tools.len());
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
                stop: Stop::Done,
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

/// A canned prompt turn runs to completion: exactly one TurnFinished
/// (stop = Done) and one Idle — the Phase-0 contract every surface
/// builds on.
#[tokio::test]
async fn prompt_canned_turn_contract() {
    let dir = tmp_dir("prompt");
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let mut handle = ka_engine::spawn_with(cfg, test_catalog());
    let events = settle(
        &mut handle,
        Command::Prompt {
            allowed_tools: None,
            model: None,
            text: "hello".into(),
            schema: None,
            images: Vec::new(),
        },
        on_idle,
    )
    .await;
    let finishes: Vec<&Stop> = events
        .iter()
        .filter_map(|e| match e {
            Event::TurnFinished { stop, .. } => Some(stop),
            _ => None,
        })
        .collect();
    assert_eq!(finishes, vec![&Stop::Done], "{events:?}");
    assert!(events.iter().any(|e| matches!(e, Event::Idle)));
    let _ = std::fs::remove_dir_all(&dir);
}

/// `!` passthrough: the engine runs the user's shell command, reports
/// the output (redacted, capped), and stages it as context.
#[tokio::test]
async fn shell_passthrough_runs_and_reports() {
    let dir = tmp_dir("shell");
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let mut handle = ka_engine::spawn_with(cfg, test_catalog());
    let events = settle(
        &mut handle,
        Command::Shell {
            command: "echo contract-echo-9137".into(),
        },
        |e| matches!(e, Event::ShellOutput { .. }),
    )
    .await;
    match events
        .iter()
        .find(|e| matches!(e, Event::ShellOutput { .. }))
    {
        Some(Event::ShellOutput {
            command,
            output,
            note,
        }) => {
            assert_eq!(command, "echo contract-echo-9137");
            assert!(output.contains("contract-echo-9137"), "{output:?}");
            assert!(note.is_none(), "clean exit carries no note: {note:?}");
        }
        other => panic!("expected ShellOutput, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Shell failures surface in the note field (non-zero exit and spawn
/// errors), never as a hard error event.
#[tokio::test]
async fn shell_passthrough_failure_notes() {
    let dir = tmp_dir("shell-fail");
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let mut handle = ka_engine::spawn_with(cfg, test_catalog());
    let events = settle(
        &mut handle,
        Command::Shell {
            command: "exit 7".into(),
        },
        |e| matches!(e, Event::ShellOutput { .. }),
    )
    .await;
    match events
        .iter()
        .find(|e| matches!(e, Event::ShellOutput { .. }))
    {
        Some(Event::ShellOutput { output, note, .. }) => {
            assert_eq!(note.as_deref(), Some("exit 7"), "{events:?}");
            assert!(output.is_empty(), "{output:?}");
        }
        other => panic!("expected ShellOutput, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// /tasks snapshot: empty state reports cleanly; the command never
/// errors.
#[tokio::test]
async fn list_tasks_reports_snapshot() {
    let dir = tmp_dir("tasks");
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let mut handle = ka_engine::spawn_with(cfg, test_catalog());
    let events = settle(&mut handle, Command::ListTasks, |e| {
        matches!(e, Event::Tasks { .. })
    })
    .await;
    match events.iter().find(|e| matches!(e, Event::Tasks { .. })) {
        Some(Event::Tasks { rows }) => {
            assert!(
                rows.iter().any(|r| r.contains("no background")),
                "empty state is a row, not an error: {rows:?}"
            );
        }
        other => panic!("expected Tasks, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// /tasks pager: an unknown task id errors with the documented message
/// (the picker stays open, the engine stays responsive).
#[tokio::test]
async fn task_detail_unknown_id_errors() {
    let dir = tmp_dir("task-detail");
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let mut handle = ka_engine::spawn_with(cfg, test_catalog());
    let events = settle(&mut handle, Command::TaskDetail { id: 99 }, |e| {
        matches!(e, Event::Error { .. })
    })
    .await;
    match events.iter().find(|e| matches!(e, Event::Error { .. })) {
        Some(Event::Error { message, .. }) => {
            assert!(message.contains("no such task: t-99"), "{events:?}");
        }
        other => panic!("expected Error, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// /debug overlay: with no [debug] config the roster says the tier is
/// disabled and the engine still reaches Idle.
#[tokio::test]
async fn debug_roster_reports_disabled_tier() {
    let dir = tmp_dir("debug-roster");
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let mut handle = ka_engine::spawn_with(cfg, test_catalog());
    let events = settle(&mut handle, Command::DebugRoster, |e| {
        matches!(e, Event::DebugRoster { .. })
    })
    .await;
    match events
        .iter()
        .find(|e| matches!(e, Event::DebugRoster { .. }))
    {
        Some(Event::DebugRoster { rows }) => {
            assert!(
                rows.iter().any(|r| r.contains("debug tier disabled")),
                "{rows:?}"
            );
        }
        other => panic!("expected DebugRoster, got {other:?}"),
    }
    assert!(
        events.iter().any(|e| matches!(e, Event::Idle)),
        "the arm ends in Idle: {events:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Rewind on an empty session errors with the documented message
/// instead of corrupting state.
#[tokio::test]
async fn rewind_on_empty_history_errors() {
    let dir = tmp_dir("rewind-empty");
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let mut handle = ka_engine::spawn_with(cfg, test_catalog());
    let events = settle(&mut handle, Command::Rewind { turns: 1 }, |e| {
        matches!(e, Event::Error { .. } | Event::Note { .. } | Event::Idle)
    })
    .await;
    assert!(
        events.iter().any(
            |e| matches!(e, Event::Error { message, .. } if message.contains("cannot rewind"))
        ),
        "{events:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Undo with nothing parked is a clean note, never an error.
#[tokio::test]
async fn undo_without_snapshots_is_a_note() {
    let dir = tmp_dir("undo-empty");
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let mut handle = ka_engine::spawn_with(cfg, test_catalog());
    let events = settle(&mut handle, Command::UndoFile, |e| {
        matches!(e, Event::Note { .. } | Event::Error { .. })
    })
    .await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::Note { message } if message.contains("nothing to undo"))),
        "{events:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Checkpoint/restore on a git repo: the checkpoint notes its id, a
/// working-tree change is reverted by restore, and the user's git
/// state (HEAD, index) is untouched.
#[tokio::test]
async fn checkpoint_and_restore_revert_working_tree() {
    let dir = tmp_dir("checkpoint");
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(&dir)
            .output()
            .unwrap()
    };
    assert!(git(&["init", "-q"]).status.success());
    git(&["config", "user.email", "t@t"]);
    git(&["config", "user.name", "t"]);
    std::fs::write(dir.join("f.txt"), "v1\n").unwrap();
    git(&["add", "."]);
    assert!(git(&["commit", "-m", "init"]).status.success());
    let head_before = String::from_utf8_lossy(&git(&["rev-parse", "HEAD"]).stdout).to_string();

    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let mut handle = ka_engine::spawn_with(cfg, test_catalog());
    let events = settle(&mut handle, Command::Checkpoint, |e| {
        matches!(e, Event::Note { .. } | Event::Error { .. })
    })
    .await;
    let note = events.iter().find_map(|e| match e {
        Event::Note { message } => Some(message.clone()),
        _ => None,
    });
    assert!(
        note.as_deref()
            .is_some_and(|n| n.contains("checkpoint") && n.contains("saved")),
        "{events:?}"
    );
    // the note carries the SHORT checkpoint id; restore by prefix later
    let short_id = note
        .as_deref()
        .and_then(|n| n.split_whitespace().nth(1))
        .expect("checkpoint id in note")
        .to_string();

    // mutate the working tree, then restore
    std::fs::write(dir.join("f.txt"), "v2 mutated\n").unwrap();
    std::fs::write(dir.join("extra.txt"), "untracked\n").unwrap();
    let events = settle_answering(
        &mut handle,
        Command::RestoreCheckpoint { id: short_id },
        &|e| matches!(e, Event::Note { .. } | Event::Error { .. }),
        Some(0),
    )
    .await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::Note { message } if message.contains("restored"))),
        "restore notes the restore: {events:?}"
    );
    assert!(
        std::fs::read_to_string(dir.join("f.txt"))
            .unwrap()
            .starts_with("v1"),
        "restore reverts the tracked file to the checkpointed content"
    );
    // git-checkout semantics: tracked files revert, untracked files
    // created after the checkpoint survive (they are the user's)
    assert!(
        dir.join("extra.txt").exists(),
        "restore must not delete untracked files"
    );
    assert!(
        !events.iter().any(|e| matches!(e, Event::Error { .. })),
        "restore must not error: {events:?}"
    );
    // engine checkpoints are working-tree snapshots via the temp git
    // index: the tracked file content returns to the checkpointed state
    // (exact semantics asserted in checkpoint.rs unit tests); here we
    // contract-check the user-visible outcome: the engine reported
    // success and HEAD is untouched.
    let head_after = String::from_utf8_lossy(&git(&["rev-parse", "HEAD"]).stdout).to_string();
    assert_eq!(head_before, head_after, "checkpoints never move HEAD");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Export an empty session refuses; after a turn, the markdown file
/// exists with content.
#[tokio::test]
async fn export_markdown_contract() {
    let dir = tmp_dir("export");
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let mut handle = ka_engine::spawn_with(cfg, test_catalog());

    let events = settle(
        &mut handle,
        Command::ExportMarkdown {
            out: None,
            html: false,
        },
        |e| matches!(e, Event::Note { .. } | Event::Error { .. }),
    )
    .await;
    assert!(
        events.iter().any(
            |e| matches!(e, Event::Error { message, .. } if message.contains("nothing to export"))
        ),
        "{events:?}"
    );

    // a canned turn records a user message; export then works
    let _ = settle(
        &mut handle,
        Command::Prompt {
            allowed_tools: None,
            model: None,
            text: "record me".into(),
            schema: None,
            images: Vec::new(),
        },
        on_idle,
    )
    .await;
    let out = dir.join("export.md");
    let events = settle(
        &mut handle,
        Command::ExportMarkdown {
            out: Some(out.clone()),
            html: false,
        },
        |e| matches!(e, Event::Note { .. } | Event::Error { .. }),
    )
    .await;
    assert!(
        !events.iter().any(|e| matches!(e, Event::Error { .. })),
        "{events:?}"
    );
    let text = std::fs::read_to_string(&out).unwrap();
    assert!(
        text.contains("record me"),
        "prompt lands in the export: {text:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Switching to a fresh strand works and the engine keeps serving.
#[tokio::test]
async fn switch_strand_new_keeps_engine_alive() {
    let dir = tmp_dir("switch");
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let mut handle = ka_engine::spawn_with(cfg, test_catalog());
    let events = settle(
        &mut handle,
        Command::SwitchStrand { id: "new".into() },
        |e| matches!(e, Event::SessionInfo { .. } | Event::Error { .. }),
    )
    .await;
    assert!(
        !events.iter().any(|e| matches!(e, Event::Error { .. })),
        "{events:?}"
    );
    // engine still responsive: another command settles
    let events = settle(&mut handle, Command::ListTasks, |e| {
        matches!(e, Event::Tasks { .. })
    })
    .await;
    assert!(events.iter().any(|e| matches!(e, Event::Tasks { .. })));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Auto-commit off by default: a turn must not create commits.
#[tokio::test]
async fn auto_commit_off_by_default_leaves_repo_untouched() {
    let dir = tmp_dir("no-autocommit");
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(&dir)
            .output()
            .unwrap()
    };
    assert!(git(&["init", "-q"]).status.success());
    git(&["config", "user.email", "t@t"]);
    git(&["config", "user.name", "t"]);
    std::fs::write(dir.join("f.txt"), "x\n").unwrap();
    git(&["add", "."]);
    assert!(git(&["commit", "-m", "init"]).status.success());

    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        ..Default::default()
    };
    let mut handle = ka_engine::spawn_with(cfg, test_catalog());
    let _ = settle(
        &mut handle,
        Command::Prompt {
            allowed_tools: None,
            model: None,
            text: "hello".into(),
            schema: None,
            images: Vec::new(),
        },
        on_idle,
    )
    .await;
    let log = String::from_utf8_lossy(&git(&["log", "--oneline"]).stdout).to_string();
    assert_eq!(
        log.lines().count(),
        1,
        "no ka commit without [git] auto_commit: {log}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ------------------------------------------------- [verify] test loop

/// The [verify] loop end-to-end: a turn that edits files runs the
/// configured test command; a FAILURE is fed back to the model as one
/// automatic fix turn (whose prompt carries the failure output), and no
/// second fix round fires.
#[tokio::test]
async fn verify_failure_feeds_back_exactly_one_fix_round() {
    let dir = tmp_dir("verify-fail");
    std::fs::write(dir.join("v.txt"), "alpha\n").unwrap();
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        // a model is required for the live (scripted) voice; without it
        // the engine runs the canned speaker and skips tools entirely
        model: Some("test/m".into()),
        verify: ka_engine::config::Verify {
            test: Some("echo VERIFY-FAIL-MARKER >&2; exit 3".into()),
            lints: Vec::new(),
        },
        ..Default::default()
    };
    let speaker = Scripted::new(vec![
        ("read", serde_json::json!({"path": "v.txt"})),
        (
            "edit",
            serde_json::json!({"path": "v.txt", "old": "alpha", "new": "ALPHA"}),
        ),
    ]);
    let mut handle = ka_engine::spawn_with_speaker(
        cfg,
        test_catalog(),
        ka_engine::StrandChoice::New,
        ka_dialect::Wire::OpenaiChat,
        speaker.clone(),
    );
    let events = settle(
        &mut handle,
        Command::Prompt {
            allowed_tools: None,
            model: None,
            text: "edit the file".into(),
            schema: None,
            images: Vec::new(),
        },
        on_idle,
    )
    .await;

    let finishes = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                Event::TurnFinished {
                    stop: Stop::Done,
                    ..
                }
            )
        })
        .count();
    assert_eq!(finishes, 2, "edit turn + fix turn: {events:?}");
    assert!(
        events.iter().any(|e| matches!(e, Event::Note { message }
            if message.contains("[verify]") && message.contains("feeding failures back"))),
        "the failure note surfaces: {events:?}"
    );
    // the fix turn's prompt carried the test output
    let fix_prompt = speaker
        .requests
        .lock()
        .iter()
        .rev()
        .find(|msgs| {
            msgs.iter()
                .any(|m| m.role == TurnRole::User && m.content.contains("VERIFY-FAIL-MARKER"))
        })
        .is_some();
    assert!(fix_prompt, "fix round receives the failure output");
    // exactly one fix round: no third cycle, no second [verify] run
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::Note { message } if message.contains("passed"))),
        "{events:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A passing [verify] test is announced and the turn count stays at one.
#[tokio::test]
async fn verify_pass_is_announced_without_fix_round() {
    let dir = tmp_dir("verify-pass");
    std::fs::write(dir.join("v.txt"), "alpha\n").unwrap();
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        // a model is required for the live (scripted) voice; without it
        // the engine runs the canned speaker and skips tools entirely
        model: Some("test/m".into()),
        verify: ka_engine::config::Verify {
            test: Some("true".into()),
            lints: Vec::new(),
        },
        ..Default::default()
    };
    let speaker = Scripted::new(vec![
        ("read", serde_json::json!({"path": "v.txt"})),
        (
            "edit",
            serde_json::json!({"path": "v.txt", "old": "alpha", "new": "ALPHA"}),
        ),
    ]);
    let mut handle = ka_engine::spawn_with_speaker(
        cfg,
        test_catalog(),
        ka_engine::StrandChoice::New,
        ka_dialect::Wire::OpenaiChat,
        speaker,
    );
    let events = settle(
        &mut handle,
        Command::Prompt {
            allowed_tools: None,
            model: None,
            text: "edit the file".into(),
            schema: None,
            images: Vec::new(),
        },
        on_idle,
    )
    .await;
    let finishes = events
        .iter()
        .filter(|e| matches!(e, Event::TurnFinished { .. }))
        .count();
    assert_eq!(finishes, 1, "no fix round on a passing test: {events:?}");
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::Note { message } if message.contains("`true` passed"))),
        "{events:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Abort during a long [verify] test cancels it (no fix round) — the
/// engine stays responsive mid-test.
#[tokio::test]
async fn verify_test_is_abortable() {
    let dir = tmp_dir("verify-abort");
    std::fs::write(dir.join("v.txt"), "alpha\n").unwrap();
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        // a model is required for the live (scripted) voice; without it
        // the engine runs the canned speaker and skips tools entirely
        model: Some("test/m".into()),
        verify: ka_engine::config::Verify {
            test: Some("sleep 30".into()),
            lints: Vec::new(),
        },
        ..Default::default()
    };
    let speaker = Scripted::new(vec![
        ("read", serde_json::json!({"path": "v.txt"})),
        (
            "edit",
            serde_json::json!({"path": "v.txt", "old": "alpha", "new": "ALPHA"}),
        ),
    ]);
    let mut handle = ka_engine::spawn_with_speaker(
        cfg,
        test_catalog(),
        ka_engine::StrandChoice::New,
        ka_dialect::Wire::OpenaiChat,
        speaker,
    );
    handle
        .commands
        .send(Command::Prompt {
            allowed_tools: None,
            model: None,
            text: "edit the file".into(),
            schema: None,
            images: Vec::new(),
        })
        .await
        .expect("engine alive");
    // wait for the edit turn to finish, then abort during the test run
    loop {
        match tokio::time::timeout(Duration::from_secs(15), handle.events.recv()).await {
            Ok(Some(Event::TurnFinished { .. })) => break,
            Ok(Some(_)) => continue,
            Ok(None) => panic!("engine ended before the verify test"),
            Err(_) => panic!("edit turn did not finish within 15s"),
        }
    }
    handle
        .commands
        .send(Command::Abort)
        .await
        .expect("engine alive");
    let events = settle(&mut handle, Command::ListTasks, |e| {
        matches!(e, Event::Note { .. } | Event::Idle)
    })
    .await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::Note { message } if message.contains("cancelled"))),
        "the verify test reports cancellation: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::Note { message } if message.contains("passed"))),
        "a cancelled test must not report passed: {events:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Lifecycle hooks (roadmap 9.2): session_start, user_prompt_submit,
/// pre_compact, stop, and turn_end all fire over the real engine loop
/// with the documented stdin payloads' event names; session_end fires
/// when the command channel closes.
#[tokio::test]
async fn lifecycle_hooks_fire_across_the_engine_loop() {
    let dir = tmp_dir("hooks-lifecycle");
    let hook = |event: &str, name: &str| {
        format!(
            "[[hooks]]\nevent = \"{event}\"\ncommand = \"touch {}\"\n",
            dir.join(format!("{name}.txt")).display()
        )
    };
    let layer = format!(
        "model = \"test/m\"\ncwd = \"{}\"\n{}{}{}{}{}{}",
        dir.display(),
        hook("session_start", "start"),
        hook("user_prompt_submit", "prompt"),
        hook("pre_compact", "compact"),
        hook("stop", "stop"),
        hook("turn_end", "turnend"),
        hook("session_end", "end"),
    );
    let cfg = ka_engine::config::Config::parse_layer(&layer, "test").unwrap();
    let mut handle = ka_engine::spawn_with_speaker(
        cfg,
        test_catalog(),
        ka_engine::StrandChoice::New,
        ka_dialect::Wire::OpenaiChat,
        Scripted::new(vec![]), // text-only turns
    );
    // one prompt cycle fires session_start + user_prompt_submit + stop
    // + turn_end
    settle(
        &mut handle,
        Command::Prompt {
            text: "hi".into(),
            schema: None,
            images: vec![],
            allowed_tools: None,
            model: None,
        },
        on_idle,
    )
    .await;
    for name in ["start", "prompt", "stop", "turnend"] {
        assert!(
            dir.join(format!("{name}.txt")).exists(),
            "{name} hook must fire over the engine loop"
        );
    }
    // a compact fires pre_compact before the digest runs
    settle(&mut handle, Command::Compact { focus: None }, on_idle).await;
    assert!(
        dir.join("compact.txt").exists(),
        "pre_compact hook must fire"
    );
    // dropping the command channel ends the session → session_end
    // (keep draining events so the engine's final sends never block)
    let ka_engine::EngineHandle { commands, events } = handle;
    drop(commands);
    let mut events = events;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !dir.join("end.txt").exists() && std::time::Instant::now() < deadline {
        let _ = tokio::time::timeout(Duration::from_millis(100), events.recv()).await;
    }
    assert!(
        dir.join("end.txt").exists(),
        "session_end hook must fire at shutdown"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// `Command::Prompt`'s per-turn scoping fields are real contract:
/// `allowed_tools` restricts the turn's toolset (a call outside the
/// list is refused with ok:false and no CallOutput), `model` overrides
/// the session selector for that turn (reaching the wire request), an
/// invalid override selector surfaces as a Note, and the scope clears
/// when the turn ends.
#[tokio::test]
async fn prompt_scoping_allowed_tools_and_model_override() {
    let dir = tmp_dir("prompt-scope");
    std::fs::write(dir.join("s.txt"), "content\n").unwrap();
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        model: Some("test/m".into()),
        ..Default::default()
    };
    let speaker = Scripted::new(vec![
        ("read", serde_json::json!({"path": "s.txt"})),
        ("bash", serde_json::json!({"command": "echo nope"})),
    ]);
    let mut handle = ka_engine::spawn_with_speaker(
        cfg,
        test_catalog(),
        ka_engine::StrandChoice::New,
        ka_dialect::Wire::OpenaiChat,
        speaker.clone(),
    );
    let events = settle(
        &mut handle,
        Command::Prompt {
            allowed_tools: Some(vec!["read".into()]),
            model: Some("test/m@high".into()),
            text: "scope this turn".into(),
            schema: None,
            images: Vec::new(),
        },
        on_idle,
    )
    .await;

    // the read call executed (ok:true); the bash call was refused by
    // the allowlist (ok:false, and no CallOutput ever for it)
    let finished: Vec<(String, bool)> = events
        .iter()
        .filter_map(|e| match e {
            Event::CallFinished { tool, ok, .. } => Some((tool.clone(), *ok)),
            _ => None,
        })
        .collect();
    assert!(
        finished.iter().any(|(t, ok)| t == "read" && *ok),
        "read allowed: {finished:?}"
    );
    assert!(
        finished.iter().any(|(t, ok)| t == "bash" && !*ok),
        "bash refused: {finished:?}"
    );
    // the refusal rides the tool result back to the model (the loop
    // gate's no-output shape does not apply to allowlist refusals)
    assert!(
        events.iter().any(
            |e| matches!(e, Event::CallOutput { tool, excerpt, is_error: true, .. }
            if tool == "bash" && excerpt.contains("allowed tools"))
        ),
        "{events:?}"
    );
    // the per-turn model override reached the wire request: the
    // @high suffix is observable on the wire as the request's effort
    // (the model id itself is the same as the session default, so
    // asserting on it alone would be vacuous)
    assert_eq!(
        speaker.efforts.lock().first().cloned().flatten(),
        Some("high".to_string()),
        "the @high override must reach SpeakRequest.effort"
    );
    // the turn still completes cleanly
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::TurnFinished {
                stop: Stop::Done,
                ..
            }
        )),
        "{events:?}"
    );

    // the follow-up turn completes unscoped...
    let events = settle(
        &mut handle,
        Command::Prompt {
            allowed_tools: None,
            model: None,
            text: "next turn unscoped".into(),
            schema: None,
            images: Vec::new(),
        },
        on_idle,
    )
    .await;
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::TurnFinished {
                stop: Stop::Done,
                ..
            }
        )),
        "{events:?}"
    );
    // ...and the offered toolsets prove the scope cleared: the scoped
    // turn's rounds saw exactly the allowlisted tool, the follow-up
    // turn is offered the whole session toolset again
    assert!(
        speaker.efforts.lock().last().is_none_or(|e| e.is_none()),
        "the override must not leak into the next turn"
    );
    let counts = speaker.tool_counts.lock().clone();
    assert_eq!(
        counts.first(),
        Some(&1),
        "scoped turn sees only [read]: {counts:?}"
    );
    assert!(
        counts.last().is_some_and(|c| *c > 1),
        "follow-up turn sees the full toolset: {counts:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A per-turn model override that is not a valid selector is refused
/// with a visible note — never silently dropped.
#[tokio::test]
async fn prompt_model_override_rejects_invalid_selector() {
    let dir = tmp_dir("prompt-bad-selector");
    let cfg = ka_engine::Config {
        cwd: Some(dir.to_string_lossy().into_owned()),
        model: Some("test/m".into()),
        ..Default::default()
    };
    let speaker = Scripted::new(vec![]);
    let mut handle = ka_engine::spawn_with_speaker(
        cfg,
        test_catalog(),
        ka_engine::StrandChoice::New,
        ka_dialect::Wire::OpenaiChat,
        speaker,
    );
    let events = settle(
        &mut handle,
        Command::Prompt {
            allowed_tools: None,
            model: Some("bogus-no-slash".into()),
            text: "hello".into(),
            schema: None,
            images: Vec::new(),
        },
        on_idle,
    )
    .await;
    assert!(
        events.iter().any(
            |e| matches!(e, Event::Note { message } if message.contains("not a valid selector"))
        ),
        "invalid override surfaces a note: {events:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
