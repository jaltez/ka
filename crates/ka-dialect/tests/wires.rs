//! Wire conformance tests: recorded SSE transcripts replayed through a
//! local socket, exercising the full request/build → stream → decode path
//! with zero provider keys. These are the CI fixture suites Phase 1
//! mandates.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use parking_lot::Mutex;
use std::sync::Arc;

use ka_dialect::dialects::{Catalog, Dialect};
use ka_dialect::speaker::{SpeakRequest, Speaker, StreamEvent, TurnMessage, TurnRole};
use ka_dialect::wire_responses::OpenaiResponses;
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
    let lower = s.to_ascii_lowercase();
    if let Some(cl) = lower
        .lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .and_then(|v| v.trim().parse::<usize>().ok())
    {
        return buf.len() >= pos + 4 + cl;
    }
    // chunked: wait for the terminating zero chunk (or any body at all
    // if the sender never terminates — bounded by the read loop's EOF)
    if lower.lines().any(|l| l.starts_with("transfer-encoding:")) {
        return s.ends_with("0\r\n\r\n");
    }
    // no body intent (GET) or headers only: complete
    true
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
            calls: Vec::new(),
            results: Vec::new(),
        }],
        tools: Vec::new(),
        token: Some("k-test-token".to_string()),
        cache_key: None,
        schema: None,
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

// ---------------------------------------------------------------- responses

const RESPONSES_BASIC: &str = r#"
data: {"type":"response.created","response":{"id":"resp_1"}}

data: {"type":"response.output_item.added","item":{"type":"message","id":"msg_1"}}

data: {"type":"response.output_text.delta","delta":"Hel"}

data: {"type":"response.output_text.delta","delta":"lo ka"}

data: {"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":12,"output_tokens":3,"input_tokens_details":{"cached_tokens":4}}}}

"#;

const RESPONSES_REASONING_AND_TOOL: &str = r#"
data: {"type":"response.output_item.added","item":{"type":"reasoning","id":"rs_1"}}

data: {"type":"response.reasoning_summary_text.delta","delta":"pondering"}

data: {"type":"response.output_item.added","item":{"type":"function_call","id":"fc_1","call_id":"call_9","name":"read","arguments":""}}

data: {"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"{\"pa"}

data: {"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"th\": \"src/x.rs\"}"}

data: {"type":"response.output_item.done","item":{"type":"function_call","id":"fc_1","call_id":"call_9","name":"read","arguments":"{\"path\": \"src/x.rs\"}"}}

data: {"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":40,"output_tokens":18}}}

"#;

const RESPONSES_INCOMPLETE: &str = r#"
data: {"type":"response.output_text.delta","delta":"partial"}

data: {"type":"response.incomplete","response":{"status":"incomplete","usage":{"input_tokens":5,"output_tokens":7}}}

"#;

const RESPONSES_FAILED: &str = r#"
data: {"type":"response.failed","response":{"error":{"code":"server_error","message":"model overloaded"}}}

"#;

