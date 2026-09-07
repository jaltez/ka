//! Web search + URL reader hands. Search goes through the first
//! `[[search]]` provider (tavily / brave / bing, payload shapes mapped
//! to one `SearchHit` list); `web_fetch` reads a URL and strips HTML to
//! plain text. Both pay exec-tier clearance: the query leaves the
//! machine, and fetches are SSRF-guarded against private targets.

use std::future::Future;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::{Value, json};

use super::{Clearance, Hand, HandContext, HandDef, ToolOutput};
use crate::config::SearchProvider;

/// One normalized search result.
pub struct SearchHit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// `web_search` hand.
pub struct WebSearchHand {
    provider: SearchProvider,
}

impl WebSearchHand {
    pub fn new(provider: SearchProvider) -> Self {
        Self { provider }
    }
}

/// `web_fetch` hand.
pub struct WebFetchHand {
    allow_private: bool,
}

impl WebFetchHand {
    pub fn new(allow_private: bool) -> Self {
        Self { allow_private }
    }
}

fn base_for(provider: &SearchProvider, default: &str) -> String {
    provider
        .base_url
        .clone()
        .unwrap_or_else(|| default.to_string())
        .trim_end_matches('/')
        .to_string()
}

async fn provider_search(provider: &SearchProvider, query: &str) -> Result<Vec<SearchHit>, String> {
    let key = ka_dialect::auth::resolve_token(&provider.api_key_env)
        .ok_or_else(|| format!("web_search: key ${} is not set", provider.api_key_env))?;
    provider_search_with(provider, query, &key).await
}

/// Search with an explicit key (tests inject one; the env ladder is
/// untestable under edition-2024 set_var rules).
async fn provider_search_with(
    provider: &SearchProvider,
    query: &str,
    key: &str,
) -> Result<Vec<SearchHit>, String> {
    let client = reqwest::Client::new();
    let hits: Vec<SearchHit> = match provider.provider.as_str() {
        "tavily" => {
            let url = format!("{}/search", base_for(provider, "https://api.tavily.com"));
            let resp: Value = client
                .post(&url)
                .header("Authorization", format!("Bearer {key}"))
                .json(&json!({"query": query, "max_results": 5}))
                .send()
                .await
                .map_err(|e| format!("tavily: {e}"))?
                .error_for_status()
                .map_err(|e| format!("tavily: {e}"))?
                .json()
                .await
                .map_err(|e| format!("tavily JSON: {e}"))?;
            resp["results"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|r| SearchHit {
                            title: r["title"].as_str().unwrap_or_default().into(),
                            url: r["url"].as_str().unwrap_or_default().into(),
                            snippet: r["content"].as_str().unwrap_or_default().into(),
                        })
                        .collect()
                })
                .unwrap_or_default()
        }
        "brave" => {
            let url = format!(
                "{}/v1/web/search?q={}",
                base_for(provider, "https://api.search.brave.com"),
                urlencode(query)
            );
            let resp: Value = client
                .get(&url)
                .header("X-Subscription-Token", key)
                .header("Accept", "application/json")
                .send()
                .await
                .map_err(|e| format!("brave: {e}"))?
                .error_for_status()
                .map_err(|e| format!("brave: {e}"))?
                .json()
                .await
                .map_err(|e| format!("brave JSON: {e}"))?;
            resp["web"]["results"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|r| SearchHit {
                            title: r["title"].as_str().unwrap_or_default().into(),
                            url: r["url"].as_str().unwrap_or_default().into(),
                            snippet: r["description"].as_str().unwrap_or_default().into(),
                        })
                        .collect()
                })
                .unwrap_or_default()
        }
        "bing" => {
            let url = format!(
                "{}/v7.0/search?q={}",
                base_for(provider, "https://api.bing.microsoft.com"),
                urlencode(query)
            );
            let resp: Value = client
                .get(&url)
                .header("Ocp-Apim-Subscription-Key", key)
                .send()
                .await
                .map_err(|e| format!("bing: {e}"))?
                .error_for_status()
                .map_err(|e| format!("bing: {e}"))?
                .json()
                .await
                .map_err(|e| format!("bing JSON: {e}"))?;
            resp["webPages"]["value"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .map(|r| SearchHit {
                            title: r["name"].as_str().unwrap_or_default().into(),
                            url: r["url"].as_str().unwrap_or_default().into(),
                            snippet: r["snippet"].as_str().unwrap_or_default().into(),
                        })
                        .collect()
                })
                .unwrap_or_default()
        }
        other => return Err(format!("web_search: unknown provider {other:?}")),
    };
    Ok(hits)
}

