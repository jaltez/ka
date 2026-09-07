//! `ka acp`: Agent Client Protocol server over line-delimited JSON-RPC
//! 2.0 on stdin/stdout. One `spawn_full` engine per session; events map
//! to `session/update` notifications; permission asks surface as
//! `session/request_permission` requests to the client (responses are
//! correlated by request id). Logging goes to stderr only. Bounded
//! method set: initialize, session/new, session/load, session/prompt,
//! session/cancel.

use std::collections::HashMap;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use ka_agent::config::Config;
use ka_agent::{EngineHandle, spawn};
use ka_protocol::{Command, DeltaKind, Event, Stop};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, mpsc, oneshot};

/// Pending client permission responses: rpc request id →
/// (engine ask id, answer channel to the drive task).
type Pending = Arc<Mutex<HashMap<u64, (String, oneshot::Sender<usize>)>>>;

/// Sessions own their engine behind a lock (one turn at a time).
type SharedEngine = Arc<Mutex<EngineHandle>>;

/// Entry: run the ACP loop over stdin/stdout.
pub async fn run() -> Result<ExitCode, String> {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    run_loop(stdin, stdout).await
}

async fn run_loop<R, W>(input: R, mut output: W) -> Result<ExitCode, String>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Value>();
    // writer task: serialized stdout
    tokio::spawn(async move {
        while let Some(v) = out_rx.recv().await {
            let mut line = v.to_string();
            line.push('\n');
            output.write_all(line.as_bytes()).await.ok();
            output.flush().await.ok();
        }
    });

    let sessions: Arc<Mutex<HashMap<String, SharedEngine>>> = Arc::default();
    let pending: Pending = Arc::default();
    let counter = Arc::new(AtomicU64::new(0));

    let mut lines = BufReader::new(input).lines();
    while let Some(line) = lines.next_line().await.map_err(|e| format!("stdin: {e}"))? {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            out_tx
                .send(rpc_error(Value::Null, -32700, "parse error"))
                .ok();
            continue;
        };
        let id = msg.get("id").cloned().unwrap_or(Value::Null);
        let method = msg["method"].as_str().map(str::to_string);

        match method {
            None => {
                // client response: resolve a pending permission request
                let Some(req_id) = id.as_u64() else {
                    continue;
                };
                let option_id = msg["result"]["option"]["optionId"]
                    .as_str()
                    .and_then(|s| s.parse::<usize>().ok())
                    .or_else(|| {
                        msg["result"]["optionId"]
                            .as_str()
                            .and_then(|s| s.parse().ok())
                    })
                    .unwrap_or(1); // default: deny
                let mut pending = pending.lock().await;
                if let Some((_ask_id, tx)) = pending.remove(&req_id) {
                    tx.send(option_id).ok();
                }
            }
            Some(m) if m == "initialize" => {
                out_tx
                    .send(rpc_result(
                        id,
                        json!({
                            "protocolVersion": 1,
                            "agentCapabilities": {"loadSession": true},
                            "authMethods": [],
                        }),
                    ))
                    .ok();
            }
            Some(m) if m == "session/new" => {
                let cwd = msg["params"]["cwd"]
                    .as_str()
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
                let cfg = Config {
                    cwd: Some(cwd.display().to_string()),
                    ..Config::default()
                };
                let handle = Arc::new(Mutex::new(spawn(cfg)));
                let mut sessions = sessions.lock().await;
                let session = format!("s{}", sessions.len() + 1);
                sessions.insert(session.clone(), handle);
                out_tx
                    .send(rpc_result(id, json!({"sessionId": session})))
                    .ok();
            }
            Some(m) if m == "session/load" => {
                let session = msg["params"]["sessionId"].as_str().unwrap_or_default();
                let sessions = sessions.lock().await;
                if sessions.contains_key(session) {
                    out_tx.send(rpc_result(id, json!({}))).ok();
                } else {
                    out_tx
                        .send(rpc_error(
                            id,
                            -32602,
                            format!("unknown session {session:?}"),
                        ))
                        .ok();
                }
            }
            Some(m) if m == "session/prompt" => {
                let session = msg["params"]["sessionId"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                let Some(prompt) = prompt_text(&msg["params"]["prompt"]) else {
                    out_tx
                        .send(rpc_error(id, -32602, "prompt: no text content".to_string()))
                        .ok();
                    continue;
                };
                let sessions = sessions.lock().await;
                let Some(engine) = sessions.get(&session).cloned() else {
                    out_tx
                        .send(rpc_error(
                            id,
                            -32602,
                            format!("unknown session {session:?}"),
                        ))
                        .ok();
                    continue;
                };
                drop(sessions);
                // the drive task owns this request id: it emits
                // session/update notifications and finally the response
                tokio::spawn(drive_turn(
                    engine,
                    session,
                    prompt,
                    id,
                    out_tx.clone(),
                    Arc::clone(&pending),
                    Arc::clone(&counter),
                ));
            }
            Some(m) if m == "session/cancel" => {
                let session = msg["params"]["sessionId"].as_str().unwrap_or_default();
                let sessions = sessions.lock().await;
                if let Some(engine) = sessions.get(session) {
                    let engine = engine.lock().await;
                    engine.commands.send(Command::Abort).await.ok();
                }
            }
            Some(other) => {
                out_tx
                    .send(rpc_error(id, -32601, format!("method not found: {other}")))
                    .ok();
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Drive one engine turn: translate events into session/update
/// notifications and finish with the prompt response. Permission asks
/// surface as `session/request_permission` requests; the main loop
/// resolves them and wakes the drive task through the pending map.
#[allow(clippy::too_many_arguments)]
async fn drive_turn(
    engine: SharedEngine,
    session: String,
    prompt: String,
    request_id: Value,
    out: mpsc::UnboundedSender<Value>,
    pending: Pending,
    counter: Arc<AtomicU64>,
) {
    let mut engine = engine.lock().await;
    let engine = &mut *engine;
    if engine
        .commands
        .send(Command::Prompt {
            text: prompt,
            schema: None,
            images: Vec::new(),
        })
        .await
        .is_err()
    {
        out.send(rpc_error(request_id, -32603, String::from("engine closed")))
            .ok();
        return;
    }
    let stop = loop {
        let Some(evt) = engine.events.recv().await else {
            out.send(rpc_error(
                request_id,
                -32603,
                String::from("engine closed mid-turn"),
            ))
            .ok();
            return;
        };
        match evt {
            Event::Delta { kind } => match kind {
                DeltaKind::Text(t) => {
                    session_update(
                        &out,
                        &session,
                        json!({
                            "sessionUpdate": "agent_message_chunk",
                            "content": {"type": "text", "text": t}
                        }),
                    )
                    .await;
                }
                DeltaKind::Thought(t) => {
                    session_update(
                        &out,
                        &session,
                        json!({
                            "sessionUpdate": "agent_thought_chunk",
                            "content": {"type": "text", "text": t}
                        }),
                    )
                    .await;
                }
                DeltaKind::Call { .. } => {}
            },
            Event::CallStarted { tool, id, detail } => {
                let content = if detail.is_empty() {
                    Vec::<Value>::new()
                } else {
                    vec![json!({
                        "type": "content",
                        "content": {"type": "text", "text": detail}
                    })]
                };
                session_update(
                    &out,
                    &session,
                    json!({
                        "sessionUpdate": "tool_call",
                        "toolCallId": id,
                        "title": tool,
                        "content": content,
                    }),
                )
                .await;
            }
            Event::CallOutput {
                tool,
                id,
                excerpt,
                is_error,
                ..
            } => {
                session_update(
                    &out,
                    &session,
                    json!({
                        "sessionUpdate": "tool_call_update",
                        "toolCallId": id,
                        "title": tool,
                        "status": if is_error { "failed" } else { "completed" },
                        "content": [{
                            "type": "content",
                            "content": {"type": "text", "text": excerpt}
                        }],
                    }),
                )
                .await;
            }
            Event::Ask { id, questions, .. } => {
                let (question_text, options_list) = questions
                    .first()
                    .map(|q| (q.text.clone(), q.options.clone()))
                    .unwrap_or_else(|| (String::new(), Vec::new()));
                let options: Vec<Value> = options_list
                    .iter()
                    .enumerate()
                    .map(|(i, o)| {
                        json!({
                            "optionId": i.to_string(),
                            "name": o,
                            "kind": if i == 0 { "allow_once" } else { "reject_once" }
                        })
                    })
                    .collect();
                let req_id = counter.fetch_add(1, Ordering::Relaxed) + 10_000;
                let (tx, rx) = oneshot::channel();
                pending.lock().await.insert(req_id, (id.0.clone(), tx));
                out.send(json!({
                    "jsonrpc": "2.0",
                    "method": "session/request_permission",
                    "params": {
                        "sessionId": session,
                        "toolCall": {"title": question_text},
                        "options": options,
                    }
                }))
                .ok();
                // the main loop resolves us when the client responds
                // client went away: deny
                let choice = rx.await.unwrap_or(1);
                engine
                    .commands
                    .send(Command::Answer {
                        question: id,
                        choice,
                    })
                    .await
                    .ok();
            }
            Event::TurnFinished { stop, .. } => {
                break match stop {
                    Stop::Done => "end_turn",
                    Stop::Length => "max_tokens",
                    Stop::Aborted => "cancelled",
                    Stop::Error => "refusal",
                };
            }
            _ => {}
        }
    };
    out.send(rpc_result(request_id, json!({"stopReason": stop})))
        .ok();
}

async fn session_update(out: &mpsc::UnboundedSender<Value>, session: &str, update: Value) {
    out.send(json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {"sessionId": session, "update": update}
    }))
    .ok();
}

/// Extract the concatenated text of a prompt content array (or a bare
/// string).
fn prompt_text(v: &Value) -> Option<String> {
    if let Some(text) = v.as_str() {
        return Some(text.to_string());
    }
    let parts = v.as_array()?;
    let mut out = String::new();
    for part in parts {
        if let Some(t) = part["text"].as_str() {
            out.push_str(t);
        }
    }
    (!out.is_empty()).then_some(out)
}

fn rpc_result(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn rpc_error(id: Value, code: i32, message: impl Into<String>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message.into() }
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// Drive the loop over in-memory duplexes: initialize → session/new →
    /// session/prompt; collect output until the prompt response arrives
    /// and assert session/update notifications were streamed.
    #[tokio::test]
    async fn acp_loop_serves_prompt_over_pipe() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let (mut to_server, server_stdin) = tokio::io::duplex(64 * 1024);
        let (mut from_server, server_stdout) = tokio::io::duplex(64 * 1024);

        tokio::spawn(async move {
            let _ = run_loop(server_stdin, server_stdout).await;
        });

        let mut buf = vec![0u8; 64 * 1024];
        let mut collected = String::new();

        // initialize
        to_server
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n")
            .await
            .unwrap();
        to_server.flush().await.unwrap();

        // session/new
        to_server
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"session/new\",\"params\":{}}\n")
            .await
            .unwrap();
        to_server.flush().await.unwrap();

        let session = loop {
            let n = from_server.read(&mut buf).await.unwrap();
            collected.push_str(&String::from_utf8_lossy(&buf[..n]));
            if let Some(line) = collected.lines().find(|l| l.contains("\"sessionId\"")) {
                let v: Value = serde_json::from_str(line).unwrap();
                break v["result"]["sessionId"].as_str().unwrap().to_string();
            }
        };

        // prompt the canned engine (no model configured: canned speaker)
        let prompt = json!({
            "jsonrpc":"2.0","id":3,"method":"session/prompt",
            "params":{"sessionId": session, "prompt": "hello"}
        });
        let mut line = prompt.to_string();
        line.push('\n');
        to_server.write_all(line.as_bytes()).await.unwrap();
        to_server.flush().await.unwrap();

        let (collected, stop) = loop {
            let n = from_server.read(&mut buf).await.unwrap();
            collected.push_str(&String::from_utf8_lossy(&buf[..n]));
            let mut found = None;
            for line in collected.lines() {
                if line.contains("\"id\":3") {
                    if let Ok(v) = serde_json::from_str::<Value>(line) {
                        if let Some(reason) = v["result"]["stopReason"].as_str() {
                            found = Some(reason.to_string());
                            break;
                        }
                    }
                }
            }
            if let Some(stop) = found {
                break (collected.clone(), stop);
            }
        };
        assert!(
            collected.contains("session/update"),
            "updates must stream: {collected}"
        );
        assert!(stop == "end_turn" || stop == "refusal", "{stop}");
    }
}
