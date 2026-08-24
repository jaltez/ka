//! Wire conformance tests: recorded SSE transcripts replayed through a
//! local socket, exercising the full request/build → stream → decode path
//! with zero provider keys. These are the CI fixture suites Phase 1
//! mandates.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use parking_lot::Mutex;
use std::sync::Arc;

use ka_dialect::dialects::{Catalog, Dialect};
use ka_dialect::speaker::{SpeakRequest, Speaker, StreamEvent, TurnMessage, TurnRole};
use ka_dialect::{AnthropicMessages, OpenaiChat};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// ---------------------------------------------------------------- fixtures

const OPENAI_BASIC: &str = r#"
data: {"choices":[{"delta":{"role":"assistant","content":"Hel"}}]}

data: {"choices":[{"delta":{"content":"lo ka"}}]}

data: {"choices":[{"delta":{},"finish_reason":"stop"}]}

data: {"choices":[],"usage":{"prompt_tokens":12,"completion_tokens":3,"prompt_tokens_details":{"cached_tokens":4}}}

data: [DONE]

"#;

const OPENAI_TOOLS: &str = r#"
data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read","arguments":""}}]}}]}

data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"pa"}}]}}]}

data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\": \"src/x.rs\"}"}}]}}]}

data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}

data: {"choices":[],"usage":{"prompt_tokens":9,"completion_tokens":18}}

data: [DONE]

"#;

const OPENAI_MALFORMED_CHUNK: &str = r#"
data: {"choices":[{"delta":{"content":"good"}}]}

data: !!not json!!

data: {"choices":[{"delta":{"content":" after"}}]}

data: {"choices":[{"delta":{},"finish_reason":"stop"}]}

data: [DONE]

"#;

const ANTHROPIC_BASIC: &str = r#"
event: message_start
data: {"type":"message_start","message":{"usage":{"input_tokens":25,"cache_read_input_tokens":10,"cache_creation_input_tokens":5}}}

event: ping
data: {"type":"ping"}

data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hel"}}

data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"lo"}}

data: {"type":"content_block_stop","index":0}

data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}

data: {"type":"message_stop"}

"#;

const ANTHROPIC_THINKING_TOOL: &str = r#"
data: {"type":"message_start","message":{"usage":{"input_tokens":40}}}

data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}

data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"pondering"}}

data: {"type":"content_block_stop","index":0}

data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"edit","input":{}}}

data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\": \"a.rs\","}}

data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"old\": \"x\""}}

data: {"type":"content_block_stop","index":1}

data: {"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":30}}

data: {"type":"message_stop"}

"#;

// ---------------------------------------------------------------- helpers

/// Serve one canned SSE response; returns the bound address plus the
/// captured raw request (headers + body).
async fn serve_sse(response: &'static str) -> (std::net::SocketAddr, Arc<Mutex<Option<String>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let captured: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let cap = captured.clone();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf: Vec<u8> = Vec::new();
        let mut tmp = [0u8; 8192];
        loop {
            let n = sock.read(&mut tmp).await.unwrap();
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
            if request_complete(&buf) {
                break;
            }
        }
        *cap.lock() = Some(String::from_utf8_lossy(&buf).into_owned());
        let http = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{response}"
        );
        sock.write_all(http.as_bytes()).await.unwrap();
        sock.shutdown().await.ok();
    });
    (addr, captured)
}

fn request_complete(buf: &[u8]) -> bool {
    let Ok(s) = std::str::from_utf8(buf) else {
        return false;
    };
    let Some(pos) = s.find("\r\n\r\n") else {
        return false;
    };
    let cl = s
        .to_ascii_lowercase()
        .lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(0);
    buf.len() >= pos + 4 + cl
}

fn dialect_for(wire: &str, addr: std::net::SocketAddr, extra: &str) -> Dialect {
    let text = format!(
        "[dialects.\"test/m\"]\nwire = \"{wire}\"\nbase_url = \"http://{addr}/v1\"\ncontext = 100000\n{extra}"
    );
    let catalog = Catalog::parse(&text).unwrap();
    catalog.get("test/m").cloned().unwrap()
}

fn request(dialect: Dialect, system: &str) -> SpeakRequest {
    SpeakRequest {
        model_id: "test/m".to_string(),
        dialect,
        effort: None,
        system: system.to_string(),
        messages: vec![TurnMessage {
            role: TurnRole::User,
            content: "hi".to_string(),
        }],
        token: Some("k-test-token".to_string()),
        cache_key: None,
    }
}

async fn collect(speaker: &dyn Speaker, req: SpeakRequest) -> Vec<StreamEvent> {
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    speaker.speak(req, tx).await;
    let mut out = Vec::new();
    while let Ok(evt) = rx.try_recv() {
        out.push(evt);
    }
    out
}

// ---------------------------------------------------------------- openai

