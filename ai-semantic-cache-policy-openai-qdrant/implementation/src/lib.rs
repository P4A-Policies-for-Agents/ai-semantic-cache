// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Salesforce, Inc.

//! ai-semantic-cache-policy-openai-qdrant — variant binary.
//!
//! All shared lifecycle plumbing (request/response filters, exact-cache
//! KV path, header injection, embedder) lives in
//! `ai_semantic_cache_core::driver`. This crate ships only:
//! - `PdkQdrantStore`: PDK-flavored `VectorStore` impl targeting the
//!   Qdrant REST API
//!   (`PUT /collections/<name>/points`,
//!    `POST /collections/<name>/points/search` with payload filter,
//!    `POST /collections/<name>/points` for fetch,
//!    `POST /collections/<name>/points/delete`).
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

const POLICY_NAME: &str = "ai-semantic-cache-policy-openai-qdrant";

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
    // Qdrant baseUrl should be the cluster root; the helper appends
    // /collections/<name>/... so `/collections/foo` already in baseUrl
    // would produce a doubled segment.
    driver::reject_service_path_with_suffix(
        "vectordb.baseUrl",
        cfg.vectordb.base_url.uri().path(),
        &["/collections", "/points", "/points/search"],
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
        Rc::new(PdkQdrantStore::new(store_cfg.clone(), http))
    })
}

// =====================================================================
// PDK-flavored QdrantStore (REST API)
// =====================================================================
//
// Endpoint surface (Qdrant 1.x):
//   PUT  /collections/{name}/points?wait=true
//   POST /collections/{name}/points/search
//   POST /collections/{name}/points
//   POST /collections/{name}/points/delete
//
// Qdrant requires numeric or UUID point IDs (no arbitrary strings), so we
// hash the composite cache key (`namespace:model:prompt_sha`) into a u64
// for the `id` field and store the original components in `payload` so
// the engine can reconstruct a CacheKey on hit.

struct PdkQdrantStore {
    cfg: GenStore,
    http: Rc<HttpClient>,
}

impl PdkQdrantStore {
    fn new(cfg: GenStore, http: Rc<HttpClient>) -> Self {
        Self { cfg, http }
    }

    fn timeout(&self) -> Duration {
        Duration::from_millis(self.cfg.timeout_ms.unwrap_or(5000) as u64)
    }

