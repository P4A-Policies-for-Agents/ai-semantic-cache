// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Salesforce, Inc.

//! ai-semantic-cache-policy-openai-azure-ai-search — variant binary.
//!
//! All shared lifecycle plumbing (request/response filters, exact-cache
//! KV path, header injection, embedder) lives in
//! `ai_semantic_cache_core::driver`. This crate ships only:
//! - `PdkAzureAiSearchStore`: PDK-flavored `VectorStore` impl targeting
//!   the Azure AI Search REST API
//!   (`POST /indexes/<name>/docs/index?api-version=…` for upsert/delete,
//!    `POST /indexes/<name>/docs/search?api-version=…` for vector search,
//!    `GET /indexes/<name>/docs/<id>?api-version=…` for exact lookup).
//! - `#[entrypoint]` that wires the generated `Config` into
//!   `driver::run`.

mod generated;

#[cfg(test)]
mod tests;

use crate::generated::config::{
    Config, EmbedderConfig as GenEmbedder, VectordbConfig as GenStore,
};
use ai_semantic_cache_core::{
    driver::{self, DriverConfig},
    embed::{Embedder, PdkOpenAICompatibleEmbedder},
    error::StoreError,
    key::CacheKey,
    store::{CachedPayload, Match, VectorStore},
};
use anyhow::Result;
use async_trait::async_trait;
use pdk::data_storage::DataStorageBuilder;
use pdk::hl::*;
use pdk::policy_violation::PolicyViolations;
use std::rc::Rc;
use std::time::Duration;

const POLICY_NAME: &str = "ai-semantic-cache-policy-openai-azure-ai-search";
const DEFAULT_VECTOR_FIELD: &str = "embedding";
const DEFAULT_API_VERSION: &str = "2024-07-01";

#[entrypoint]
async fn configure(
    launcher: Launcher,
    Configuration(bytes): Configuration,
    policy_violations: PolicyViolations,
    store_builder: DataStorageBuilder,
) -> Result<()> {
    let cfg: Config = serde_json::from_slice(&bytes)?;
    driver::reject_service_path_with_suffix(
        "embedder.baseUrl",
        cfg.embedder.base_url.uri().path(),
        &["/embeddings", "/v1/embeddings"],
    )?;
    // Azure AI Search endpoint should be the service root; the helper
    // appends /indexes/<name>/...
    driver::reject_service_path_with_suffix(
        "vectordb.endpoint",
        cfg.vectordb.endpoint.uri().path(),
        &["/indexes", "/docs"],
    )?;
    let driver_cfg = build_driver_config(&cfg);
    let embedder_factory = build_embedder_factory(cfg.embedder.clone());
    let store_factory = build_store_factory(cfg.vectordb.clone());
    driver::run(
        launcher,
        driver_cfg,
        store_builder,
        policy_violations,
        embedder_factory,
        store_factory,
    )
    .await
}

fn build_driver_config(cfg: &Config) -> DriverConfig {
    DriverConfig {
        policy_name: POLICY_NAME,
        namespace: cfg.namespace.clone(),
        similarity_threshold: cfg.similarity_threshold.unwrap_or(0.92) as f32,
        ttl_seconds: cfg.ttl_seconds.unwrap_or(3600) as u64,
        exact_caching: cfg.exact_caching.unwrap_or(true),
        bypass_on_stream: cfg.bypass_on_stream.unwrap_or(true),
        prompt_extractor: cfg
            .prompt_extractor
            .clone()
            .unwrap_or_else(|| "openai".to_string()),
        cache_response_status_codes: cfg
            .cache_response_status_codes
            .clone()
            .unwrap_or_else(|| vec![200])
            .into_iter()
            .map(|c| c as u16)
            .collect(),
        failure_mode: cfg
            .failure_mode
            .clone()
            .unwrap_or_else(|| "fail_open".to_string()),
    }
}

fn build_embedder_factory(emb: GenEmbedder) -> driver::EmbedderFactory {
    Rc::new(move |http: Rc<HttpClient>| -> Rc<dyn Embedder> {
        Rc::new(PdkOpenAICompatibleEmbedder::new(
            emb.base_url.clone(),
            emb.model.clone(),
            emb.api_key.clone(),
            emb.auth_header.clone().unwrap_or_else(|| "Authorization".into()),
            emb.auth_scheme.clone().unwrap_or_else(|| "Bearer".into()),
            emb.dimension as usize,
            emb.timeout_ms.unwrap_or(5000) as u64,
            http,
        ))
    })
}

