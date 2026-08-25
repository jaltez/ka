//! Minimal MCP (Model Context Protocol) stdio client: newline-delimited
//! JSON-RPC 2.0 over a spawned server process. Hand-rolled on purpose —
//! the footprint budget has no room for rmcp, and ka only needs
//! initialize / tools-list / tools-call.
//!
//! Lifecycle: [`McpClient::spawn`] boots the server, handshakes, and
//! lists tools. Failures are per-server and non-fatal: the engine notes
//! them and carries on with the built-in hands.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex, mpsc, oneshot};

/// A configured MCP server (one `[[mcp]]` table in ka.toml).
#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct McpServerConfig {
    /// Tool-name prefix (`<name>.<tool>`).
    pub name: String,
    /// Executable to run.
    pub command: String,
    /// Arguments for the executable.
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment for the process.
    #[serde(default)]
    pub env: HashMap<String, String>,
}

/// One tool advertised by a server.
#[derive(Debug, Clone, PartialEq)]
pub struct McpTool {
    /// Prefixed tool name (`server.tool`).
    pub name: String,
    /// Raw tool name on the server.
    pub raw_name: String,
    /// Human/model-facing description.
    pub description: String,
    /// JSON schema for the arguments object.
    pub schema: Value,
}

/// Connection to one MCP server over stdio.
#[derive(Debug)]
pub struct McpClient {
    server_name: String,
    child: Child,
    stdin: ChildStdin,
    /// Responses routed by id (shared with the reader task).
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    next_id: AtomicU64,
    /// Server-initiated notifications/requests (logged, not handled).
    _noise: mpsc::Receiver<String>,
}

/// Protocol version ka speaks (negotiation tolerates others).
const PROTOCOL_VERSION: &str = "2025-06-18";

impl McpClient {
    /// Spawn the server, run the initialize handshake, and list tools.
    /// One call so callers get a fully-usable client or an error string.
    pub async fn spawn_connect(cfg: &McpServerConfig) -> Result<(Self, Vec<McpTool>), String> {
        let mut child = Command::new(&cfg.command)
            .args(&cfg.args)
            .envs(&cfg.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("spawn {} failed: {e}", cfg.command))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "server stdin unavailable".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "server stdout unavailable".to_string())?;
        let (noise_tx, noise_rx) = mpsc::channel(64);
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        tokio::spawn(read_loop(stdout, pending.clone(), noise_tx));
        let mut client = Self {
            server_name: cfg.name.clone(),
            child,
            stdin,
            pending,
            next_id: AtomicU64::new(1),
            _noise: noise_rx,
        };
        client.initialize().await?;
        let tools = client.list_tools().await?;
        Ok((client, tools))
    }

    /// The configured server name.
    pub fn name(&self) -> &str {
        &self.server_name
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        let line = serde_json::to_string(&msg).map_err(|e| format!("serialize {method}: {e}"))?;
        self.stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| format!("write to server: {e}"))?;
        self.stdin
            .write_all(b"\n")
            .await
            .map_err(|e| format!("write to server: {e}"))?;
        self.stdin
            .flush()
            .await
            .map_err(|e| format!("write to server: {e}"))?;
        let resp = tokio::time::timeout(std::time::Duration::from_secs(30), rx)
            .await
            .map_err(|_| format!("{method}: server timed out"))?
            .map_err(|_| format!("{method}: reader dropped the reply"))?;
        if let Some(err) = resp.get("error") {
            return Err(format!("{method}: {err}"));
        }
        Ok(resp["result"].clone())
    }

    async fn initialize(&mut self) -> Result<(), String> {
        self.request(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "ka", "version": env!("CARGO_PKG_VERSION")},
            }),
        )
        .await?;
        self.notify("notifications/initialized", json!({})).await?;
        Ok(())
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<(), String> {
        let msg = json!({"jsonrpc": "2.0", "method": method, "params": params});
        let line = serde_json::to_string(&msg).map_err(|e| format!("serialize {method}: {e}"))?;
        self.stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| format!("write to server: {e}"))?;
        self.stdin
            .write_all(b"\n")
            .await
            .map_err(|e| format!("write to server: {e}"))?;
        self.stdin
            .flush()
            .await
            .map_err(|e| format!("write to server: {e}"))
    }

    async fn list_tools(&mut self) -> Result<Vec<McpTool>, String> {
        let result = self.request("tools/list", json!({})).await?;
        let tools = result["tools"]
            .as_array()
            .ok_or_else(|| "tools/list: no tools array".to_string())?;
        Ok(tools
            .iter()
            .filter_map(|t| {
                let raw_name = t["name"].as_str()?.to_string();
                let description = t["description"].as_str().unwrap_or("").to_string();
                let schema = t
                    .get("inputSchema")
                    .cloned()
                    .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
                Some(McpTool {
                    name: format!("{}.{}", self.server_name, raw_name),
                    raw_name,
                    description,
                    schema,
                })
            })
            .collect())
    }

    /// Invoke a tool; returns the concatenated text content.
    pub async fn call_tool(&mut self, tool: &str, args: Value) -> Result<String, String> {
        let result = self
            .request(
                "tools/call",
                json!({"name": tool, "arguments": args.as_object().cloned().unwrap_or_default()}),
            )
            .await?;
        if result["isError"].as_bool().unwrap_or(false) {
            return Err(extract_text(&result));
        }
        Ok(extract_text(&result))
    }

    /// Whether the server process is still alive.
    pub async fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