/// Minimal percent-encoding for query strings.
fn urlencode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Whether a URL's host is private/loopback/link-local.
pub fn is_private_host(url: &str) -> bool {
    let Some(host) = host_of(url) else {
        return true; // unparseable: treat as private (refuse)
    };
    let lower = host.to_ascii_lowercase();
    if lower == "localhost" || lower.ends_with(".localhost") || lower.ends_with(".local") {
        return true;
    }
    match lower.parse::<IpAddr>() {
        Ok(IpAddr::V4(v4)) => {
            v4.is_loopback() || v4.is_private() || v4.is_link_local() || v4.is_unspecified()
        }
        Ok(IpAddr::V6(v6)) => v6.is_loopback() || (v6.segments()[0] & 0xfe00) == 0xfc00,
        Err(_) => false, // public hostname: allowed (SSRF via DNS rebinding
                         // is out of scope for a tool-level guard)
    }
}

/// Extract the host component of a URL (no external crates).
fn host_of(url: &str) -> Option<String> {
    let (_, rest) = url.split_once("://").unwrap_or(("https", url));
    let rest = rest.split('/').next()?;
    let rest = rest.rsplit_once('@').map_or(rest, |(_, h)| h);
    if let Some(inner) = rest.strip_prefix('[') {
        return inner.split(']').next().map(str::to_string);
    }
    Some(rest.split(':').next()?.to_string())
}

/// Reduce HTML to readable plain text: drop script/style/nav blocks,
/// keep headings, paragraphs, list items, links (text + href) and code.
pub fn html_to_text(html: &str) -> String {
    use std::fmt::Write as _;
    let mut text = String::with_capacity(html.len() / 2);
    let mut chars = html.chars().peekable();
    // block-level tags that introduce a line break
    const BLOCKS: &[&str] = &[
        "p",
        "div",
        "br",
        "li",
        "tr",
        "h1",
        "h2",
        "h3",
        "h4",
        "h5",
        "h6",
        "table",
        "ul",
        "ol",
        "pre",
        "blockquote",
        "section",
        "article",
        "header",
        "footer",
    ];
    let mut skip_depth = 0usize; // inside script/style/nav
    while let Some(c) = chars.next() {
        if c == '<' {
            let mut tag = String::new();
            for n in chars.by_ref() {
                if n == '>' {
                    break;
                }
                tag.push(n);
            }
            let lower = tag.to_ascii_lowercase();
            let name = lower
                .trim()
                .trim_start_matches('/')
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_string();
            if matches!(name.as_str(), "script" | "style" | "nav" | "head") {
                if lower.starts_with('/') {
                    skip_depth = skip_depth.saturating_sub(1);
                } else {
                    skip_depth += 1;
                }
                continue;
            }
            if skip_depth > 0 {
                continue;
            }
            if BLOCKS.contains(&name.as_str()) {
                text.push('\n');
            }
            // keep href of links as plain reference
            if name == "a" && !lower.starts_with('/') {
                if let Some(href) = attr_value(&lower, "href") {
                    let _ = write!(text, "[{href}] ");
                }
            }
            continue;
        }
        if skip_depth > 0 {
            continue;
        }
        if c == '&' {
            // tiny entity handling
            let mut entity = String::new();
            for n in chars.by_ref() {
                if n == ';' {
                    break;
                }
                entity.push(n);
                if entity.len() > 8 {
                    break;
                }
            }
            match entity.as_str() {
                "amp" => text.push('&'),
                "lt" => text.push('<'),
                "gt" => text.push('>'),
                "quot" => text.push('"'),
                "nbsp" => text.push(' '),
                _ => {
                    text.push('&');
                    text.push_str(&entity);
                    text.push(';');
                }
            }
            continue;
        }
        text.push(c);
    }
    text.trim().to_string()
}

/// Pull `attr="value"` out of a tag string.
fn attr_value(tag: &str, attr: &str) -> Option<String> {
    let idx = tag.to_ascii_lowercase().find(attr)?;
    let rest = &tag[idx + attr.len()..];
    let rest = rest.trim_start_matches('=');
    let rest = rest.trim_start();
    let quoted = rest.starts_with('"') || rest.starts_with('\'');
    let quote = rest.chars().next()?;
    if quoted {
        rest[1..].split(quote).next().map(str::to_string)
    } else {
        rest.split_whitespace().next().map(str::to_string)
    }
}

