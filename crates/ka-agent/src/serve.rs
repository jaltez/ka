//! `ka serve`: HTTP/SSE server mode. Hand-rolled HTTP/1.1 on a tokio
//! TcpListener (request line + headers + Content-Length body, one
//! connection per request). Routes:
//!
//! - `GET /health` → `{"ok":true}`
//! - `POST /sessions` → spawn an engine → `{"id":"s1"}`; optional body
//!   `{"resume":"latest"}` or `{"resume":"<strand id/prefix>"}` attaches
//!   to a strand on disk (replayed over SSE)
//! - `POST /sessions/{id}/prompt` `{"text":...,"schema":...}` → queues
//!   the turn → `{"ok":true}`
//! - `GET /sessions/{id}/events` → SSE stream of ka events as
//!   `data:` NDJSON lines, each with an `id:` sequence number;
//!   `Last-Event-ID` re-attaches mid-stream (events before the id are
//!   skipped, a `replay` marker announces the gap)
//!
//! `--token T` enables bearer-token auth on every route; without it the
//! server binds loopback only.

use std::collections::HashMap;
use std::process::ExitCode;
use std::sync::Arc;

use ka_engine::config::Config;
use ka_engine::{StrandChoice, spawn_full};
use ka_protocol::{Command, Event};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::sync::{Mutex, mpsc};

struct Session {
    commands: mpsc::Sender<Command>,
    /// Single-consumer events: taken by the SSE response while streaming.
    events: Mutex<Option<mpsc::Receiver<Event>>>,
}

/// Entry: bind and serve until the process is stopped.
pub async fn run(addr: &str, token: Option<String>) -> Result<ExitCode, String> {
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("bind {addr}: {e}"))?;
    eprintln!("ka serve listening on {addr}");
    serve(listener, token).await;
    Ok(ExitCode::SUCCESS)
}

async fn serve(listener: tokio::net::TcpListener, token: Option<String>) {
    let sessions: Arc<Mutex<HashMap<String, Arc<Session>>>> = Arc::default();
    let mut next_id = 0u64;
    loop {
        let Ok((socket, _)) = listener.accept().await else {
            return;
        };
        next_id += 1;
        tokio::spawn(handle_connection(
            socket,
            Arc::clone(&sessions),
            token.clone(),
            next_id,
        ));
    }
}

struct Request {
    method: String,
    path: String,
    last_event_id: Option<u64>,
    bearer: Option<String>,
    body: Vec<u8>,
}

