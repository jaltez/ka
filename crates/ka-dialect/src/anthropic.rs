//! anthropic-messages wire: `/v1/messages` over SSE.

use serde_json::{Value, json};

use crate::retry::RetryPolicy;
use crate::speaker::{
    SpeakFuture, SpeakRequest, Speaker, SpeakerDelta, SpeakerError, SpeakerEvent, SpeakerStop,
    SpeakerUsage,
};
use crate::sse::{WireConfig, run_wire};

/// A Speaker over the Anthropic Messages API.
#[derive(Debug, Clone)]
pub struct AnthropicMessages {
    /// Shared HTTP client.
    pub client: reqwest::Client,
    /// Base URL (default `https://api.anthropic.com`).
    pub base_url: String,
    /// API key (x-api-key header).
    pub api_key: String,
    /// Model id (`vendor/model` without the vendor? no: full model slug).
    pub model: String,
    /// Max output tokens.
    pub max_output: u32,
    /// First-byte timeout ms (0 = unbounded).
    pub first_byte_timeout_ms: u64,
    /// Retry policy.
    pub policy: RetryPolicy,
}

const ANTHROPIC_VERSION: &str = "2023-06-01";

impl AnthropicMessages {
    fn build_body(&self, request: &SpeakRequest) -> Value {
        let mut body = json!({
            "model": self.model,
            "max_tokens": self.max_output,
            "stream": true,
            "system": request.system,
            "messages": request.messages.iter().map(|m| json!({
                "role": if matches!(m.role, crate::speaker::SpeakRole::Assistant) { "assistant" } else { "user" },
                "content": [{ "type": "text", "text": m.text }],
            })).collect::<Vec<_>>(),
        });
        if let Some(effort) = request.effort {
            if let Some(budget) = thinking_budget(effort) {
                body["thinking"] = json!({ "type": "enabled", "budget_tokens": budget });
            }
        }
        body
    }