impl Hand for WebSearchHand {
    fn def(&self) -> HandDef {
        HandDef {
            name: "web_search".into(),
            description: "Search the web via the configured provider. Returns title/url/snippet \
                          results. The query leaves the machine."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Search query"}
                },
                "required": ["query"]
            }),
            clearance: Clearance::Exec,
            read_only: true,
        }
    }

    fn execute<'a>(
        &'a self,
        args: &'a Value,
        _ctx: &'a HandContext,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>> {
        let provider = self.provider.clone();
        Box::pin(async move {
            let Some(query) = args.get("query").and_then(Value::as_str) else {
                return ToolOutput::err("web_search: missing required 'query'");
            };
            match provider_search(&provider, query).await {
                Ok(hits) if hits.is_empty() => ToolOutput::ok("(no results)"),
                Ok(hits) => {
                    let mut out = String::new();
                    for (i, h) in hits.iter().enumerate() {
                        out.push_str(&format!(
                            "{}. {} — {}\n   {}\n",
                            i + 1,
                            h.title,
                            h.url,
                            h.snippet
                        ));
                    }
                    ToolOutput::ok(out)
                }
                Err(e) => ToolOutput::err(e),
            }
        })
    }
}

/// Fetch size cap: 200 KB.
const FETCH_CAP: usize = 200 * 1024;

impl Hand for WebFetchHand {
    fn def(&self) -> HandDef {
        HandDef {
            name: "web_fetch".into(),
            description: "Fetch a URL and return its content as plain text (HTML reduced to \
                          readable text). Private/loopback hosts are refused unless allowed."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "url": {"type": "string", "description": "Absolute http(s) URL"}
                },
                "required": ["url"]
            }),
            clearance: Clearance::Exec,
            read_only: true,
        }
    }

    fn execute<'a>(
        &'a self,
        args: &'a Value,
        _ctx: &'a HandContext,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>> {
        let allow_private = self.allow_private;
        Box::pin(async move {
            let Some(url) = args.get("url").and_then(Value::as_str) else {
                return ToolOutput::err("web_fetch: missing required 'url'");
            };
            if !url.starts_with("http://") && !url.starts_with("https://") {
                return ToolOutput::err("web_fetch: only http(s) URLs are supported");
            }
            if !allow_private && is_private_host(url) {
                return ToolOutput::err(format!(
                    "web_fetch: refusing private/loopback target {url} ([tools.web] \
                     allow_private_hosts = true to override)"
                ));
            }
            let client = reqwest::Client::new();
            let resp = client
                .get(url)
                .header("User-Agent", "ka-web-fetch")
                .timeout(std::time::Duration::from_secs(20))
                .send()
                .await;
            let resp = match resp {
                Ok(r) => r,
                Err(e) => return ToolOutput::err(format!("web_fetch: {e}")),
            };
            let status = resp.status();
            if !status.is_success() {
                return ToolOutput::err(format!("web_fetch: status {status}"));
            }
            let ct = resp
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            let body = match resp.bytes().await {
                Ok(b) => b,
                Err(e) => return ToolOutput::err(format!("web_fetch: {e}")),
            };
            let mut bytes: &[u8] = &body;
            if bytes.len() > FETCH_CAP {
                bytes = &bytes[..FETCH_CAP];
            }
            let raw = String::from_utf8_lossy(bytes).into_owned();
            let text = if ct.contains("html") || raw.trim_start().starts_with('<') {
                html_to_text(&raw)
            } else {
                raw
            };
            if text.trim().is_empty() {
                return ToolOutput::ok("(empty page)");
            }
            ToolOutput::ok(text)
        })
    }
}