#[tokio::test]
async fn responses_basic_text_and_usage() {
    let (addr, captured) = serve_sse(RESPONSES_BASIC).await;
    let dialect = dialect_for("openai_responses", addr, "");
    let events = collect(&OpenaiResponses::new(), request(dialect, "be brief")).await;

    assert_eq!(
        events,
        vec![
            StreamEvent::Text("Hel".into()),
            StreamEvent::Text("lo ka".into()),
            StreamEvent::Finished {
                stop: ka_protocol::Stop::Done,
                // input excludes the cached 4 (billed separately)
                usage: ka_protocol::Usage {
                    input: 8,
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
    assert!(raw.contains("/v1/responses"), "endpoint:\n{raw}");
    assert!(
        raw.contains("\"instructions\":\"be brief\""),
        "system prompt rides instructions:\n{raw}"
    );
    assert!(raw.contains("\"store\":false"), "{raw}");
    // serde_json serializes keys alphabetically: assert per-item, not order
    assert!(raw.contains("\"input\":["), "input array:\n{raw}");
    assert!(raw.contains("\"type\":\"message\""), "{raw}");
    assert!(raw.contains("\"role\":\"user\""), "{raw}");
    assert!(raw.contains("\"content\":\"hi\""), "{raw}");
}

#[tokio::test]
async fn responses_reasoning_streams_and_tool_accumulates() {
    let (addr, captured) = serve_sse(RESPONSES_REASONING_AND_TOOL).await;
    let dialect = dialect_for("openai_responses", addr, "");
    let mut req = request(dialect, "");
    req.effort = Some("medium".to_string());
    let events = collect(&OpenaiResponses::new(), req).await;

    let thought: String = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::Thought(t) => Some(t.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(thought, "pondering");

    let call = events.iter().find_map(|e| match e {
        StreamEvent::Call(c) => Some(c.clone()),
        _ => None,
    });
    let call = call.expect("expected a tool call");
    assert_eq!(call.id, "call_9");
    assert_eq!(call.tool, "read");
    assert_eq!(
        call.arguments.get("path").and_then(|v| v.as_str()),
        Some("src/x.rs")
    );

    let raw = captured.lock().clone().unwrap();
    assert!(
        raw.contains("\"reasoning\":{\"effort\":\"medium\"}"),
        "effort rides reasoning.effort:\n{raw}"
    );
}

#[tokio::test]
async fn responses_incomplete_maps_to_length() {
    let (addr, _cap) = serve_sse(RESPONSES_INCOMPLETE).await;
    let dialect = dialect_for("openai_responses", addr, "");
    let events = collect(&OpenaiResponses::new(), request(dialect, "")).await;
    assert!(matches!(
        events.last(),
        Some(StreamEvent::Finished {
            stop: ka_protocol::Stop::Length,
            ..
        })
    ));
}

#[tokio::test]
async fn responses_failed_event_is_an_error() {
    let (addr, _cap) = serve_sse(RESPONSES_FAILED).await;
    let dialect = dialect_for("openai_responses", addr, "");
    let events = collect(&OpenaiResponses::new(), request(dialect, "")).await;
    assert!(matches!(
        events.last(),
        Some(StreamEvent::Failed {
            class: ka_protocol::ErrorClass::Protocol,
            ..
        })
    ));
}

// ------------------------------------------------------- structured output

fn request_with_schema(mut req: SpeakRequest, schema: serde_json::Value) -> SpeakRequest {
    req.schema = Some(schema);
    req
}

fn answer_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": { "answer": { "type": "integer" } },
        "required": ["answer"],
        "additionalProperties": false
    })
}

#[tokio::test]
async fn openai_structured_output_attaches_response_format() {
    let (addr, captured) = serve_sse(OPENAI_BASIC).await;
    let dialect = dialect_for("openai_chat", addr, "");
    let req = request_with_schema(request(dialect, ""), answer_schema());
    let events = collect(&OpenaiChat::new(), req).await;
    assert!(events.iter().any(|e| matches!(e, StreamEvent::Text(_))));

    let raw = captured.lock().clone().unwrap();
    let body: serde_json::Value = serde_json::from_str(raw.split("\r\n\r\n").nth(1).unwrap())
        .unwrap();
    let rf = &body["response_format"];
    assert_eq!(rf["type"], "json_schema");
    assert_eq!(rf["json_schema"]["name"], "output");
    assert_eq!(rf["json_schema"]["strict"], true);
    assert_eq!(rf["json_schema"]["schema"]["type"], "object");
}

#[tokio::test]
async fn responses_structured_output_attaches_text_format() {
    let (addr, captured) = serve_sse(RESPONSES_BASIC).await;
    let dialect = dialect_for("openai_responses", addr, "");
    let req = request_with_schema(request(dialect, ""), answer_schema());
    let events = collect(&OpenaiResponses::new(), req).await;
    assert!(events.iter().any(|e| matches!(e, StreamEvent::Text(_))));

    let raw = captured.lock().clone().unwrap();
    let body: serde_json::Value = serde_json::from_str(raw.split("\r\n\r\n").nth(1).unwrap())
        .unwrap();
    let fmt = &body["text"]["format"];
    assert_eq!(fmt["type"], "json_schema");
    assert_eq!(fmt["name"], "output");
    assert_eq!(fmt["strict"], true);
    assert_eq!(fmt["schema"]["properties"]["answer"]["type"], "integer");
}

const ANTHROPIC_STRUCTURED: &str = r#"
data: {"type":"message_start","message":{"usage":{"input_tokens":20}}}

data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_s","name":"structured_output","input":{}}}

data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"answer\":"}}

data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"42}"}}

data: {"type":"content_block_stop","index":0}

data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":8}}

data: {"type":"message_stop"}

"#;

#[tokio::test]
async fn anthropic_structured_output_forces_tool_and_parses_args_as_reply() {
    let (addr, captured) = serve_sse(ANTHROPIC_STRUCTURED).await;
    let dialect = dialect_for("anthropic_messages", addr, "");
    let req = request_with_schema(request(dialect, ""), answer_schema());
    let events = collect(&AnthropicMessages::new(), req).await;

    // the forced tool's arguments ARE the reply text
    let text: String = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::Text(t) => Some(t.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(text, r#"{"answer":42}"#);
    assert!(
        !events.iter().any(|e| matches!(e, StreamEvent::Call(_))),
        "structured_output must not surface as a tool call"
    );

    let raw = captured.lock().clone().unwrap();
    let body: serde_json::Value = serde_json::from_str(raw.split("\r\n\r\n").nth(1).unwrap())
        .unwrap();
    let tools = body["tools"].as_array().unwrap();
    let forced = tools
        .iter()
        .find(|t| t["name"] == "structured_output")
        .expect("structured_output tool attached");
    assert_eq!(forced["input_schema"]["type"], "object");
    assert_eq!(
        body["tool_choice"],
        serde_json::json!({ "type": "tool", "name": "structured_output" })
    );
}
