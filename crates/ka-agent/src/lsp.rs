//! Opt-in language-server diagnostics: a hand-rolled LSP client over
//! stdio (JSON-RPC with `Content-Length` framing — no new dependency,
//! same precedent as the MCP client). Only diagnostics; no completions,
//! no hover, no workspace features.
//!
//! Contract (the opencode #9102 lesson): diagnostics are informational
//! context appended to a *successful* edit/write tool result. They never
//! affect tool success, clearance verdicts, or exit codes. Rendering is
//! capped (20 items / 2 000 bytes) so a noisy server cannot flood the
//! context window.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::oneshot;

use crate::config::Lsp as LspConfig;

/// Max rendered diagnostic items.
const MAX_ITEMS: usize = 20;
/// Max rendered bytes (the `(+N more)` trailer may exceed this).
const MAX_BYTES: usize = 2_000;
/// How long `touch` waits for the `initialize` response.
const INIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// One cached diagnostic (normalized off the wire).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diag {
    /// 1=error 2=warning 3=info 4=hint (0 = unset).
    pub severity: u8,
    /// 0-based LSP line.
    pub line: u64,
    pub message: String,
    pub source: Option<String>,
}

/// Extension → language id (only these get diagnostics).
pub fn language_for(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "rs" => "rust",
        "py" | "pyi" => "python",
        "ts" | "tsx" => "typescript",
        "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "go" => "go",
        "c" | "cc" | "cpp" | "h" | "hpp" => "c",
        "java" => "java",
        "rb" => "ruby",
        _ => return None,
    })
}

