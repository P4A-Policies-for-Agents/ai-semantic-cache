// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Salesforce, Inc.

//! ai-semantic-cache-policy-openai-pinecone — variant binary.
//!
//! All shared lifecycle plumbing (request/response filters, exact-cache
//! KV path, header injection, embedder) lives in
//! `ai_semantic_cache_core::driver`. This crate ships only:
//! - `PdkPineconeStore`: PDK-flavored `VectorStore` impl targeting the
//!   Pinecone serverless REST API (`/vectors/upsert`, `/query` with
//!   metadata filter, `/vectors/fetch`, `/vectors/delete`).
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

const POLICY_NAME: &str = "ai-semantic-cache-policy-openai-pinecone";

#[entrypoint]
async fn configure(
    launcher: Launcher,
    Configuration(bytes): Configuration,
    policy_violations: PolicyViolations,
    store_builder: DataStorageBuilder,
) -> Result<()> {
    let cfg: Config = serde_json::from_slice(&bytes)?;
    // Reject misconfigured Service URLs that would cause double-suffix
    // request paths (e.g. baseUrl ending in `/embeddings`). The helper
    // appends suffixes to the Service URI path; see
    // `driver::reject_service_path_with_suffix` for rationale.
    driver::reject_service_path_with_suffix(
        "embedder.baseUrl",
        cfg.embedder.base_url.uri().path(),
        &["/embeddings", "/v1/embeddings"],
    )?;
    driver::reject_service_path_with_suffix(
        "vectordb.indexHost",
        cfg.vectordb.index_host.uri().path(),
        &["/query", "/vectors/upsert", "/vectors/fetch", "/vectors/delete"],
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
        Rc::new(PdkPineconeStore::new(store_cfg.clone(), http))
    })
}

// =====================================================================
// PDK-flavored PineconeStore (REST API)
// =====================================================================

struct PdkPineconeStore {
    cfg: GenStore,
    http: Rc<HttpClient>,
}

impl PdkPineconeStore {
    fn new(cfg: GenStore, http: Rc<HttpClient>) -> Self {
        Self { cfg, http }
    }

    fn timeout(&self) -> Duration {
        Duration::from_millis(self.cfg.timeout_ms.unwrap_or(5000) as u64)
    }

    fn full_path(&self, path: &str) -> String {
        let base = self.cfg.index_host.uri().path().trim_end_matches('/');
        if base.is_empty() {
            path.to_string()
        } else {
            format!("{base}{path}")
        }
    }

    async fn json_post(
        &self,
        path: &str,
        body: serde_json::Value,
    ) -> Result<serde_json::Value, StoreError> {
        let bytes = body.to_string();
        let full = self.full_path(path);
        let resp = self
            .http
            .request(&self.cfg.index_host)
            .path(&full)
            .headers(vec![
                ("content-type", "application/json"),
                ("Api-Key", self.cfg.api_key.as_str()),
                ("X-Pinecone-API-Version", "2025-01"),
            ])
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
impl VectorStore for PdkPineconeStore {
    /// `POST /vectors/upsert` with one vector. The vector ID is the composite
    /// cache key; metadata carries `namespace` and `model` so `query_topk`
    /// can filter on them.
    async fn upsert(
        &self,
        key: &CacheKey,
        vector: &[f32],
        payload: &CachedPayload,
        _ttl: Duration,
    ) -> Result<(), StoreError> {
        let payload_json = serde_json::to_string(payload)
            .map_err(|e| StoreError::Malformed(e.to_string()))?;
        let id = key.as_redis_key(); // re-use the deterministic composite id
        let body = serde_json::json!({
            "vectors": [{
                "id": id,
                "values": vector,
                "metadata": {
                    "namespace": key.namespace,
                    "model": key.model,
                    "payload": payload_json,
                }
            }]
        });
        self.json_post("/vectors/upsert", body).await?;
        Ok(())
    }

    /// `POST /query` with metadata filter on `(namespace, model)` and `topK = k`.
    async fn query_topk(
        &self,
        namespace: &str,
        model: &str,
        vector: &[f32],
        k: usize,
    ) -> Result<Vec<Match>, StoreError> {
        let body = serde_json::json!({
            "vector": vector,
            "topK": k,
            "includeMetadata": true,
            "filter": {
                "namespace": { "$eq": namespace },
                "model": { "$eq": model }
            }
        });
        let resp = self.json_post("/query", body).await?;
        let matches = resp
            .get("matches")
            .and_then(|m| m.as_array())
            .cloned()
            .unwrap_or_default();
        let mut out = Vec::with_capacity(matches.len());
        for m in matches {
            let id = m.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let score = m
                .get("score")
                .and_then(|v| v.as_f64())
                .map(|f| f as f32)
                .unwrap_or(0.0);
            let payload_json = m
                .get("metadata")
                .and_then(|md| md.get("payload"))
                .and_then(|p| p.as_str());
            let Some(payload_json) = payload_json else { continue };
            let payload: CachedPayload = match serde_json::from_str(payload_json) {
                Ok(p) => p,
                Err(_) => continue,
            };
            // Re-derive a CacheKey for the match. Pinecone gives us the id;
            // split on ":" to recover (namespace, model, sha).
            let mut parts = id.splitn(3, ':');
            let ns = parts.next().unwrap_or("").to_string();
            let mdl = parts.next().unwrap_or("").to_string();
            let sha = parts.next().unwrap_or("").to_string();
            let key = CacheKey {
                namespace: ns,
                model: mdl,
                prompt_sha: sha,
            };
            out.push(Match { key, score, payload });
        }
        Ok(out)
    }

    /// `GET /vectors/fetch?ids=<id>`. Pinecone returns
    /// `{vectors: {<id>: {metadata: {...}}}}`.
    async fn get_exact(&self, key: &CacheKey) -> Result<Option<CachedPayload>, StoreError> {
        let id = key.as_redis_key();
        let path = self.full_path(&format!("/vectors/fetch?ids={}", id));
        let resp = self
            .http
            .request(&self.cfg.index_host)
            .path(&path)
            .headers(vec![
                ("Api-Key", self.cfg.api_key.as_str()),
                ("X-Pinecone-API-Version", "2025-01"),
            ])
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
        let body: serde_json::Value = serde_json::from_slice(resp.body())
            .map_err(|e| StoreError::Malformed(e.to_string()))?;
        let payload_json = body
            .get("vectors")
            .and_then(|vs| vs.get(&id))
            .and_then(|v| v.get("metadata"))
            .and_then(|md| md.get("payload"))
            .and_then(|p| p.as_str());
        let Some(payload_json) = payload_json else {
            return Ok(None);
        };
        let p: CachedPayload = serde_json::from_str(payload_json)
            .map_err(|e| StoreError::Malformed(e.to_string()))?;
        Ok(Some(p))
    }

    async fn delete(&self, key: &CacheKey) -> Result<(), StoreError> {
        let body = serde_json::json!({ "ids": [ key.as_redis_key() ] });
        self.json_post("/vectors/delete", body).await?;
        Ok(())
    }
}
