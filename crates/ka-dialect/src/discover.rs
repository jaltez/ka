//! Local-endpoint discovery: probe Ollama / LM Studio / vLLM `/v1/models`
//! and fold the results into the catalog as ephemeral dialect rows.

use std::time::Duration;

use serde_json::Value;

use crate::{Catalog, Dialect, Discovery, Modality, Wire};

/// Probe one local engine; returns `vendor/model` → dialect rows. Failures
/// (not running, timeout) yield an empty vec — discovery is best-effort.
pub async fn discover(kind: Discovery, base_override: Option<String>) -> Vec<(String, Dialect)> {
    let (vendor, base_v1) = resolve_base(kind, base_override);
    let Ok(models) = probe_models(&base_v1).await else {
        return Vec::new();
    };
    models
        .into_iter()
        .map(|id| {
            (
                format!("{vendor}/{id}"),
                local_dialect(kind, base_v1.clone()),
            )
        })
        .collect()
}

/// Probe all three engines concurrently and overlay results on a catalog.
pub async fn overlay_discovered(catalog: &mut Catalog) {
    let (a, b, c) = tokio::join!(
        discover(Discovery::Ollama, None),
        discover(Discovery::LmStudio, None),
        discover(Discovery::Vllm, None),
    );
    for (id, dialect) in a.into_iter().chain(b).chain(c) {
        // explicit catalog rows always win over discovery
        catalog.dialects.entry(id).or_insert(dialect);
    }
}

fn resolve_base(kind: Discovery, base_override: Option<String>) -> (&'static str, String) {
    match kind {
        Discovery::Ollama => {
            let root = base_override
                .or_else(|| std::env::var("OLLAMA_HOST").ok())
                .unwrap_or_else(|| "http://localhost:11434".to_string());
            let root = if root.contains("://") {
                root
            } else {
                format!("http://{root}")
            };
            ("ollama", format!("{}/v1", root.trim_end_matches('/')))
        }
        Discovery::LmStudio => {
            let root = base_override
                .or_else(|| std::env::var("LMSTUDIO_BASE_URL").ok())
                .unwrap_or_else(|| "http://localhost:1234".to_string());
            ("lmstudio", format!("{}/v1", root.trim_end_matches('/')))
        }
        Discovery::Vllm => {
            let root = base_override
                .or_else(|| std::env::var("KA_VLLM_BASE_URL").ok())
                .unwrap_or_else(|| "http://localhost:8000".to_string());
            ("vllm", format!("{}/v1", root.trim_end_matches('/')))
        }
    }
}

async fn probe_models(base_v1: &str) -> Result<Vec<String>, ()> {
    let client = reqwest::Client::new();
    let resp = tokio::time::timeout(
        Duration::from_millis(1_500),
        client
            .get(format!("{base_v1}/models"))
            .timeout(Duration::from_millis(1_200))
            .send(),
    )
    .await
    .map_err(|_| ())?
    .map_err(|_| ())?;
    if !resp.status().is_success() {
        return Err(());
    }
    let body: Value = resp.json().await.map_err(|_| ())?;
    let Some(data) = body["data"].as_array() else {
        return Ok(Vec::new());
    };
    Ok(data
        .iter()
        .filter_map(|m| m["id"].as_str().map(String::from))
        .collect())
}

fn local_dialect(kind: Discovery, base_v1: String) -> Dialect {
    Dialect {
        wire: Wire::OpenaiChat,
        discovery: Some(kind),
        context: match kind {
            Discovery::Ollama => 131_072,
            Discovery::LmStudio | Discovery::Vllm => 32_768,
        },
        max_output: 8_192,
        efforts: Vec::new(),
        input: vec![Modality::Text],
        base_url: Some(base_v1),
        auth_env: None,
        cache: Default::default(),
        ratio: 3.2,
        first_byte_timeout_ms: 0,
        price: Default::default(),
        flags: Default::default(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::testkit::{Scripted, TestServer};

    #[tokio::test]
    async fn probes_and_builds_rows() {
        let server = TestServer::start(vec![Scripted {
            status: 200,
            headers: vec![("content-type".to_string(), "application/json".to_string())],
            body: r#"{"data":[{"id":"qwen3:32b"},{"id":"llama4:8b"}]}"#.to_string(),
        }])
        .await;
        // point discovery at the test server's root; /v1/models path appended
        let rows = discover(Discovery::Ollama, Some(server.base_url.clone())).await;
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, "ollama/qwen3:32b");
        assert_eq!(
            rows[0].1.base_url.as_deref(),
            Some(format!("{}/v1", server.base_url).as_str())
        );
        assert_eq!(rows[0].1.context, 131_072);
        assert_eq!(rows[0].1.first_byte_timeout_ms, 0);
    }

    #[tokio::test]
    async fn dead_endpoint_is_empty() {
        let rows = discover(Discovery::LmStudio, Some("http://127.0.0.1:1".into())).await;
        assert!(rows.is_empty());
    }

    #[tokio::test]
    async fn overlay_respects_explicit_rows() {
        let mut catalog = Catalog::embedded();
        overlay_discovered(&mut catalog).await;
        // embedded ollama row must survive even if a local server exists
        assert_eq!(
            catalog.get("ollama/qwen3-32b").unwrap().context,
            Catalog::embedded().get("ollama/qwen3-32b").unwrap().context
        );
    }
}