async fn read_request(socket: &mut tokio::net::TcpStream) -> Option<Request> {
    use tokio::io::AsyncReadExt;
    let mut buf: Vec<u8> = Vec::new();
    let mut tmp = [0u8; 8192];
    loop {
        match socket.read(&mut tmp).await {
            Ok(0) | Err(_) => return None,
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if let Ok(text) = std::str::from_utf8(&buf) {
                    if let Some(head_end) = text.find("\r\n\r\n") {
                        let cl = text
                            .lines()
                            .find_map(|l| {
                                l.strip_prefix("content-length:")
                                    .or_else(|| l.strip_prefix("Content-Length:"))
                                    .and_then(|v| v.trim().parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        if buf.len() >= head_end + 4 + cl {
                            break;
                        }
                    }
                }
            }
        }
    }
    let text = String::from_utf8_lossy(&buf).into_owned();
    let split = text.find("\r\n\r\n")?;
    let head = &text[..split];
    let body_start = split + 4;
    let body = buf[body_start..].to_vec();
    let mut lines = head.lines();
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    let last_event_id = head.lines().find_map(|l| {
        l.strip_prefix("last-event-id:")
            .or_else(|| l.strip_prefix("Last-Event-ID:"))
            .and_then(|v| v.trim().parse::<u64>().ok())
    });
    let bearer = head.lines().find_map(|l| {
        let v = l
            .strip_prefix("authorization: Bearer ")
            .or_else(|| l.strip_prefix("Authorization: Bearer "))?;
        Some(v.trim().to_string())
    });
    Some(Request {
        method,
        path,
        last_event_id,
        bearer,
        body,
    })
}

async fn handle_connection(
    mut socket: tokio::net::TcpStream,
    sessions: Arc<Mutex<HashMap<String, Arc<Session>>>>,
    token: Option<String>,
    _conn_id: u64,
) {
    let Some(req) = read_request(&mut socket).await else {
        return;
    };

    // bearer token gate
    if let Some(expected) = &token {
        if req.bearer.as_deref() != Some(expected.as_str()) {
            respond(&mut socket, 401, r#"{"error":"unauthorized"}"#).await;
            return;
        }
    }

    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/health") => respond(&mut socket, 200, r#"{"ok":true}"#).await,
        ("POST", "/sessions") => {
            // optional body: {"resume": "latest" | "<strand id/prefix>"}
            // (absent = fresh session, today's behavior)
            let resume = std::str::from_utf8(&req.body)
                .ok()
                .and_then(|t| serde_json::from_str::<Value>(t).ok())
                .and_then(|v| {
                    v.get("resume").map(|r| match r.as_str() {
                        Some(s) => Ok(s.to_string()),
                        None => Err("resume must be a string".to_string()),
                    })
                })
                .transpose();
            let resume = match resume {
                Ok(r) => r,
                Err(e) => {
                    respond(&mut socket, 400, &json!({"error": e}).to_string()).await;
                    return;
                }
            };
            let choice = match resume.as_deref() {
                None => StrandChoice::New,
                Some("latest") => StrandChoice::Latest,
                Some(id) => match resolve_strand(id) {
                    Ok(choice) => choice,
                    Err(e) => {
                        respond(&mut socket, 400, &json!({"error": e}).to_string()).await;
                        return;
                    }
                },
            };
            let mut sessions = sessions.lock().await;
            let id = format!("s{}", sessions.len() + 1);
            let ka_engine::EngineHandle { commands, events } =
                spawn_full(Config::default(), ka_dialect::Catalog::embedded(), choice);
            sessions.insert(
                id.clone(),
                Arc::new(Session {
                    commands,
                    events: Mutex::new(Some(events)),
                }),
            );
            let body = json!({"id": id}).to_string();
            respond(&mut socket, 200, &body).await;
        }
        (method, path) if path.starts_with("/sessions/") => {
            let rest = &path["/sessions/".len()..];
            let (id, action) = rest.split_once('/').unwrap_or((rest, ""));
            let session = sessions.lock().await.get(id).cloned();
            let Some(session) = session else {
                respond(&mut socket, 404, r#"{"error":"no such session"}"#).await;
                return;
            };
            match (method, action) {
                ("POST", "prompt") => {
                    let Ok(text) = std::str::from_utf8(&req.body) else {
                        respond(&mut socket, 400, r#"{"error":"bad body"}"#).await;
                        return;
                    };
                    let Ok(v) = serde_json::from_str::<Value>(text) else {
                        respond(&mut socket, 400, r#"{"error":"bad JSON"}"#).await;
                        return;
                    };
                    let prompt = v["text"].as_str().unwrap_or_default().to_string();
                    let schema = v.get("schema").cloned();
                    session
                        .commands
                        .send(Command::Prompt {
                            text: prompt,
                            schema,
                            images: Vec::new(),
                        })
                        .await
                        .ok();
                    respond(&mut socket, 200, r#"{"ok":true}"#).await;
                }
                ("GET", "events") => {
                    stream_events(&mut socket, &session, req.last_event_id).await;
                }
                _ => respond(&mut socket, 404, r#"{"error":"not found"}"#).await,
            }
        }
        _ => respond(&mut socket, 404, r#"{"error":"not found"}"#).await,
    }
}

/// Resolve a strand reference (id or prefix) against the server's cwd,
/// exactly like `ka --session`. `Err` = 400 material.
fn resolve_strand(id: &str) -> Result<StrandChoice, String> {
    let cwd = std::env::current_dir().unwrap_or_default();
    match ka_strand::resolve_id(&cwd, id).map_err(|e| format!("session lookup: {e}"))? {
        ka_strand::IdMatch::Unique(summary) => Ok(StrandChoice::Path(summary.path)),
        ka_strand::IdMatch::None => Err(format!("no session matches '{id}'")),
        ka_strand::IdMatch::Ambiguous(candidates) => {
            let ids: Vec<String> = candidates.iter().map(|c| c.id.clone()).collect();
            Err(format!("ambiguous session '{id}': {}", ids.join(", ")))
        }
    }
}

async fn respond(socket: &mut tokio::net::TcpStream, status: u16, body: &str) {
    let reason = match status {
        200 => "OK",
        401 => "Unauthorized",
        404 => "Not Found",
        _ => "Error",
    };
    let resp = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    socket.write_all(resp.as_bytes()).await.ok();
    socket.shutdown().await.ok();
}

/// Stream session events as SSE until the turn settles (Idle).
async fn stream_events(
    socket: &mut tokio::net::TcpStream,
    session: &Session,
    last_event_id: Option<u64>,
) {
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n";
    socket.write_all(head.as_bytes()).await.ok();
    socket.flush().await.ok();

    let Some(mut events) = session.events.lock().await.take() else {
        let line = sse_line(
            None,
            r#"{"type":"error","message":"event stream already in use"}"#,
        );
        socket.write_all(line.as_bytes()).await.ok();
        return;
    };

    // Last-Event-ID resumes the sequence: earlier events are skipped
    let mut seq: u64 = last_event_id.unwrap_or(0);
    if seq > 0 {
        let marker = sse_line(
            Some(seq),
            &format!(r#"{{"type":"replay","resumed_after":{seq}}}"#),
        );
        socket.write_all(marker.as_bytes()).await.ok();
    }

    while let Some(evt) = events.recv().await {
        seq += 1;
        if seq <= last_event_id.unwrap_or(0) {
            continue;
        }
        let is_idle = matches!(evt, Event::Idle);
        let line = sse_line(Some(seq), &serde_json::to_string(&evt).unwrap_or_default());
        socket.write_all(line.as_bytes()).await.ok();
        socket.flush().await.ok();
        if is_idle {
            break;
        }
    }
}

fn sse_line(id: Option<u64>, data: &str) -> String {
    let id_part = id.map(|i| format!("id: {i}\r\n")).unwrap_or_default();
    format!("{id_part}data: {data}\r\n\r\n")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use tokio::io::AsyncReadExt;

    use tokio::io::AsyncWriteExt;

    async fn http_get(
        addr: std::net::SocketAddr,
        path: &str,
        token: Option<&str>,
    ) -> (u16, String) {
        let mut sock = tokio::net::TcpStream::connect(addr).await.unwrap();
        let auth = token
            .map(|t| format!("Authorization: Bearer {t}\r\n"))
            .unwrap_or_default();
        let req = format!("GET {path} HTTP/1.1\r\nHost: x\r\n{auth}Connection: close\r\n\r\n");
        sock.write_all(req.as_bytes()).await.unwrap();
        let mut buf = Vec::new();
        sock.read_to_end(&mut buf).await.unwrap();
        let text = String::from_utf8_lossy(&buf).into_owned();
        let status: u16 = text
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
        let body = text
            .split("\r\n\r\n")
            .nth(1)
            .unwrap_or_default()
            .to_string();
        (status, body)
    }

    async fn http_post(
        addr: std::net::SocketAddr,
        path: &str,
        token: Option<&str>,
        body: &str,
    ) -> (u16, String) {
        let mut sock = tokio::net::TcpStream::connect(addr).await.unwrap();
        let auth = token
            .map(|t| format!("Authorization: Bearer {t}\r\n"))
            .unwrap_or_default();
        let req = format!(
            "POST {path} HTTP/1.1\r\nHost: x\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        sock.write_all(req.as_bytes()).await.unwrap();
        let mut buf = Vec::new();
        sock.read_to_end(&mut buf).await.unwrap();
        let text = String::from_utf8_lossy(&buf).into_owned();
        let status: u16 = text
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
        let body = text
            .split("\r\n\r\n")
            .nth(1)
            .unwrap_or_default()
            .to_string();
        (status, body)
    }

    #[tokio::test]
    async fn serve_routes_health_sessions_prompt_and_auth() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(super::serve(listener, Some("sekrit".into())));

        // health (token required)
        let (status, _) = http_get(addr, "/health", None).await;
        assert_eq!(status, 401, "missing token must 401");
        let (status, body) = http_get(addr, "/health", Some("sekrit")).await;
        assert_eq!(status, 200);
        assert_eq!(body, r#"{"ok":true}"#);

        // create a session
        let (status, body) = http_post(addr, "/sessions", Some("sekrit"), "{}").await;
        assert_eq!(status, 200);
        assert!(body.contains("\"id\""), "{body}");

        // unknown session 404s
        let (status, _) = http_post(
            addr,
            "/sessions/nope/prompt",
            Some("sekrit"),
            r#"{"text":"hi"}"#,
        )
        .await;
        assert_eq!(status, 404);

        // prompt a real session (canned engine, no model)
        let id: String = serde_json::from_str::<serde_json::Value>(&body).unwrap()["id"]
            .as_str()
            .unwrap()
            .into();
        let (status, body) = http_post(
            addr,
            &format!("/sessions/{id}/prompt"),
            Some("sekrit"),
            r#"{"text":"hello"}"#,
        )
        .await;
        assert_eq!(status, 200, "{body}");

        // events stream (canned reply lands quickly)
        let mut sock = tokio::net::TcpStream::connect(addr).await.unwrap();
        let req = format!(
            "GET /sessions/{id}/events HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer sekrit\r\nConnection: close\r\n\r\n"
        );
        sock.write_all(req.as_bytes()).await.unwrap();
        let mut buf = vec![0u8; 64 * 1024];
        let mut collected = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            tokio::select! {
                r = sock.read(&mut buf) => {
                    match r {
                        Ok(0) | Err(_) => break,
                        Ok(n) => collected.extend_from_slice(&buf[..n]),
                    }
                }
                _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {}
            }
            let text = String::from_utf8_lossy(&collected).into_owned();
            if text.contains("\"type\":\"turn_finished\"") {
                break;
            }
        }
        let text = String::from_utf8_lossy(&collected).into_owned();
        assert!(text.contains("data:"), "{text}");
        assert!(text.contains("turn_finished"), "{text}");
    }
}