#[tokio::test]
async fn openai_basic_stream_text_and_usage() {
    let (addr, captured) = serve_sse(OPENAI_BASIC).await;
    let dialect = dialect_for("openai_chat", addr, "");
    let events = collect(&OpenaiChat::new(), request(dialect, "be brief")).await;

    assert_eq!(
        events,
        vec![
            StreamEvent::Text("Hel".into()),
            StreamEvent::Text("lo ka".into()),
            StreamEvent::Finished {
                stop: ka_protocol::Stop::Done,
                usage: ka_protocol::Usage {
                    input: 12,
                    output: 3,
                    cache_read: 4,
                    cache_write: 0,
                    cost: 0.0,
                },
            },
        ]
    );

    let raw = captured.lock().clone().unwrap();
    assert!(
        raw.contains("authorization: Bearer k-test-token"),
        "auth header missing:\n{raw}"
    );
    assert!(
        raw.contains("\"model\":\"m\""),
        "wire model should strip vendor:\n{raw}"
    );
    assert!(raw.contains("\"stream\":true"), "{raw}");
    assert!(
        raw.contains("\"role\":\"system\""),
        "system role expected:\n{raw}"
    );
    assert!(raw.contains("be brief"), "{raw}");
}

#[tokio::test]
async fn openai_tool_call_fragments_accumulate() {
    let (addr, _cap) = serve_sse(OPENAI_TOOLS).await;
    let dialect = dialect_for("openai_chat", addr, "");
    let events = collect(&OpenaiChat::new(), request(dialect, "")).await;

    let call = events.iter().find_map(|e| match e {
        StreamEvent::Call(c) => Some(c.clone()),
        _ => None,
    });
    let call = call.expect("expected a tool call");
    assert_eq!(call.id, "call_1");
    assert_eq!(call.tool, "read");
    assert_eq!(
        call.arguments.get("path").and_then(|v| v.as_str()),
        Some("src/x.rs")
    );
    assert!(matches!(
        events.last(),
        Some(StreamEvent::Finished {
            stop: ka_protocol::Stop::Done,
            ..
        })
    ));
}

#[tokio::test]
async fn openai_malformed_chunk_is_dropped_not_fatal() {
    let (addr, _cap) = serve_sse(OPENAI_MALFORMED_CHUNK).await;
    let dialect = dialect_for("openai_chat", addr, "");
    let events = collect(&OpenaiChat::new(), request(dialect, "")).await;
    let text: String = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "good after");
}

// ---------------------------------------------------------------- anthropic

#[tokio::test]
async fn anthropic_basic_stream_blocks_and_usage() {
    let (addr, captured) = serve_sse(ANTHROPIC_BASIC).await;
    let dialect = dialect_for("anthropic_messages", addr, "cache = \"control\"");
    let events = collect(&AnthropicMessages::new(), request(dialect, "sys prompt")).await;

    assert_eq!(
        events,
        vec![
            StreamEvent::Text("Hel".into()),
            StreamEvent::Text("lo".into()),
            StreamEvent::Finished {
                stop: ka_protocol::Stop::Done,
                usage: ka_protocol::Usage {
                    input: 25,
                    output: 2,
                    cache_read: 10,
                    cache_write: 5,
                    cost: 0.0,
                },
            },
        ]
    );

    let raw = captured.lock().clone().unwrap();
    assert!(raw.contains("x-api-key: k-test-token"), "{raw}");
    assert!(raw.contains("anthropic-version:"), "{raw}");
    assert!(raw.contains("/v1/messages"), "{raw}");
    assert!(
        raw.contains("cache_control"),
        "cache=control must add breakpoint:\n{raw}"
    );
    assert!(raw.contains("sys prompt"), "{raw}");
    assert!(
        raw.contains("\"max_tokens\":"),
        "anthropic requires max_tokens:\n{raw}"
    );
}

#[tokio::test]
async fn anthropic_thinking_and_truncated_tool_args_repaired() {
    let (addr, _cap) = serve_sse(ANTHROPIC_THINKING_TOOL).await;
    let dialect = dialect_for("anthropic_messages", addr, "");
    let events = collect(&AnthropicMessages::new(), request(dialect, "")).await;

    assert!(
        events
            .iter()
            .any(|e| matches!(e, StreamEvent::Thought(t) if t == "pondering"))
    );
    let call = events.iter().find_map(|e| match e {
        StreamEvent::Call(c) => Some(c.clone()),
        _ => None,
    });
    let call = call.expect("expected repaired tool call");
    assert_eq!(call.id, "toolu_1");
    assert_eq!(call.tool, "edit");
    // streamed args were truncated mid-string: {"path": "a.rs","old": "x"
    assert_eq!(
        call.arguments.get("path").and_then(|v| v.as_str()),
        Some("a.rs")
    );
    assert_eq!(
        call.arguments.get("old").and_then(|v| v.as_str()),
        Some("x")
    );
}

#[tokio::test]
async fn anthropic_length_stop_maps_to_length() {
    let body = concat!(
        "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":5}}}\n\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"partial\"}}\n\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"max_tokens\"},\"usage\":{\"output_tokens\":9}}\n\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    let (addr, _cap) = serve_sse(body).await;
    let dialect = dialect_for("anthropic_messages", addr, "");
    let events = collect(&AnthropicMessages::new(), request(dialect, "")).await;
    assert!(matches!(
        events.last(),
        Some(StreamEvent::Finished {
            stop: ka_protocol::Stop::Length,
            ..
        })
    ));
}
