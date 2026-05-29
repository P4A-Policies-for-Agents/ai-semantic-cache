// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Salesforce, Inc.

use crate::error::EmbedError;
use async_trait::async_trait;
use pdk::hl::{HttpClient, Service};
use std::rc::Rc;
use std::time::Duration;

/// Implementations target single-threaded wasm runtimes (PDK's `HttpClient`
/// is `Rc`-based, hence `!Send + !Sync`). We therefore use `?Send` futures
/// and drop the `Send + Sync` super-bounds. Native test fakes still work —
/// they're held inside `Rc<dyn Embedder>` constructed and consumed on the
/// same Tokio worker.
#[async_trait(?Send)]
pub trait Embedder {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError>;
    fn dimension(&self) -> usize;
}

#[derive(Debug, Clone)]
pub struct OpenAICompatibleEmbedderConfig {
    pub base_url: String,
    pub model: String,
    pub api_key: Option<String>,
    pub auth_header: String,
    pub auth_scheme: String,
    pub dimension: usize,
    pub timeout_ms: u64,
}

impl OpenAICompatibleEmbedderConfig {
    pub fn auth_header_value(&self) -> Option<(String, String)> {
        self.api_key.as_ref().map(|k| {
            let v = if self.auth_scheme.is_empty() {
                k.clone()
            } else {
                format!("{} {}", self.auth_scheme, k)
            };
            (self.auth_header.clone(), v)
        })
    }

    pub fn embeddings_url(&self) -> String {
        let trimmed = self.base_url.trim_end_matches('/');
        format!("{trimmed}/embeddings")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(scheme: &str, header: &str, key: Option<&str>) -> OpenAICompatibleEmbedderConfig {
        OpenAICompatibleEmbedderConfig {
            base_url: "https://api.openai.com/v1".to_string(),
            model: "text-embedding-3-small".to_string(),
            api_key: key.map(String::from),
            auth_header: header.to_string(),
            auth_scheme: scheme.to_string(),
            dimension: 1536,
            timeout_ms: 5000,
        }
    }

    #[test]
    fn bearer_authorization() {
        let c = cfg("Bearer", "Authorization", Some("sk-abc"));
        let (h, v) = c.auth_header_value().unwrap();
        assert_eq!(h, "Authorization");
        assert_eq!(v, "Bearer sk-abc");
    }

    #[test]
    fn azure_raw_key_header() {
        let c = cfg("", "api-key", Some("k"));
        let (h, v) = c.auth_header_value().unwrap();
        assert_eq!(h, "api-key");
        assert_eq!(v, "k");
    }

    #[test]
    fn x_api_key_header() {
        let c = cfg("", "X-API-Key", Some("k"));
        let (h, v) = c.auth_header_value().unwrap();
        assert_eq!(h, "X-API-Key");
        assert_eq!(v, "k");
    }

    #[test]
    fn no_api_key_means_no_auth_header() {
        let c = cfg("Bearer", "Authorization", None);
        assert!(c.auth_header_value().is_none());
    }

    #[test]
    fn embeddings_url_trims_trailing_slash() {
        let mut c = cfg("Bearer", "Authorization", Some("k"));
        c.base_url = "https://x.example.com/v1/".to_string();
        assert_eq!(c.embeddings_url(), "https://x.example.com/v1/embeddings");
    }
}

/// PDK-flavored implementation that any variant binary can share. Calls
/// any OpenAI-compatible `/v1/embeddings` endpoint via the PDK
/// `HttpClient`. The variant supplies the parsed `Service` (from its
/// generated `gcl.yaml` config) and a per-request `Rc<HttpClient>`.
pub struct PdkOpenAICompatibleEmbedder {
    base_url: Service,
    model: String,
    api_key: Option<String>,
    auth_header: String,
    auth_scheme: String,
    dimension: usize,
    timeout_ms: u64,
    http: Rc<HttpClient>,
}

impl PdkOpenAICompatibleEmbedder {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        base_url: Service,
        model: String,
        api_key: Option<String>,
        auth_header: String,
        auth_scheme: String,
        dimension: usize,
        timeout_ms: u64,
        http: Rc<HttpClient>,
    ) -> Self {
        Self {
            base_url,
            model,
            api_key,
            auth_header,
            auth_scheme,
            dimension,
            timeout_ms,
            http,
        }
    }
}

#[async_trait(?Send)]
impl Embedder for PdkOpenAICompatibleEmbedder {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        // Send `dimensions: <dimension>` so OpenAI
        // (`-3-small` / `-3-large`) truncates the returned vector to the
        // size the operator configured in api.yaml — must match the
        // vector store dimension. Servers that do not support the field
        // (older OpenAI models, TEI / vLLM) generally ignore unknown
        // request keys; if they hard-reject, the policy logs the error
        // via the bypass-error path and falls open.
        let body = serde_json::json!({
            "model": self.model,
            "input": text,
            "dimensions": self.dimension,
        })
        .to_string();
        let auth_value = self.api_key.as_ref().map(|k| {
            if self.auth_scheme.is_empty() {
                k.clone()
            } else {
                format!("{} {}", self.auth_scheme, k)
            }
        });

        let mut headers: Vec<(&str, &str)> = vec![("content-type", "application/json")];
        if let Some(ref v) = auth_value {
            headers.push((self.auth_header.as_str(), v.as_str()));
        }

        // PDK's `.path()` REPLACES the Service URI path; we want to APPEND
        // `/embeddings` after whatever prefix the operator put in `baseUrl`
        // (e.g. `http://x/v1` + `/embeddings` → `/v1/embeddings`).
        let base = self.base_url.uri().path().trim_end_matches('/');
        let full_path = format!("{base}/embeddings");
        let resp = self
            .http
            .request(&self.base_url)
            .path(&full_path)
            .headers(headers)
            .body(body.as_bytes())
            .timeout(Duration::from_millis(self.timeout_ms))
            .post()
            .await
            .map_err(|e| EmbedError::Upstream {
                status: 0,
                body: format!("{e:?}"),
            })?;

        let status = resp.status_code() as u16;
        if status == 401 {
            return Err(EmbedError::Unauthorized);
        }
        if !(200..300).contains(&status) {
            return Err(EmbedError::Upstream {
                status,
                body: String::from_utf8_lossy(resp.body()).to_string(),
            });
        }

        let v: serde_json::Value = serde_json::from_slice(resp.body())
            .map_err(|e| EmbedError::Malformed(e.to_string()))?;
        let arr = v
            .get("data")
            .and_then(|d| d.as_array())
            .and_then(|a| a.first())
            .and_then(|el| el.get("embedding"))
            .and_then(|e| e.as_array())
            .ok_or_else(|| EmbedError::Malformed("missing data[0].embedding".into()))?;
        let vec: Vec<f32> = arr
            .iter()
            .filter_map(|v| v.as_f64().map(|f| f as f32))
            .collect();
        if vec.len() != self.dimension {
            return Err(EmbedError::DimensionMismatch {
                expected: self.dimension,
                got: vec.len(),
            });
        }
        Ok(vec)
    }

    fn dimension(&self) -> usize {
        self.dimension
    }
}