/// `file://` URI with minimal percent-encoding (everything outside the
/// unreserved set except `/`).
pub fn uri_for(path: &Path) -> String {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    let mut out = String::from("file://");
    for &b in abs.as_os_str().as_encoded_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Render cached diagnostics: errors/warnings first (then info/hint),
/// stable by line, capped at [`MAX_ITEMS`] items and [`MAX_BYTES`] bytes
/// with a `(+N more)` trailer.
pub fn render_diags(diags: &[Diag]) -> Vec<String> {
    let severity_rank = |d: &Diag| match d.severity {
        1 => 0,
        2 => 1,
        _ => 2,
    };
    let mut sorted: Vec<&Diag> = diags.iter().collect();
    sorted.sort_by_key(|d| (severity_rank(d), d.line));
    let mut lines: Vec<String> = Vec::with_capacity(MAX_ITEMS + 1);
    let mut bytes = 0usize;
    let mut shown = 0usize;
    for d in sorted.iter() {
        if shown >= MAX_ITEMS {
            break;
        }
        let label = match d.severity {
            1 => "error",
            2 => "warning",
            3 => "info",
            4 => "hint",
            _ => "diagnostic",
        };
        // within-line cap: one huge message (rust-analyzer trait
        // mismatch listings run long) must not break the byte budget
        const MESSAGE_CAP: usize = 300;
        let message = if d.message.chars().count() > MESSAGE_CAP {
            let cut: String = d.message.chars().take(MESSAGE_CAP).collect();
            format!("{cut}…")
        } else {
            d.message.clone()
        };
        let line = format!(
            "{label} L{}: {} ({})",
            d.line + 1,
            message,
            d.source.as_deref().unwrap_or("unknown")
        );
        if shown > 0 && bytes + line.len() > MAX_BYTES {
            break;
        }
        bytes += line.len();
        lines.push(line);
        shown += 1;
    }
    let hidden = sorted.len().saturating_sub(shown);
    if hidden > 0 {
        lines.push(format!("(+{hidden} more)"));
    }
    lines
}

/// A live server child for one language.
struct ServerProc {
    _child: Child,
    writer: ChildStdin,
    /// Per-URI document versions (didChange).
    versions: HashMap<String, i64>,
}

/// Shared diagnostics cache (URI → latest publishDiagnostics). `None`
/// marks "no publish for the current content yet" (inserted on touch,
/// replaced by the reader on publish) so a poll never mistakes the
/// previous edit's diagnostics for the new content's.
type Cache = parking_lot::Mutex<HashMap<String, Option<Vec<Diag>>>>;

/// Diagnostics manager. Inert unless `[lsp] enable = true`.
pub struct LspManager {
    enabled: bool,
    cwd: PathBuf,
    commands: BTreeMap<String, String>,
    servers: HashMap<String, ServerProc>,
    /// Languages whose server failed to spawn or initialize; never
    /// retried within the session.
    failed: HashSet<String>,
    cache: std::sync::Arc<Cache>,
}

impl LspManager {
    /// New manager; a no-op unless the config enables it.
    pub fn new(cwd: &Path, cfg: &LspConfig) -> Self {
        Self {
            enabled: cfg.enable == Some(true),
            cwd: cwd.to_path_buf(),
            commands: cfg.commands.clone().unwrap_or_default(),
            servers: HashMap::new(),
            failed: HashSet::new(),
            cache: std::sync::Arc::new(parking_lot::Mutex::new(HashMap::new())),
        }
    }

    /// Report a file's new content: spawns the language's server on
    /// first touch (initialize → initialized → didOpen), then sends a
    /// full-text didChange. Spawns/initialization failures mark the
    /// language failed for the session; every path is silent — LSP is
    /// advisory context, never a turn failure. Returns `false` when
    /// nothing was sent (disabled, unknown language, unconfigured, or
    /// failed server) so callers can skip polling entirely.
    pub async fn touch(&mut self, path: &Path, new_text: &str) -> bool {
        if !self.enabled {
            return false;
        }
        let Some(lang) = language_for(path) else {
            return false;
        };
        let Some(command) = self.commands.get(lang).cloned() else {
            return false;
        };
        if self.failed.contains(lang) {
            return false;
        }
        if !self.servers.contains_key(lang) && !self.spawn(lang, &command).await {
            return false;
        }
        let Some(server) = self.servers.get_mut(lang) else {
            return false;
        };
        let uri = uri_for(path);
        let version = server.versions.entry(uri.clone()).or_insert(0);
        *version += 1;
        let method = if *version == 1 {
            ("textDocument/didOpen", None)
        } else {
            ("textDocument/didChange", Some(*version))
        };
        let text_doc = match method.1 {
            None => serde_json::json!({
                "uri": uri,
                "languageId": lang,
                "version": 1,
                "text": new_text,
            }),
            Some(v) => serde_json::json!({ "uri": uri, "version": v }),
        };
        let params = if method.1.is_none() {
            serde_json::json!({ "textDocument": text_doc })
        } else {
            serde_json::json!({
                "textDocument": text_doc,
                "contentChanges": [{ "text": new_text }],
            })
        };
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method.0,
            "params": params,
        });
        // invalidate any publish for the previous content BEFORE the
        // write: a poll between now and the server's republish must see
        // "nothing yet", never the prior edit's diagnostics
        self.cache.lock().insert(uri.clone(), None);
        if write_msg(&mut server.writer, &msg).await.is_err() {
            // server died: drop it and never retry this session
            self.servers.remove(lang);
            self.failed.insert(lang.to_string());
            return false;
        }
        true
    }

    /// Latest rendered diagnostics for `path` (`None` when the server
    /// has not published for the current content yet — distinguishes
    /// "wait" from "clean, nothing to report").
    pub fn diagnostics(&self, path: &Path) -> Option<Vec<String>> {
        let uri = uri_for(path);
        let cache = self.cache.lock();
        match cache.get(&uri) {
            Some(Some(diags)) => Some(render_diags(diags)),
            _ => None,
        }
    }

    /// Spawn + initialize one server. `false` = failed (recorded).
    async fn spawn(&mut self, lang: &str, command: &str) -> bool {
        let mut argv = command.split_whitespace().map(str::to_string);
        let Some(program) = argv.next() else {
            self.failed.insert(lang.to_string());
            return false;
        };
        let args: Vec<String> = argv.collect();
        let mut child = match tokio::process::Command::new(&program)
            .args(&args)
            .current_dir(&self.cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
        {
            Ok(c) => c,
            Err(_) => {
                self.failed.insert(lang.to_string());
                return false;
            }
        };
        let mut writer = match child.stdin.take() {
            Some(w) => w,
            None => {
                self.failed.insert(lang.to_string());
                return false;
            }
        };
        let reader = child.stdout.take();
        let (init_tx, init_rx) = oneshot::channel::<bool>();
        if let Some(reader) = reader {
            tokio::spawn(read_loop(
                BufReader::new(reader),
                self.cache.clone(),
                init_tx,
            ));
        }
        let init = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "processId": null,
                "rootUri": uri_for(&self.cwd),
                "capabilities": {},
            },
        });
        if write_msg(&mut writer, &init).await.is_err() {
            self.failed.insert(lang.to_string());
            return false;
        }
        // wait for the initialize response before further traffic
        let ack = tokio::time::timeout(INIT_TIMEOUT, init_rx).await;
        let ok = matches!(ack, Ok(Ok(true)));
        if !ok {
            self.failed.insert(lang.to_string());
            return false;
        }
        let initialized = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "initialized",
            "params": {},
        });
        if write_msg(&mut writer, &initialized).await.is_err() {
            self.failed.insert(lang.to_string());
            return false;
        }
        self.servers.insert(
            lang.to_string(),
            ServerProc {
                _child: child,
                writer,
                versions: HashMap::new(),
            },
        );
        true
    }
}

