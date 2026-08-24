//! Keyless wire-test harness: a tiny HTTP/1.1 server over a tokio TcpListener
//! that replays scripted responses and records what the client asked for.
//! Only used by tests; fixtures are hand-authored from provider stream docs
//! and should be replaced with real captures over time.

use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

/// A scripted response.
#[derive(Debug, Clone)]
pub struct Scripted {
    /// HTTP status line code.
    pub status: u16,
    /// Extra headers (e.g. retry-after).
    pub headers: Vec<(String, String)>,
    /// Body bytes (SSE text).
    pub body: String,
}

impl Scripted {
    /// 200 with an SSE body.
    pub fn sse(body: &str) -> Self {
        Self {
            status: 200,
            headers: vec![],
            body: body.to_string(),
        }
    }

    /// An error status with a JSON-ish body and optional extra headers.
    pub fn error(status: u16, headers: Vec<(String, String)>, body: &str) -> Self {
        Self {
            status,
            headers,
            body: body.to_string(),
        }
    }
}

/// A captured request.
#[derive(Debug, Clone)]
pub struct Captured {
    /// First line of the HTTP request.
    pub request_line: String,
    /// Header map, lowercased names.
    pub headers: Vec<(String, String)>,
    /// Body bytes.
    pub body: String,
}

/// Handle to the scripted server.
pub struct TestServer {
    /// Base URL (`http://127.0.0.1:port`) to point wires at.
    pub base_url: String,
    /// Requests captured so far.
    pub captured: Arc<Mutex<Vec<Captured>>>,
    shutdown: Option<oneshot::Sender<()>>,
}

impl TestServer {
    /// Start a server that responds to each request with the next scripted
    /// response (last one repeats).
    pub async fn start(script: Vec<Scripted>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap_or_else(|e| panic!("bind: {e}"));
        let port = listener
            .local_addr()
            .unwrap_or_else(|e| panic!("addr: {e}"))
            .port();
        let captured: Arc<Mutex<Vec<Captured>>> = Arc::new(Mutex::new(Vec::new()));
        let (tx, rx) = oneshot::channel::<()>();
        let script = Arc::new(script);
        let cap = Arc::clone(&captured);
        tokio::spawn(async move {
            let mut script_idx = 0usize;
            let mut shutdown = std::pin::pin!(rx);
            loop {
                let (stream, _) = tokio::select! {
                    res = listener.accept() => match res {
                        Ok(v) => v,
                        Err(_) => break,
                    },
                    _ = &mut shutdown => break,
                };
                let script = Arc::clone(&script);
                let cap = Arc::clone(&cap);
                let idx = script_idx.min(script.len().saturating_sub(1));
                script_idx += 1;
                tokio::spawn(async move {
                    if let Err(e) = serve_one(stream, &script[idx], &cap).await {
                        eprintln!("test server error: {e}");
                    }
                });
            }
        });
        Self {
            base_url: format!("http://127.0.0.1:{port}"),
            captured,
            shutdown: Some(tx),
        }
    }

    /// Captured requests so far (clone out).
    pub fn requests(&self) -> Vec<Captured> {
        self.captured.lock().map(|c| c.clone()).unwrap_or_default()
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

async fn serve_one(
    mut stream: tokio::net::TcpStream,
    scripted: &Scripted,
    captured: &Mutex<Vec<Captured>>,
) -> std::io::Result<()> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    // read headers
    let header_end = loop {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or_default().to_string();
    let mut headers = Vec::new();
    let mut content_length = 0usize;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            let k = k.trim().to_ascii_lowercase();
            let v = v.trim().to_string();
            if k == "content-length" {
                content_length = v.parse().unwrap_or(0);
            }
            headers.push((k, v));
        }
    }
    // read body
    let mut body_bytes = buf[header_end..].to_vec();
    while body_bytes.len() < content_length {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        body_bytes.extend_from_slice(&tmp[..n]);
    }
    if let Ok(mut c) = captured.lock() {
        c.push(Captured {
            request_line,
            headers,
            body: String::from_utf8_lossy(&body_bytes).to_string(),
        });
    }

    let reason = match scripted.status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Status",
    };
    let mut resp = format!("HTTP/1.1 {} {}\r\n", scripted.status, reason);
    resp.push_str("Content-Type: text/event-stream\r\n");
    for (k, v) in &scripted.headers {
        resp.push_str(&format!("{k}: {v}\r\n"));
    }
    resp.push_str(&format!("Content-Length: {}\r\n", scripted.body.len()));
    resp.push_str("Connection: close\r\n\r\n");
    stream.write_all(resp.as_bytes()).await?;
    stream.write_all(scripted.body.as_bytes()).await?;
    stream.flush().await?;
    Ok(())
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}
