//! openai-chat wire: `/chat/completions` over SSE — also the wire for every
//! OpenAI-compatible endpoint (Ollama, LM Studio, vLLM, gateways).

use std::collections::HashMap;

use serde_json::{Value, json};

use crate::retry::RetryPolicy;
use crate::speaker::{
    SpeakFuture, SpeakRequest, Speaker, SpeakerDelta, SpeakerError, SpeakerEvent, SpeakerStop,
    SpeakerUsage,
};
use crate::sse::{WireConfig, run_wire};

/// A Speaker over OpenAI Chat Completions (or compatible endpoint).
#[derive(Debug, Clone)]
pub struct OpenaiChat {
    /// Shared HTTP client.
    pub client: reqwest::Client,
    /// Base URL **including** `/v1` (default `https://api.openai.com/v1`;
    /// Ollama: `http://localhost:11434/v1`).
    pub base_url: String,
    /// Bearer token (`None` for keyless local endpoints).
    pub api_key: Option<String>,
    /// Model id as the endpoint expects it.
    pub model: String,
    /// Max output tokens.
    pub max_output: u32,
    /// Effort levels the endpoint supports (from dialect `efforts`).
    pub supported_efforts: Vec<String>,
    /// First-byte timeout ms (0 = unbounded).
    pub first_byte_timeout_ms: u64,
    /// Retry policy.
    pub policy: RetryPolicy,
}

impl OpenaiChat {
    fn build_body(&self, request: &SpeakRequest) -> Value {
        let mut messages = vec![json!({ "role": "system", "content": request.system })];
        for m in &request.messages {
            messages.push(json!({
                "role": if matches!(m.role, crate::speaker::SpeakRole::Assistant) { "assistant" } else { "user" },
                "content": m.text,
            }));
        }
        let mut body = json!({
            "model": self.model,
            "messages": messages,
            "max_tokens": self.max_output,
            "stream": true,
            "stream_options": { "include_usage": true },
        });
        if let Some(effort) = request.effort {
            if let Some(name) = resolve_effort(effort, &self.supported_efforts) {
                body["reasoning_effort"] = json!(name);
            }
        }
        body
    }

    fn parse_event(payload: &str, state: &mut WireState, out: &mut Vec<SpeakerEvent>) {
        let Ok(v) = serde_json::from_str::<Value>(payload) else {
            return;
        };
        if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
            state.usage.input = u["prompt_tokens"].as_u64().unwrap_or(0);
            state.usage.output = u["completion_tokens"].as_u64().unwrap_or(0);
            state.usage.cache_read = u["prompt_tokens_details"]["cached_tokens"]
                .as_u64()
                .unwrap_or(0);
            // OpenAI bills cached tokens at a discount; report input without
            // them so cost math can price both classes.
            state.usage.input = state.usage.input.saturating_sub(state.usage.cache_read);
        }
        let Some(choice) = v["choices"].get(0) else {
            return;
        };
        let delta = &choice["delta"];
        if let Some(reasoning) = delta["reasoning_content"].as_str() {
            if !reasoning.is_empty() {
                out.push(SpeakerEvent::Delta(SpeakerDelta::Thought(
                    reasoning.to_string(),
                )));
            }
        }
        if let Some(text) = delta["content"].as_str() {
            if !text.is_empty() {
                out.push(SpeakerEvent::Delta(SpeakerDelta::Text(text.to_string())));
            }
        }
        if let Some(calls) = delta["tool_calls"].as_array() {
            for call in calls {
                let idx = call["index"].as_u64().unwrap_or(0);
                let func = &call["function"];
                if let (Some(id), Some(name)) = (call["id"].as_str(), func["name"].as_str()) {
                    let id = id.to_string();
                    state.calls.insert(idx, id.clone());
                    out.push(SpeakerEvent::Delta(SpeakerDelta::CallStart {
                        tool: name.to_string(),
                        id,
                    }));
                }
                if let Some(args) = func["arguments"].as_str() {
                    if !args.is_empty() {
                        if let Some(id) = state.calls.get(&idx) {
                            out.push(SpeakerEvent::Delta(SpeakerDelta::CallArgs {
                                id: id.clone(),
                                chunk: args.to_string(),
                            }));
                        }
                    }
                }
            }
        }
        if let Some(finish) = choice["finish_reason"].as_str() {
            for (_, id) in state.calls.drain() {
                out.push(SpeakerEvent::Delta(SpeakerDelta::CallEnd { id }));
            }
            state.stop = Some(match finish {
                "length" => SpeakerStop::Length,
                "tool_calls" => SpeakerStop::Tools,
                _ => SpeakerStop::Done,
            });
        }
    }
}