impl Drop for LspManager {
    fn drop(&mut self) {
        // dropping the writers closes stdin (clean server exit); the
        // Child handles follow with kill_on_drop as backstop
        self.servers.clear();
        self.failed.clear();
    }
}

/// Write one framed JSON-RPC message.
async fn write_msg(w: &mut ChildStdin, msg: &serde_json::Value) -> std::io::Result<()> {
    let body = serde_json::to_string(msg)?;
    w.write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
        .await?;
    w.write_all(body.as_bytes()).await?;
    w.flush().await
}

/// Read framed messages off a server's stdout, caching
/// publishDiagnostics and signaling the initialize response. Runs until
/// the pipe closes (server exit drops the cache updates).
async fn read_loop<R: tokio::io::AsyncRead + Unpin + Send + 'static>(
    mut reader: R,
    cache: std::sync::Arc<Cache>,
    init_tx: oneshot::Sender<bool>,
) {
    let mut init_tx = Some(init_tx);
    loop {
        // read headers
        let headers = match read_headers(&mut reader).await {
            Ok(Some(h)) => h,
            _ => return,
        };
        let len: usize = match headers
            .iter()
            .find_map(|h| h.strip_prefix("Content-Length: "))
            .and_then(|v| v.trim().parse().ok())
        {
            Some(l) => l,
            None => continue,
        };
        // read body
        let mut body = vec![0u8; len];
        if reader.read_exact(&mut body).await.is_err() {
            return;
        }
        let Ok(msg) = serde_json::from_slice::<serde_json::Value>(&body) else {
            continue;
        };
        if let Some(id) = msg.get("id") {
            // initialize response: true when the server returned a result
            if id.as_i64() == Some(1) {
                if let Some(tx) = init_tx.take() {
                    let ok = msg.get("result").is_some();
                    let _ = tx.send(ok);
                }
                continue;
            }
        }
        if msg.get("method").and_then(|m| m.as_str()) == Some("textDocument/publishDiagnostics") {
            let Some(uri) = msg["params"]["uri"].as_str() else {
                continue;
            };
            let diags: Vec<Diag> = msg["params"]["diagnostics"]
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|d| {
                            let message = d.get("message")?.as_str()?.to_string();
                            Some(Diag {
                                severity: d
                                    .get("severity")
                                    .and_then(serde_json::Value::as_u64)
                                    .unwrap_or(0) as u8,
                                line: d["range"]["start"]["line"].as_u64().unwrap_or(0),
                                message,
                                source: d
                                    .get("source")
                                    .and_then(serde_json::Value::as_str)
                                    .map(str::to_string),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            cache.lock().insert(uri.to_string(), Some(diags));
        }
        // everything else (logMessage, progress, ...) is ignored
    }
}

/// Read one header block (to the blank line); `None` = EOF. The
/// preceding body of the last message must already be consumed.
async fn read_headers<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
) -> std::io::Result<Option<Vec<String>>> {
    let mut headers: Vec<String> = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        // read one line terminated by \n (header lines end \r\n)
        let mut line = Vec::new();
        loop {
            match reader.read(&mut byte).await {
                Ok(0) | Err(_) => return Ok(None),
                Ok(_) if byte[0] == b'\n' => break,
                Ok(_) => line.push(byte[0]),
            }
        }
        let line = String::from_utf8_lossy(&line)
            .trim_end_matches('\r')
            .to_string();
        if line.is_empty() {
            if headers.is_empty() {
                continue; // stray blank lines between messages
            }
            return Ok(Some(std::mem::take(&mut headers)));
        }
        headers.push(line);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn diag(sev: u8, line: u64, message: &str) -> Diag {
        Diag {
            severity: sev,
            line,
            message: message.to_string(),
            source: Some("test-ls".to_string()),
        }
    }

    #[test]
    fn caps_at_20_items() {
        let diags: Vec<Diag> = (0..25).map(|i| diag(1, i, "boom")).collect();
        let lines = render_diags(&diags);
        assert_eq!(lines.len(), 21, "20 items + trailer");
        assert_eq!(lines.last().unwrap(), "(+5 more)");
    }

    #[test]
    fn caps_at_2000_bytes() {
        let msg = "x".repeat(300);
        let diags: Vec<Diag> = (0..40).map(|i| diag(1, i, &msg)).collect();
        let lines = render_diags(&diags);
        let body: usize = lines.iter().map(|l| l.len()).sum();
        assert!(body < MAX_BYTES + 64, "body {body} stays near the cap");
        assert!(
            lines.last().unwrap().starts_with("(+"),
            "truncation is noted: {:?}",
            lines.last()
        );
    }

    #[test]
    fn errors_and_warnings_sort_first() {
        let diags = vec![diag(3, 1, "info"), diag(2, 5, "warn"), diag(1, 9, "bad")];
        let lines = render_diags(&diags);
        assert!(lines[0].starts_with("error L10"));
        assert!(lines[1].starts_with("warning L6"));
        assert!(lines[2].starts_with("info L2"));
    }

    #[test]
    fn renders_source_and_line_one_based() {
        let lines = render_diags(&[diag(1, 11, "expected `;`")]);
        assert_eq!(lines[0], "error L12: expected `;` (test-ls)");
    }

    #[test]
    fn language_table_and_uris() {
        assert_eq!(language_for(Path::new("a/b.rs")), Some("rust"));
        assert_eq!(language_for(Path::new("x.TS")), Some("typescript"));
        assert_eq!(language_for(Path::new("a.txt")), None);
        assert_eq!(
            uri_for(Path::new("/tmp/s p/q.rs")),
            "file:///tmp/s%20p/q.rs"
        );
    }

    #[tokio::test]
    async fn manager_is_inert_without_enable() {
        let mut m = LspManager::new(
            Path::new("/tmp"),
            &LspConfig {
                enable: None,
                commands: Some(BTreeMap::from([(
                    "rust".to_string(),
                    "rust-analyzer".to_string(),
                )])),
            },
        );
        m.touch(Path::new("/tmp/x.rs"), "fn a() {}").await;
        assert!(m.servers.is_empty(), "disabled manager spawns nothing");
        assert!(
            !m.touch(Path::new("/tmp/x.rs"), "fn a() {}").await,
            "inert touch reports false so callers skip polling"
        );
        assert!(m.diagnostics(Path::new("/tmp/x.rs")).is_none());
    }

    #[tokio::test]
    async fn dead_command_marks_language_failed_without_panicking() {
        let mut m = LspManager::new(
            Path::new("/tmp"),
            &LspConfig {
                enable: Some(true),
                commands: Some(BTreeMap::from([(
                    "rust".to_string(),
                    "ka-definitely-missing-lsp-binary".to_string(),
                )])),
            },
        );
        m.touch(Path::new("/tmp/x.rs"), "fn a() {}").await;
        assert!(m.failed.contains("rust"), "missing binary is recorded");
        assert!(m.servers.is_empty());
    }

    /// A fake stdio server publishing exactly once (on didOpen) and
    /// staying silent on didChange — the worst-case stale publisher.
    /// Regression for "the previous edit's diagnostics attach to the
    /// next edit": touch must invalidate the cache so the poll sees
    /// "nothing yet", never the old publish.
    #[cfg(unix)]
    #[tokio::test]
    async fn touch_invalidates_previous_publish() {
        use std::io::Write;
        let dir = std::env::temp_dir().join(format!("ka-lsp-fake-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("fake.sh");
        let mut f = std::fs::File::create(&fake).unwrap();
        writeln!(
            f,
            r#"#!/bin/sh
# minimal LSP fake: ack initialize, publish ONE diagnostic shortly after
# (didOpen lands in that window), then stay silently alive
resp='{{"jsonrpc":"2.0","id":1,"result":{{"capabilities":{{}}}}}}'
printf 'Content-Length: %d\r\n\r\n%s' "${{#resp}}" "$resp"
sleep 0.4
diag='{{"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{{"uri":"{}","diagnostics":[{{"range":{{"start":{{"line":0,"character":0}},"end":{{"line":0,"character":1}}}},"severity":1,"message":"STALE-ERROR","source":"fake"}}]}}}}'
printf 'Content-Length: %d\r\n\r\n%s' "${{#diag}}" "$diag"
cat > /dev/null
"#,
            uri_for(&dir.join("x.rs")),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&fake).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&fake, perms).unwrap();
        }
        let mut m = LspManager::new(
            &dir,
            &LspConfig {
                enable: Some(true),
                commands: Some(BTreeMap::from([(
                    "rust".to_string(),
                    format!("sh {}", fake.display()),
                )])),
            },
        );
        let src = dir.join("x.rs");
        assert!(m.touch(&src, "let x: u32 = 1;").await, "didOpen sent");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut first = m.diagnostics(&src);
        while first.is_none() && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            first = m.diagnostics(&src);
        }
        let first = first.expect("fake server published");
        assert!(
            first.iter().any(|l| l.contains("STALE-ERROR")),
            "first publish landed: {first:?}"
        );
        // second edit: the cache must be invalidated immediately and the
        // silent fake server never republishes — a poll must see None,
        // never the STALE-ERROR from edit 1
        assert!(m.touch(&src, "let x: u64 = 1;").await, "didChange sent");
        assert!(
            m.diagnostics(&src).is_none(),
            "touch invalidates the previous publish"
        );
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(
            m.diagnostics(&src).is_none(),
            "stale diagnostics must not reattach from a silent server"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
