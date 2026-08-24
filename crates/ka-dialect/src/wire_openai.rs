//! The openai-chat wire: Chat Completions with SSE streaming, covering
//! OpenAI, Ollama/LM Studio/vLLM compat endpoints, and OpenAI-shaped
//! gateways.

use futures_util::StreamExt;
use ka_protocol::Stop;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::client::{WireError, post_sse};
use crate::json_repair::repair_json;
use crate::speaker::{SpeakRequest, Speaker, StreamEvent, ToolCall, speak_failed};
use crate::sse::SseParser;

/// Speaker for `wire = "openai_chat"` dialects.
#[derive(Debug, Clone)]
pub struct OpenaiChat {
    client: reqwest::Client,
}

impl OpenaiChat {
    /// New speaker with a shared HTTP client (rustls, 10s connect).
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap_or_default();
        Self { client }
    }
}

impl Default for OpenaiChat {
    fn default() -> Self {
        Self::new()
    }
}

impl Speaker for OpenaiChat {
    fn speak<'a>(
        &'a self,
        req: SpeakRequest,
        out: mpsc::Sender<StreamEvent>,
    ) -> crate::speaker::SpeakFuture<'a> {
        Box::pin(async move {
            if let Err(e) = speak_openai(&self.client, &req, &out).await {
                let _ = out.send(speak_failed(e)).await;
            }
        })
    }
}

async fn speak_openai(
    client: &reqwest::Client,
    req: &SpeakRequest,
    out: &mpsc::Sender<StreamEvent>,
) -> Result<(), WireError> {
    let dialect = &req.dialect;
    let Some(base) = dialect.base_url.clone() else {
        return Err(WireError {
            class: ka_protocol::ErrorClass::Protocol,
            retryable: false,
            message: format!("dialect {} has no base_url", req.model_id),
        });
    };
    let url = format!("{}/chat/completions", base.trim_end_matches('/'));

    let mut headers: Vec<(String, String)> =
        vec![("content-type".into(), "application/json".into())];
    if let Some(token) = req.token.clone() {
        headers.push(("authorization".into(), format!("Bearer {token}")));
    }

    let wire_model = dialect.wire_model.clone().unwrap_or_else(|| {
        req.model_id
            .split_once('/')
            .map(|(_, m)| m.to_string())
            .unwrap_or(req.model_id.clone())
    });

    let mut body = json!({
        "model": wire_model,
        "stream": true,
        "stream_options": { "include_usage": true },
    });
    if !req.messages.is_empty() {
        let mut messages = Vec::new();
        if !req.system.is_empty() {
            messages.push(json!({"role": "system", "content": req.system}));
        }
        for m in &req.messages {
            let role = match m.role {
                crate::speaker::TurnRole::User => "user",
                crate::speaker::TurnRole::Assistant => "assistant",
            };
            messages.push(json!({"role": role, "content": m.content}));
        }
        body["messages"] = Value::Array(messages);
    }
    let max_field = dialect
        .flags
        .max_tokens_field
        .clone()
        .unwrap_or_else(|| "max_tokens".into());
    if dialect.max_output > 0 {
        body[max_field.as_str()] = json!(dialect.max_output);
    }
    if let Some(effort) = req.effort.clone() {
        if dialect.flags.reasoning_field.is_some() {
            body["reasoning_effort"] = json!(effort);
        }
    }

    let resp = post_sse(
        client,
        &url,
        &headers,
        serde_json::to_string(&body).unwrap_or_else(|_| "{}".into()),
        dialect.first_byte_timeout_ms,
        3,
    )
    .await?;

    let mut parser = SseParser::new();
    let mut stream = resp.bytes_stream();
    let mut usage = ka_protocol::Usage::default();
    let mut stop = Stop::Done;
    // openai streams tool_calls by index; we accumulate per index and emit
    // complete Call events at stream end (or index switch).
    let mut calls: Vec<PendingCall> = Vec::new();
    let mut finished = false;

    loop {
        let item =
            match tokio::time::timeout(std::time::Duration::from_secs(300), stream.next()).await {
                Ok(item) => item,
                Err(_) => {
                    return Err(WireError::network(
                        "idle timeout: 300s without a stream chunk",
                    ));
                }
            };
        let Some(chunk) = item else {
            break;
        };
        let chunk = chunk.map_err(|e| WireError::network(e.to_string()))?;
        for evt in parser.feed(&chunk) {
            if evt.data.trim() == "[DONE]" {
                finished = true;
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(&evt.data) else {
                continue; // malformed chunk: drop, keep streaming
            };
            if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
                usage.input = u
                    .get("prompt_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(usage.input);
                usage.output = u
                    .get("completion_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(usage.output);
                let cached = u
                    .get("prompt_tokens_details")
                    .and_then(|d| d.get("cached_tokens"))
                    .and_then(Value::as_u64)
                    .or_else(|| u.get("cached_tokens").and_then(Value::as_u64))
                    .unwrap_or(0);
                usage.cache_read = cached;
            }
            let Some(choice) = v.pointer("/choices/0") else {
                continue;
            };
            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                stop = match reason {
                    "length" => Stop::Length,
                    _ => Stop::Done,
                };
            }
            let Some(delta) = choice.get("delta") else {
                continue;
            };
            if let Some(text) = delta.get("content").and_then(Value::as_str) {
                if !text.is_empty() {
                    out.send(StreamEvent::Text(text.to_string())).await.ok();
                }
            }
            for key in ["reasoning_content", "reasoning"] {
                if let Some(thought) = delta.get(key).and_then(Value::as_str) {
                    if !thought.is_empty() {
                        out.send(StreamEvent::Thought(thought.to_string()))
                            .await
                            .ok();
                    }
                    break;
                }
            }
            if let Some(tc) = delta.get("tool_calls").and_then(Value::as_array) {
                for frag in tc {
                    ingest_tool_fragment(&mut calls, frag);
                }
            }
        }
    }
    for evt in parser.finish() {
        // trailing frame without [DONE]: treat identically
        if let Ok(v) = serde_json::from_str::<Value>(&evt.data) {
            if let Some(choice) = v.pointer("/choices/0") {
                if let Some(delta) = choice.get("delta") {
                    if let Some(text) = delta.get("content").and_then(Value::as_str) {
                        out.send(StreamEvent::Text(text.to_string())).await.ok();
                    }
                }
            }
        }
    }

    // Emit complete calls.
    for pending in calls {
        if pending.tool.is_empty() {
            continue;
        }
        out.send(StreamEvent::Call(ToolCall {
            id: pending.id,
            tool: pending.tool,
            arguments: repair_json(&pending.args),
        }))
        .await
        .ok();
    }
    let _ = finished;
    out.send(StreamEvent::Finished { stop, usage }).await.ok();
    Ok(())
}

#[derive(Debug, Default)]
struct PendingCall {
    id: String,
    tool: String,
    args: String,
}

fn ingest_tool_fragment(calls: &mut Vec<PendingCall>, frag: &Value) {
    let index = frag.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
    while calls.len() <= index {
        calls.push(PendingCall::default());
    }
    let pending = &mut calls[index];
    if let Some(id) = frag.get("id").and_then(Value::as_str) {
        pending.id = id.to_string();
    }
    if let Some(name) = frag.pointer("/function/name").and_then(Value::as_str) {
        pending.tool = name.to_string();
    }
    if let Some(args) = frag.pointer("/function/arguments").and_then(Value::as_str) {
        pending.args.push_str(args);
    }
}