fn build_store_factory(store_cfg: GenStore) -> driver::StoreFactory {
    Rc::new(move |http: Rc<HttpClient>| -> Rc<dyn VectorStore> {
        Rc::new(PdkAzureAiSearchStore::new(store_cfg.clone(), http))
    })
}

// =====================================================================
// PDK-flavored Azure AI Search store (REST API)
// =====================================================================
//
// Endpoint surface (Azure AI Search 2024-07-01):
//   POST   /indexes/{name}/docs/index?api-version=…   (upsert + delete via @search.action)
//   POST   /indexes/{name}/docs/search?api-version=…  (vector + filter)
//   GET    /indexes/{name}/docs/{id}?api-version=…    (exact lookup)
//
// Document IDs must be valid Edm.String keys (URL-safe). We build the
// id from the composite cache key components, separated by underscores
// so the result matches `[A-Za-z0-9_\-=]+`.

struct PdkAzureAiSearchStore {
    cfg: GenStore,
    http: Rc<HttpClient>,
}

impl PdkAzureAiSearchStore {
    fn new(cfg: GenStore, http: Rc<HttpClient>) -> Self {
        Self { cfg, http }
    }

    fn timeout(&self) -> Duration {
        Duration::from_millis(self.cfg.timeout_ms.unwrap_or(5000) as u64)
    }

    fn vector_field(&self) -> &str {
        self.cfg
            .vector_field
            .as_deref()
            .unwrap_or(DEFAULT_VECTOR_FIELD)
    }

    fn api_version(&self) -> &str {
        self.cfg
            .api_version
            .as_deref()
            .unwrap_or(DEFAULT_API_VERSION)
    }

    fn auth_headers<'a>(&'a self) -> Vec<(&'a str, &'a str)> {
        vec![
            ("content-type", "application/json"),
            ("api-key", self.cfg.api_key.as_str()),
        ]
    }

    fn full_path(&self, suffix: &str) -> String {
        let base = self.cfg.endpoint.uri().path().trim_end_matches('/');
        let index = &self.cfg.index_name;
        if base.is_empty() {
            format!("/indexes/{index}{suffix}")
        } else {
            format!("{base}/indexes/{index}{suffix}")
        }
    }

    fn doc_id(key: &CacheKey) -> String {
        // Azure AI Search keys: `[A-Za-z0-9_\-=]+`. We use `_` as the
        // component separator since none of namespace/model/prompt_sha
        // are guaranteed to be Azure-key-safe by themselves; replace any
        // ":" that may have leaked in from operator-supplied namespace.
        format!("{}_{}_{}", key.namespace, key.model, key.prompt_sha)
            .replace(':', "_")
    }

    async fn json_post(
        &self,
        path: &str,
        body: serde_json::Value,
    ) -> Result<serde_json::Value, StoreError> {
        let full = self.full_path(path);
        let bytes = body.to_string();
        let resp = self
            .http
            .request(&self.cfg.endpoint)
            .path(&full)
            .headers(self.auth_headers())
            .body(bytes.as_bytes())
            .timeout(self.timeout())
            .post()
            .await
            .map_err(|e| StoreError::Upstream {
                status: 0,
                body: format!("{e:?}"),
            })?;
        let status = resp.status_code() as u16;
        if !(200..300).contains(&status) {
            return Err(StoreError::Upstream {
                status,
                body: String::from_utf8_lossy(resp.body()).to_string(),
            });
        }
        if resp.body().is_empty() {
            return Ok(serde_json::Value::Null);
        }
        serde_json::from_slice(resp.body())
            .map_err(|e| StoreError::Malformed(e.to_string()))
    }
}

#[async_trait(?Send)]
impl VectorStore for PdkAzureAiSearchStore {
    /// `POST /indexes/<name>/docs/index?api-version=…` with a single
    /// `mergeOrUpload` action. The document carries `namespace`, `model`,
    /// `prompt_sha`, and the cached HTTP response (as a JSON-encoded
    /// string field).
    async fn upsert(
        &self,
        key: &CacheKey,
        vector: &[f32],
        payload: &CachedPayload,
        _ttl: Duration,
    ) -> Result<(), StoreError> {
        let payload_json = serde_json::to_string(payload)
            .map_err(|e| StoreError::Malformed(e.to_string()))?;
        let id = Self::doc_id(key);
        let vfield = self.vector_field();
        let mut doc = serde_json::Map::new();
        doc.insert(
            "@search.action".into(),
            serde_json::Value::String("mergeOrUpload".into()),
        );
        doc.insert("id".into(), serde_json::Value::String(id));
        doc.insert(
            "namespace".into(),
            serde_json::Value::String(key.namespace.clone()),
        );
        doc.insert(
            "model".into(),
            serde_json::Value::String(key.model.clone()),
        );
        doc.insert(
            "prompt_sha".into(),
            serde_json::Value::String(key.prompt_sha.clone()),
        );
        doc.insert("cached".into(), serde_json::Value::String(payload_json));
        let vec_json: Vec<serde_json::Value> =
            vector.iter().map(|f| serde_json::json!(*f as f64)).collect();
        doc.insert(vfield.to_string(), serde_json::Value::Array(vec_json));
        let body = serde_json::json!({ "value": [doc] });
        let path = format!("/docs/index?api-version={}", self.api_version());
        self.json_post(&path, body).await?;
        Ok(())
    }