    fn auth_headers<'a>(&'a self) -> Vec<(&'a str, &'a str)> {
        let mut h = vec![("content-type", "application/json")];
        if let Some(k) = self.cfg.api_key.as_deref() {
            h.push(("api-key", k));
        }
        h
    }

    fn full_path(&self, suffix: &str) -> String {
        let base = self.cfg.base_url.uri().path().trim_end_matches('/');
        let collection = &self.cfg.collection;
        if base.is_empty() {
            format!("/collections/{collection}{suffix}")
        } else {
            format!("{base}/collections/{collection}{suffix}")
        }
    }

    /// Derive a deterministic UUID-shaped point id from the composite
    /// cache key. Qdrant accepts numeric or UUID ids; UUID gives us
    /// 128 bits of entropy from SHA-256 instead of the 64-bit prefix
    /// the original `point_id` used (which had a ~2.7% birthday-
    /// collision chance at 10M entries — silently catastrophic for a
    /// cache, since a collision returns the wrong cached response).
    ///
    /// The bytes come from SHA-256 of `namespace:model:prompt_sha`;
    /// version (5) and variant (RFC 4122) bits are forced so the result
    /// passes Qdrant's UUID parser. The construction is deterministic
    /// and stable: same input → same point id.
    pub(crate) fn point_id(key: &CacheKey) -> String {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(key.namespace.as_bytes());
        h.update(b":");
        h.update(key.model.as_bytes());
        h.update(b":");
        h.update(key.prompt_sha.as_bytes());
        let digest = h.finalize();
        let mut bytes = [0u8; 16];
        bytes.copy_from_slice(&digest[..16]);
        // RFC 4122 §4.1.3: set version to 5 (name-based, SHA-1) — we
        // use SHA-256 for deeper entropy, but the version field is just
        // a metadata flag; Qdrant doesn't validate the underlying hash.
        bytes[6] = (bytes[6] & 0x0f) | 0x50;
        // RFC 4122 §4.1.1: set variant to RFC 4122 (10xxxxxx).
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        format!(
            "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            bytes[0], bytes[1], bytes[2], bytes[3],
            bytes[4], bytes[5],
            bytes[6], bytes[7],
            bytes[8], bytes[9],
            bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
        )
    }

    async fn json_call(
        &self,
        method: &str,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, StoreError> {
        let full = self.full_path(path);
        let mut builder = self
            .http
            .request(&self.cfg.base_url)
            .path(&full)
            .headers(self.auth_headers())
            .timeout(self.timeout());
        let body_string;
        if let Some(b) = body {
            body_string = b.to_string();
            builder = builder.body(body_string.as_bytes());
        }
        let resp = match method {
            "POST" => builder.post().await,
            "PUT" => builder.put().await,
            other => return Err(StoreError::Malformed(format!("unsupported method {other}"))),
        }
        .map_err(|e| StoreError::Upstream {
            status: 0,
            body: format!("{e:?}"),
        })?;
        let status = resp.status_code() as u16;
        if status == 404 {
            return Ok(serde_json::Value::Null);
        }
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
impl VectorStore for PdkQdrantStore {
    /// `PUT /collections/{name}/points?wait=true` with one point. The
    /// point's `payload` carries `namespace`, `model`, `prompt_sha`, and
    /// the cached HTTP response (as a JSON-encoded string).
    async fn upsert(
        &self,
        key: &CacheKey,
        vector: &[f32],
        payload: &CachedPayload,
        _ttl: Duration,
    ) -> Result<(), StoreError> {
        let payload_json = serde_json::to_string(payload)
            .map_err(|e| StoreError::Malformed(e.to_string()))?;
        let id = Self::point_id(key);
        let body = serde_json::json!({
            "points": [{
                "id": id,
                "vector": vector,
                "payload": {
                    "namespace": key.namespace,
                    "model": key.model,
                    "prompt_sha": key.prompt_sha,
                    "cached": payload_json,
                }
            }]
        });
        self.json_call("PUT", "/points?wait=true", Some(body)).await?;
        Ok(())
    }

    /// `POST /collections/{name}/points/search` with payload filter on
    /// `(namespace, model)` and `limit = k`.
    async fn query_topk(
        &self,
        namespace: &str,
        model: &str,
        vector: &[f32],
        k: usize,
    ) -> Result<Vec<Match>, StoreError> {
        let body = serde_json::json!({
            "vector": vector,
            "limit": k,
            "with_payload": true,
            "filter": {
                "must": [
                    {"key": "namespace", "match": {"value": namespace}},
                    {"key": "model",     "match": {"value": model}},
                ]
            }
        });
        let resp = self.json_call("POST", "/points/search", Some(body)).await?;
        let matches = resp
            .get("result")
            .and_then(|r| r.as_array())
            .cloned()
            .unwrap_or_default();
        let mut out = Vec::with_capacity(matches.len());
        for m in matches {
            let score = m
                .get("score")
                .and_then(|v| v.as_f64())
                .map(|f| f as f32)
                .unwrap_or(0.0);
            let pl = match m.get("payload") {
                Some(p) => p,
                None => continue,
            };
            let cached_json = match pl.get("cached").and_then(|c| c.as_str()) {
                Some(s) => s,
                None => continue,
            };
            let cached_payload: CachedPayload = match serde_json::from_str(cached_json) {
                Ok(p) => p,
                Err(_) => continue,
            };
            let ns = pl
                .get("namespace")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let mdl = pl
                .get("model")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let sha = pl
                .get("prompt_sha")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let cache_key = CacheKey {
                namespace: ns,
                model: mdl,
                prompt_sha: sha,
            };
            out.push(Match {
                key: cache_key,
                score,
                payload: cached_payload,
            });
        }
        Ok(out)
    }

    /// `POST /collections/{name}/points` with
    /// `{ ids: [<id>], with_payload: true }`.
    async fn get_exact(&self, key: &CacheKey) -> Result<Option<CachedPayload>, StoreError> {
        let id = Self::point_id(key);
        let body = serde_json::json!({ "ids": [id], "with_payload": true });
        let resp = self.json_call("POST", "/points", Some(body)).await?;
        let arr = resp
            .get("result")
            .and_then(|r| r.as_array())
            .cloned()
            .unwrap_or_default();
        let Some(point) = arr.first() else {
            return Ok(None);
        };
        let cached_json = point
            .get("payload")
            .and_then(|p| p.get("cached"))
            .and_then(|c| c.as_str());
        let Some(cached_json) = cached_json else {
            return Ok(None);
        };
        let p: CachedPayload = serde_json::from_str(cached_json)
            .map_err(|e| StoreError::Malformed(e.to_string()))?;
        Ok(Some(p))
    }

    async fn delete(&self, key: &CacheKey) -> Result<(), StoreError> {
        let id = Self::point_id(key);
        let body = serde_json::json!({ "points": [id] });
        self.json_call("POST", "/points/delete?wait=true", Some(body)).await?;
        Ok(())
    }
}