    fn parse_event(
        payload: &str,
        state: &mut WireState,
        out: &mut Vec<SpeakerEvent>,
    ) -> Option<()> {
        let v: Value = serde_json::from_str(payload).ok()?;
        match v["type"].as_str().unwrap_or_default() {
            "message_start" => {
                let u = &v["message"]["usage"];
                state.usage.input = u["input_tokens"].as_u64().unwrap_or(0);
                state.usage.cache_read = u["cache_read_input_tokens"].as_u64().unwrap_or(0);
                state.usage.cache_write = u["cache_creation_input_tokens"].as_u64().unwrap_or(0);
            }
            "content_block_start" => {
                let block = &v["content_block"];
                if block["type"].as_str() == Some("tool_use") {
                    let id = block["id"].as_str().unwrap_or_default().to_string();
                    let tool = block["name"].as_str().unwrap_or_default().to_string();
                    state
                        .blocks
                        .insert(v["index"].as_u64().unwrap_or(0), id.clone());
                    out.push(SpeakerEvent::Delta(SpeakerDelta::CallStart { tool, id }));
                }
            }
            "content_block_delta" => {
                let delta = &v["delta"];
                match delta["type"].as_str().unwrap_or_default() {
                    "text_delta" => {
                        let t = delta["text"].as_str().unwrap_or_default().to_string();
                        out.push(SpeakerEvent::Delta(SpeakerDelta::Text(t)));
                    }
                    "thinking_delta" => {
                        let t = delta["thinking"].as_str().unwrap_or_default().to_string();
                        out.push(SpeakerEvent::Delta(SpeakerDelta::Thought(t)));
                    }
                    "input_json_delta" => {
                        if let Some(id) = state.blocks.get(&v["index"].as_u64().unwrap_or(0)) {
                            out.push(SpeakerEvent::Delta(SpeakerDelta::CallArgs {
                                id: id.clone(),
                                chunk: delta["partial_json"]
                                    .as_str()
                                    .unwrap_or_default()
                                    .to_string(),
                            }));
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                if let Some(id) = state.blocks.get(&v["index"].as_u64().unwrap_or(0)).cloned() {
                    out.push(SpeakerEvent::Delta(SpeakerDelta::CallEnd { id }));
                }
            }
            "message_delta" => {
                let stop = match v["delta"]["stop_reason"].as_str() {
                    Some("max_tokens") => SpeakerStop::Length,
                    Some("tool_use") => SpeakerStop::Tools,
                    _ => SpeakerStop::Done,
                };
                let u = &v["usage"];
                if let Some(o) = u["output_tokens"].as_u64() {
                    state.usage.output = o;
                }
                state.stop = Some(stop);
            }
            "message_stop" => {
                state.done = true;
            }
            "error" => {
                let msg = v["error"]["message"]
                    .as_str()
                    .unwrap_or("unknown error")
                    .to_string();
                state.failed = Some(SpeakerError::Protocol(msg));
            }
            _ => {}
        }
        Some(())
    }
}

/// thinking budget per protocol Effort (tokens).
fn thinking_budget(effort: ka_protocol::Effort) -> Option<u64> {
    use ka_protocol::Effort;
    match effort {
        Effort::Off => None,
        Effort::Low => Some(2_048),
        Effort::Medium => Some(8_192),
        Effort::High => Some(16_384),
        Effort::Max => Some(32_000),
    }
}

#[derive(Default)]
struct WireState {
    usage: SpeakerUsage,
    stop: Option<SpeakerStop>,
    done: bool,
    failed: Option<SpeakerError>,
    blocks: std::collections::HashMap<u64, String>,
}

impl Speaker for AnthropicMessages {
    fn converse(&self, request: SpeakRequest) -> SpeakFuture<'_> {
        let this = self.clone();
        let body = this.build_body(&request);
        Box::pin(async move {
            let url = format!("{}/v1/messages", this.base_url.trim_end_matches('/'));
            let cfg = WireConfig {
                url,
                headers: vec![
                    ("x-api-key".to_string(), this.api_key.clone()),
                    (
                        "anthropic-version".to_string(),
                        ANTHROPIC_VERSION.to_string(),
                    ),
                ],
                body,
                first_byte_timeout_ms: this.first_byte_timeout_ms,
                policy: this.policy,
            };
            let parse =
                |payload: &str, state: &mut Option<Box<WireState>>, out: &mut Vec<SpeakerEvent>| {
                    let st = state.get_or_insert_with(Box::default);
                    AnthropicMessages::parse_event(payload, st, out);
                };
            let terminal = |state: Option<Box<WireState>>| match state {
                None => Some(SpeakerEvent::Failed(SpeakerError::Protocol(
                    "empty stream".into(),
                ))),
                Some(st) => {
                    if let Some(e) = st.failed {
                        return Some(SpeakerEvent::Failed(e));
                    }
                    Some(SpeakerEvent::Finished {
                        stop: st.stop.unwrap_or(SpeakerStop::Done),
                        usage: st.usage,
                    })
                }
            };
            run_wire(cfg, parse, terminal).await
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::time::Duration;

    use super::*;
    use crate::speaker::{SpeakMessage, SpeakRole};
    use crate::testkit::{Scripted, TestServer};

    fn speaker(base: &str) -> AnthropicMessages {
        AnthropicMessages {
            client: reqwest::Client::new(),
            base_url: base.to_string(),
            api_key: "test-key".into(),
            model: "claude-sonnet-5".into(),
            max_output: 1024,
            first_byte_timeout_ms: 5_000,
            policy: RetryPolicy {
                base: Duration::from_millis(1),
            },
        }
    }

    async fn collect(mut rx: tokio::sync::mpsc::Receiver<SpeakerEvent>) -> Vec<SpeakerEvent> {
        let mut out = Vec::new();
        while let Some(ev) = rx.recv().await {
            let terminal = matches!(ev, SpeakerEvent::Finished { .. } | SpeakerEvent::Failed(_));
            out.push(ev);
            if terminal {
                break;
            }
        }
        out
    }

    const NORMAL: &str = "\
event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":42,\"cache_read_input_tokens\":100,\"cache_creation_input_tokens\":7}}}\n\n\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\"}}\n\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hel\"}}\n\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"ponder\"}}\n\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"lo\"}}\n\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":5}}\n\n\
data: {\"type\":\"message_stop\"}\n\n";

    #[tokio::test]
    async fn normal_stream_maps_to_events() {
        let server = TestServer::start(vec![Scripted::sse(NORMAL)]).await;
        let sp = speaker(&server.base_url);
        let req = SpeakRequest {
            system: "You are ka.".into(),
            messages: vec![SpeakMessage::new(SpeakRole::User, "hi")],
            effort: Some(ka_protocol::Effort::Medium),
        };
        let rx = sp.converse(req).await.unwrap();
        let events = collect(rx).await;
        assert_eq!(
            events,
            vec![
                SpeakerEvent::Delta(SpeakerDelta::Text("Hel".into())),
                SpeakerEvent::Delta(SpeakerDelta::Thought("ponder".into())),
                SpeakerEvent::Delta(SpeakerDelta::Text("lo".into())),
                SpeakerEvent::Finished {
                    stop: SpeakerStop::Done,
                    usage: SpeakerUsage {
                        input: 42,
                        output: 5,
                        cache_read: 100,
                        cache_write: 7
                    },
                },
            ]
        );
        let reqs = server.requests();
        assert_eq!(reqs[0].request_line, "POST /v1/messages HTTP/1.1");
        assert!(
            reqs[0]
                .headers
                .iter()
                .any(|(k, v)| k == "x-api-key" && v == "test-key")
        );
        let body: Value = serde_json::from_str(&reqs[0].body).unwrap();
        assert_eq!(body["thinking"]["budget_tokens"], 8192);
        assert_eq!(body["stream"], true);
    }

    #[tokio::test]
    async fn overflow_never_retried() {
        let body = r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 210000 tokens > 200000 maximum"}}"#;
        let server = TestServer::start(vec![Scripted::error(400, vec![], body)]).await;
        let sp = speaker(&server.base_url);
        let req = SpeakRequest {
            system: String::new(),
            messages: vec![SpeakMessage::new(SpeakRole::User, "hi")],
            effort: None,
        };
        let rx = sp.converse(req).await.unwrap();
        let events = collect(rx).await;
        assert_eq!(
            events,
            vec![SpeakerEvent::Failed(SpeakerError::Overflow { detail: "{\"type\":\"error\",\"error\":{\"type\":\"invalid_request_error\",\"message\":\"prompt is too long".into() })]
        );
        assert_eq!(server.requests().len(), 1);
    }

    #[tokio::test]
    async fn rate_limit_retries_then_succeeds() {
        let server = TestServer::start(vec![
            Scripted::error(
                429,
                vec![("retry-after".to_string(), "0".to_string())],
                "{\"error\":\"rate limited\"}",
            ),
            Scripted::sse(NORMAL),
        ])
        .await;
        let sp = speaker(&server.base_url);
        let req = SpeakRequest {
            system: String::new(),
            messages: vec![SpeakMessage::new(SpeakRole::User, "hi")],
            effort: None,
        };
        let rx = sp.converse(req).await.unwrap();
        let events = collect(rx).await;
        assert!(matches!(events.last(), Some(SpeakerEvent::Finished { .. })));
        assert_eq!(server.requests().len(), 2);
    }

    #[tokio::test]
    async fn tool_use_stream_maps() {
        let body = "\
data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tu_1\",\"name\":\"read\"}}\n\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\"}}\n\n\
data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"a.rs\\\"}\"}}\n\n\
data: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":9}}\n\n";
        let server = TestServer::start(vec![Scripted::sse(body)]).await;
        let sp = speaker(&server.base_url);
        let req = SpeakRequest {
            system: String::new(),
            messages: vec![SpeakMessage::new(SpeakRole::User, "read it")],
            effort: None,
        };
        let rx = sp.converse(req).await.unwrap();
        let events = collect(rx).await;
        assert_eq!(
            events,
            vec![
                SpeakerEvent::Delta(SpeakerDelta::CallStart {
                    tool: "read".into(),
                    id: "tu_1".into()
                }),
                SpeakerEvent::Delta(SpeakerDelta::CallArgs {
                    id: "tu_1".into(),
                    chunk: "{\"path\":".into()
                }),
                SpeakerEvent::Delta(SpeakerDelta::CallArgs {
                    id: "tu_1".into(),
                    chunk: "\"a.rs\"}".into()
                }),
                SpeakerEvent::Delta(SpeakerDelta::CallEnd { id: "tu_1".into() }),
                SpeakerEvent::Finished {
                    stop: SpeakerStop::Tools,
                    usage: SpeakerUsage {
                        input: 0,
                        output: 9,
                        cache_read: 0,
                        cache_write: 0
                    }
                },
            ]
        );
    }
}