/// Map protocol effort to the endpoint's supported value: exact name match,
/// else the highest supported level, else None (omit the parameter).
fn resolve_effort(effort: ka_protocol::Effort, supported: &[String]) -> Option<String> {
    use ka_protocol::Effort;
    if supported.is_empty() {
        return None;
    }
    let want = match effort {
        Effort::Off => return None,
        Effort::Low => "low",
        Effort::Medium => "medium",
        Effort::High => "high",
        Effort::Max => "max",
    };
    if supported.iter().any(|s| s == want) {
        return Some(want.to_string());
    }
    // highest known tier available
    for tier in ["max", "xhigh", "high", "medium", "low"] {
        if supported.iter().any(|s| s == tier) {
            return Some(tier.to_string());
        }
    }
    supported.last().cloned()
}

#[derive(Default)]
struct WireState {
    usage: SpeakerUsage,
    stop: Option<SpeakerStop>,
    calls: HashMap<u64, String>,
}

impl Speaker for OpenaiChat {
    fn converse(&self, request: SpeakRequest) -> SpeakFuture<'_> {
        let this = self.clone();
        let body = this.build_body(&request);
        Box::pin(async move {
            let url = format!("{}/chat/completions", this.base_url.trim_end_matches('/'));
            let mut headers = vec![];
            if let Some(key) = &this.api_key {
                headers.push(("authorization".to_string(), format!("Bearer {key}")));
            }
            let cfg = WireConfig {
                url,
                headers,
                body,
                first_byte_timeout_ms: this.first_byte_timeout_ms,
                policy: this.policy,
            };
            let parse =
                |payload: &str, state: &mut Option<Box<WireState>>, out: &mut Vec<SpeakerEvent>| {
                    let st = state.get_or_insert_with(Box::default);
                    OpenaiChat::parse_event(payload, st, out);
                };
            let terminal = |state: Option<Box<WireState>>| match state {
                None => Some(SpeakerEvent::Failed(SpeakerError::Protocol(
                    "empty stream".into(),
                ))),
                Some(st) => Some(SpeakerEvent::Finished {
                    stop: st.stop.unwrap_or(SpeakerStop::Done),
                    usage: st.usage,
                }),
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

    fn speaker(base: &str) -> OpenaiChat {
        OpenaiChat {
            client: reqwest::Client::new(),
            base_url: format!("{base}/v1"),
            api_key: Some("test-key".into()),
            model: "gpt-5.1".into(),
            max_output: 1024,
            supported_efforts: vec!["low".into(), "medium".into(), "high".into()],
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
data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"thinking...\"}}]}\n\n\
data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n\
data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\n\
data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
data: {\"choices\":[],\"usage\":{\"prompt_tokens\":210,\"completion_tokens\":4,\"prompt_tokens_details\":{\"cached_tokens\":200}}}\n\n\
data: [DONE]\n\n";

    #[tokio::test]
    async fn normal_stream_maps_to_events() {
        let server = TestServer::start(vec![Scripted::sse(NORMAL)]).await;
        let sp = speaker(&server.base_url);
        let req = SpeakRequest {
            system: "You are ka.".into(),
            messages: vec![SpeakMessage::new(SpeakRole::User, "hi")],
            effort: Some(ka_protocol::Effort::Max),
        };
        let rx = sp.converse(req).await.unwrap();
        let events = collect(rx).await;
        assert_eq!(
            events,
            vec![
                SpeakerEvent::Delta(SpeakerDelta::Thought("thinking...".into())),
                SpeakerEvent::Delta(SpeakerDelta::Text("Hel".into())),
                SpeakerEvent::Delta(SpeakerDelta::Text("lo".into())),
                SpeakerEvent::Finished {
                    stop: SpeakerStop::Done,
                    // cached split out: 210 - 200 = 10 fresh input
                    usage: SpeakerUsage {
                        input: 10,
                        output: 4,
                        cache_read: 200,
                        cache_write: 0
                    },
                },
            ]
        );
        let reqs = server.requests();
        assert_eq!(reqs[0].request_line, "POST /v1/chat/completions HTTP/1.1");
        let body: Value = serde_json::from_str(&reqs[0].body).unwrap();
        // Max clamps to highest supported = high
        assert_eq!(body["reasoning_effort"], "high");
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert!(
            reqs[0]
                .headers
                .iter()
                .any(|(k, v)| k == "authorization" && v == "Bearer test-key")
        );
    }

    #[tokio::test]
    async fn context_overflow_detected() {
        let body = r#"{"error":{"message":"This model maximum context length is 400000 tokens. However, you requested 410000","code":"context_length_exceeded"}}"#;
        let server = TestServer::start(vec![Scripted::error(400, vec![], body)]).await;
        let sp = speaker(&server.base_url);
        let req = SpeakRequest {
            system: String::new(),
            messages: vec![SpeakMessage::new(SpeakRole::User, "big")],
            effort: None,
        };
        let err = sp
            .converse(req)
            .await
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(err.contains("context"), "got: {err}");
        assert_eq!(server.requests().len(), 1);
    }

    #[tokio::test]
    async fn tool_call_fragments_accumulate() {
        let body = "\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"edit\",\"arguments\":\"\"}}]}}]}\n\n\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"path\\\":\"}}]}}]}\n\n\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"a.rs\\\"}\"}}]}}]}\n\n\
data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
data: [DONE]\n\n";
        let server = TestServer::start(vec![Scripted::sse(body)]).await;
        let sp = speaker(&server.base_url);
        let req = SpeakRequest {
            system: String::new(),
            messages: vec![SpeakMessage::new(SpeakRole::User, "edit it")],
            effort: None,
        };
        let rx = sp.converse(req).await.unwrap();
        let events = collect(rx).await;
        assert_eq!(
            events,
            vec![
                SpeakerEvent::Delta(SpeakerDelta::CallStart {
                    tool: "edit".into(),
                    id: "call_1".into()
                }),
                SpeakerEvent::Delta(SpeakerDelta::CallArgs {
                    id: "call_1".into(),
                    chunk: "{\"path\":".into()
                }),
                SpeakerEvent::Delta(SpeakerDelta::CallArgs {
                    id: "call_1".into(),
                    chunk: "\"a.rs\"}".into()
                }),
                SpeakerEvent::Delta(SpeakerDelta::CallEnd {
                    id: "call_1".into()
                }),
                SpeakerEvent::Finished {
                    stop: SpeakerStop::Tools,
                    usage: SpeakerUsage::default()
                },
            ]
        );
    }

    #[test]
    fn effort_resolution() {
        let s: Vec<String> = vec!["low".into(), "medium".into(), "high".into()];
        assert_eq!(
            resolve_effort(ka_protocol::Effort::Medium, &s).as_deref(),
            Some("medium")
        );
        assert_eq!(
            resolve_effort(ka_protocol::Effort::Max, &s).as_deref(),
            Some("high")
        );
        assert_eq!(resolve_effort(ka_protocol::Effort::Off, &s), None);
        assert_eq!(resolve_effort(ka_protocol::Effort::High, &[]), None);
    }
}