/// Both web hands over one provider config (engine bootstrap).
pub fn hands(provider: Option<SearchProvider>, allow_private: bool) -> Vec<Arc<dyn Hand>> {
    let mut out: Vec<Arc<dyn Hand>> = Vec::new();
    if let Some(p) = provider {
        out.push(Arc::new(WebSearchHand::new(p)));
    }
    out.push(Arc::new(WebFetchHand::new(allow_private)));
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn html_to_text_strips_scripts_keeps_structure() {
        let html = "<html><head><style>p{}</style></head><body>                    <nav>menu</nav><h1>Title</h1>                    <p>First para &amp; more.</p>                    <script>alert(1)</script>                    <a href=\"https://x.y\">a link</a>                    <ul><li>item</li></ul></body></html>";
        let text = html_to_text(html);
        assert!(text.contains("Title"), "{text}");
        assert!(text.contains("First para & more."), "{text}");
        assert!(text.contains("a link"), "{text}");
        assert!(text.contains("https://x.y"), "{text}");
        assert!(text.contains("item"), "{text}");
        assert!(!text.contains("alert"), "{text}");
        assert!(!text.contains("menu"), "{text}");
        assert!(!text.contains("p{}"), "{text}");
    }

    #[test]
    fn ssrf_guard_refuses_private_targets() {
        assert!(is_private_host("http://127.0.0.1:9000/x"));
        assert!(is_private_host("http://localhost/x"));
        assert!(is_private_host("http://10.0.0.5/"));
        assert!(is_private_host("http://192.168.1.1/"));
        assert!(is_private_host("http://169.254.1.1/"));
        assert!(is_private_host("http://[::1]/"));
        assert!(!is_private_host("https://example.com/x"));
    }

    /// Serve a canned HTTP response; returns the bound address.
    async fn serve(body: &'static str) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = [0u8; 4096];
                    let _ = sock.read(&mut buf).await;
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    sock.write_all(resp.as_bytes()).await.ok();
                });
            }
        });
        addr
    }

    #[tokio::test]
    async fn tavily_provider_maps_hits() {
        let body = r#"{"results":[
            {"title":"R1","url":"https://a","content":"snippet a"},
            {"title":"R2","url":"https://b","content":"snippet b"}]}"#;
        let addr = serve(body).await;
        let provider = SearchProvider {
            provider: "tavily".into(),
            api_key_env: "KA_TEST_TAVILY_KEY".into(),
            base_url: Some(format!("http://{addr}")),
        };
        let hits = provider_search_with(&provider, "rust async", "test-key")
            .await
            .unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "R1");
        assert_eq!(hits[0].url, "https://a");
        assert_eq!(hits[1].snippet, "snippet b");
    }

    #[tokio::test]
    async fn brave_provider_maps_hits() {
        let body = r#"{"web":{"results":[
            {"title":"B1","url":"https://b1","description":"d1"}]}}"#;
        let addr = serve(body).await;
        let provider = SearchProvider {
            provider: "brave".into(),
            api_key_env: "KA_TEST_BRAVE_KEY".into(),
            base_url: Some(format!("http://{addr}")),
        };
        let hits = provider_search_with(&provider, "q", "test-key")
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "B1");
        assert_eq!(hits[0].snippet, "d1");
    }

    #[tokio::test]
    async fn bing_provider_maps_hits() {
        let body = r#"{"webPages":{"value":[
            {"name":"N1","url":"https://n1","snippet":"s1"}]}}"#;
        let addr = serve(body).await;
        let provider = SearchProvider {
            provider: "bing".into(),
            api_key_env: "KA_TEST_BING_KEY".into(),
            base_url: Some(format!("http://{addr}")),
        };
        let hits = provider_search_with(&provider, "q", "test-key")
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "N1");
        assert_eq!(hits[0].snippet, "s1");
    }

    #[tokio::test]
    async fn web_fetch_refuses_private_hosts_unless_allowed() {
        let hand = WebFetchHand::new(false);
        let out = hand
            .execute(&json!({"url": "http://127.0.0.1:1/x"}), &test_ctx())
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("private"), "{}", out.content);

        // allowed: fetches through the loopback fixture
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let (mut sock, _) = listener.accept().await.unwrap();
            let html = "<html><body><h1>Hi</h1><p>Body text</p></body></html>";
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}",
                html.len()
            );
            sock.write_all(resp.as_bytes()).await.ok();
        });
        let hand = WebFetchHand::new(true);
        let out = hand
            .execute(&json!({"url": format!("http://{addr}/page")}), &test_ctx())
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("Hi"), "{}", out.content);
        assert!(out.content.contains("Body text"), "{}", out.content);
    }

    fn test_ctx() -> HandContext {
        HandContext {
            cwd: std::env::temp_dir(),
            ledger: std::sync::Arc::new(parking_lot::Mutex::new(crate::hands::Ledger::default())),
            spill: std::sync::Arc::new(crate::hands::Spill::new()),
            snapshots: std::sync::Arc::new(parking_lot::Mutex::new(
                crate::hands::snapshots::Snapshots::inert(),
            )),
            jobs: std::sync::Arc::new(crate::hands::jobs::JobTable::new()),
            bash_background_ms: 0,
            max_image_mb: 5,
            web_allow_private: false,
        }
    }
}