/// Concatenate a tool result's text content parts.
fn extract_text(result: &Value) -> String {
    result["content"]
        .as_array()
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| p["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// Reader task: one JSON-RPC message per line, responses routed by id.
async fn read_loop(
    stdout: ChildStdout,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    noise: mpsc::Sender<String>,
) {
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(id) = msg["id"].as_u64() {
            if let Some(tx) = pending.lock().await.remove(&id) {
                tx.send(msg).ok();
            }
        } else if let Some(method) = msg["method"].as_str() {
            let _ = noise.send(method.to_string()).await;
        }
    }
}

/// Bridge an MCP tool into the hand registry. External tools always pay
/// the exec-tier gate: the engine knows nothing about what they do.
pub struct McpHand {
    tool: McpTool,
    client: Arc<tokio::sync::Mutex<McpClient>>,
}

impl McpHand {
    /// Wrap one advertised tool.
    pub fn new(tool: McpTool, client: Arc<tokio::sync::Mutex<McpClient>>) -> Self {
        Self { tool, client }
    }
}

impl crate::hands::Hand for McpHand {
    fn def(&self) -> crate::hands::HandDef {
        crate::hands::HandDef {
            name: self.tool.name.clone(),
            description: if self.tool.description.is_empty() {
                format!("MCP tool {}", self.tool.name)
            } else {
                self.tool.description.clone()
            },
            parameters: self.tool.schema.clone(),
            clearance: crate::hands::Clearance::Exec,
            read_only: false,
        }
    }

    fn execute<'a>(
        &'a self,
        args: &'a Value,
        _ctx: &'a crate::hands::HandContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::hands::ToolOutput> + Send + 'a>>
    {
        let tool = self.tool.raw_name.clone();
        let args = args.clone();
        let client = self.client.clone();
        Box::pin(async move {
            let mut guard = client.lock().await;
            match guard.call_tool(&tool, args).await {
                Ok(text) if text.trim().is_empty() => {
                    crate::hands::ToolOutput::ok("(empty result)".to_string())
                }
                Ok(text) => crate::hands::ToolOutput::ok(text),
                Err(e) => crate::hands::ToolOutput::err(e),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// A fake MCP stdio server in python: initialize, tools/list (one
    /// echo tool), tools/call returns the arguments as text.
    const FAKE_SERVER: &str = r#"
import json, sys
def send(o): sys.stdout.write(json.dumps(o) + "\n"); sys.stdout.flush()
for line in sys.stdin:
    try: m = json.loads(line)
    except Exception: continue
    if m.get("method") == "initialize":
        send({"jsonrpc":"2.0","id":m["id"],"result":{"protocolVersion":m["params"]["protocolVersion"],"capabilities":{"tools":{}},"serverInfo":{"name":"fake","version":"0"}}})
    elif m.get("method") == "notifications/initialized":
        pass
    elif m.get("method") == "tools/list":
        send({"jsonrpc":"2.0","id":m["id"],"result":{"tools":[{"name":"echo","description":"echo the text","inputSchema":{"type":"object","properties":{"text":{"type":"string"}}}}]}})
    elif m.get("method") == "tools/call":
        if m["params"]["name"] != "echo":
            send({"jsonrpc":"2.0","id":m["id"],"result":{"isError":True,"content":[{"type":"text","text":"unknown tool: " + m["params"]["name"]}]}})
        else:
            text = m["params"]["arguments"].get("text","")
            send({"jsonrpc":"2.0","id":m["id"],"result":{"content":[{"type":"text","text":"echo: " + text}]}})
"#;

    fn python3_available() -> bool {
        std::process::Command::new("python3")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    fn cfg() -> McpServerConfig {
        McpServerConfig {
            name: "fake".to_string(),
            command: "python3".to_string(),
            args: vec!["-c".to_string(), FAKE_SERVER.to_string()],
            env: HashMap::new(),
        }
    }

    #[tokio::test]
    async fn handshake_lists_and_calls_tools() {
        if !python3_available() {
            return;
        }
        let (mut client, tools) = McpClient::spawn_connect(&cfg()).await.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "fake.echo");
        assert!(tools[0].description.contains("echo"));
        let out = client
            .call_tool("echo", json!({"text": "hello"}))
            .await
            .unwrap();
        assert_eq!(out, "echo: hello");
        assert!(client.alive().await);
    }

    #[tokio::test]
    async fn unknown_tool_errors() {
        if !python3_available() {
            return;
        }
        let (mut client, _tools) = McpClient::spawn_connect(&cfg()).await.unwrap();
        let err = client.call_tool("nope", json!({})).await.unwrap_err();
        assert!(err.contains("unknown tool"), "{err}");
    }

    #[tokio::test]
    async fn bad_command_reports_spawn_error() {
        let mut bad = cfg();
        bad.command = "ka-no-such-binary".to_string();
        let err = McpClient::spawn_connect(&bad).await.unwrap_err();
        assert!(err.contains("spawn"), "{err}");
    }

    #[test]
    fn extract_text_joins_parts() {
        let v = json!({"content": [
            {"type":"text","text":"a"},
            {"type":"image","data":"..."},
            {"type":"text","text":"b"},
        ]});
        assert_eq!(extract_text(&v), "a\nb");
    }
}