    /// `POST /indexes/<name>/docs/search?api-version=…` with `vectorQueries`
    /// + an OData `$filter` on `(namespace, model)`.
    async fn query_topk(
        &self,
        namespace: &str,
        model: &str,
        vector: &[f32],
        k: usize,
    ) -> Result<Vec<Match>, StoreError> {
        let vfield = self.vector_field();
        let filter = format!(
            "namespace eq '{}' and model eq '{}'",
            escape_odata(namespace),
            escape_odata(model)
        );
        let body = serde_json::json!({
            "count": false,
            "select": "id,namespace,model,prompt_sha,cached",
            "filter": filter,
            "vectorQueries": [{
                "kind": "vector",
                "vector": vector,
                "fields": vfield,
                "k": k,
            }]
        });
        let path = format!("/docs/search?api-version={}", self.api_version());
        let resp = self.json_post(&path, body).await?;
        let arr = resp
            .get("value")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let mut out = Vec::with_capacity(arr.len());
        for d in arr {
            let score = d
                .get("@search.score")
                .and_then(|v| v.as_f64())
                .map(|f| f as f32)
                .unwrap_or(0.0);
            let cached_json = d.get("cached").and_then(|c| c.as_str());
            let Some(cached_json) = cached_json else {
                continue;
            };
            let payload: CachedPayload = match serde_json::from_str(cached_json) {
                Ok(p) => p,
                Err(_) => continue,
            };
            let ns = d
                .get("namespace")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let mdl = d
                .get("model")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let sha = d
                .get("prompt_sha")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let key = CacheKey {
                namespace: ns,
                model: mdl,
                prompt_sha: sha,
            };
            out.push(Match { key, score, payload });
        }
        Ok(out)
    }

    /// `GET /indexes/<name>/docs/<id>?api-version=…`.
    async fn get_exact(&self, key: &CacheKey) -> Result<Option<CachedPayload>, StoreError> {
        let id = Self::doc_id(key);
        let path = self.full_path(&format!(
            "/docs/{}?api-version={}",
            urlencode(&id),
            self.api_version()
        ));
        let resp = self
            .http
            .request(&self.cfg.endpoint)
            .path(&path)
            .headers(self.auth_headers())
            .timeout(self.timeout())
            .get()
            .await
            .map_err(|e| StoreError::Upstream {
                status: 0,
                body: format!("{e:?}"),
            })?;
        let status = resp.status_code() as u16;
        if status == 404 {
            return Ok(None);
        }
        if !(200..300).contains(&status) {
            return Err(StoreError::Upstream {
                status,
                body: String::from_utf8_lossy(resp.body()).to_string(),
            });
        }
        let v: serde_json::Value = serde_json::from_slice(resp.body())
            .map_err(|e| StoreError::Malformed(e.to_string()))?;
        let cached_json = v.get("cached").and_then(|c| c.as_str());
        let Some(cached_json) = cached_json else {
            return Ok(None);
        };
        let p: CachedPayload = serde_json::from_str(cached_json)
            .map_err(|e| StoreError::Malformed(e.to_string()))?;
        Ok(Some(p))
    }

    async fn delete(&self, key: &CacheKey) -> Result<(), StoreError> {
        let id = Self::doc_id(key);
        let body = serde_json::json!({
            "value": [{ "@search.action": "delete", "id": id }]
        });
        let path = format!("/docs/index?api-version={}", self.api_version());
        self.json_post(&path, body).await?;
        Ok(())
    }
}

fn escape_odata(s: &str) -> String {
    s.replace('\'', "''")
}

// Hand-rolled percent encoder for Azure AI Search's doc-id charset
// (`[A-Za-z0-9_\-=]+`). Sufficient for our composite IDs; not a
// general-purpose URL encoder.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
