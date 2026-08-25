//! Local-endpoint discovery: probe `/v1/models` on local OpenAI-compatible
//! servers and turn findings into ephemeral dialect rows.

use crate::dialects::{Dialect, Discovery, Wire};

/// A discovered local model.
#[derive(Debug, Clone)]
pub struct FoundModel {
    /// `vendor/model` id.
    pub model_id: String,
    /// Dialect row for it.
    pub dialect: Dialect,
}

/// Probe an OpenAI-compatible `/v1/models` endpoint and build dialect rows.
/// Unknown capabilities default conservatively: context 0 (unknown),
/// unbounded first-byte timeout (local prefill can be slow).
pub async fn discover_openai_compatible(
    client: &reqwest::Client,
    vendor: &str,
    base_url: &str,
    discovery: Discovery,
) -> Vec<FoundModel> {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let Ok(resp) = client
        .get(&url)
        .timeout(std::time::Duration::from_secs(2))
        .send()
        .await
    else {
        return Vec::new();
    };
    if !resp.status().is_success() {
        return Vec::new();
    }
    let Ok(text) = resp.text().await else {
        return Vec::new();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    let Some(ids) = v.get("data").and_then(|d: &serde_json::Value| d.as_array()) else {
        return Vec::new();
    };
    ids.iter()
        .filter_map(|entry| {
            let id = entry.get("id").and_then(|i| i.as_str())?;
            Some(FoundModel {
                model_id: format!("{vendor}/{id}"),
                dialect: Dialect {
                    wire: Wire::OpenaiChat,
                    base_url: Some(base_url.to_string()),
                    discovery: Some(discovery),
                    first_byte_timeout_ms: 0,
                    ..Dialect::default_for_discovery()
                },
            })
        })
        .collect()
}

/// Probe default local endpoints (Ollama, LM Studio) and insert findings
/// into the catalog. Explicit catalog rows always win over discovered ones.
pub async fn overlay_discovered(catalog: &mut crate::dialects::Catalog) {
    let client = reqwest::Client::new();
    let mut found = discover_ollama(&client).await;
    found.extend(discover_lmstudio(&client).await);
    for f in found {
        catalog.dialects.entry(f.model_id).or_insert(f.dialect);
    }
}

/// Probe a local Ollama server on the default port.
pub async fn discover_ollama(client: &reqwest::Client) -> Vec<FoundModel> {
    discover_openai_compatible(
        client,
        "ollama",
        "http://127.0.0.1:11434/v1",
        Discovery::Ollama,
    )
    .await
}

/// Probe a local LM Studio server on the default port.
pub async fn discover_lmstudio(client: &reqwest::Client) -> Vec<FoundModel> {
    discover_openai_compatible(
        client,
        "lmstudio",
        "http://127.0.0.1:1234/v1",
        Discovery::LmStudio,
    )
    .await
}

impl Dialect {
    /// Defaults for discovered rows: cheap, safe, unknown-context.
    pub fn default_for_discovery() -> Self {
        Self {
            wire: Wire::OpenaiChat,
            base_url: None,
            api_key_env: None,
            wire_model: None,
            discovery: None,
            context: 0,
            max_output: 8_192,
            efforts: Vec::new(),
            input: Vec::new(),
            cache: crate::dialects::Cache::Off,
            ratio: 4.0,
            first_byte_timeout_ms: 0,
            effort_budgets: Default::default(),
            price: Default::default(),
            priced: false,
            flags: Default::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[tokio::test]
    async fn discovers_models_from_endpoint() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 2048];
            let _ = sock.read(&mut buf).await;
            let body = r#"{"data":[{"id":"qwen3:8b"},{"id":"llama4:latest"}]}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
        });
        let client = reqwest::Client::new();
        let found = discover_openai_compatible(
            &client,
            "ollama",
            &format!("http://{addr}/v1"),
            Discovery::Ollama,
        )
        .await;
        let ids: Vec<&str> = found.iter().map(|f| f.model_id.as_str()).collect();
        assert!(ids.contains(&"ollama/qwen3:8b"), "got: {ids:?}");
        assert!(ids.contains(&"ollama/llama4:latest"), "got: {ids:?}");
        let d = &found[0].dialect;
        assert_eq!(d.wire, Wire::OpenaiChat);
        assert_eq!(d.first_byte_timeout_ms, 0);
        assert_eq!(
            d.base_url.as_deref(),
            Some(format!("http://{addr}/v1").as_str())
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn dead_endpoint_is_empty() {
        let client = reqwest::Client::new();
        let found =
            discover_openai_compatible(&client, "x", "http://127.0.0.1:1/v1", Discovery::Ollama)
                .await;
        assert!(found.is_empty());
    }
}
