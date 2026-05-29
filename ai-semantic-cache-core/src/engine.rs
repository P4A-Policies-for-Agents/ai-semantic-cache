// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Salesforce, Inc.

use crate::config::{FailureMode, Threshold, Ttl};
use crate::embed::Embedder;
use crate::error::StoreError;
use crate::extract::{ExtractOutcome, PromptExtractor};
use crate::key::CacheKey;
use crate::replay::{build_replay, CacheEvent, ReplayResponse};
use crate::store::{CachedPayload, VectorStore};
use pdk::logger;
use serde_json::Value;
use std::rc::Rc;
use std::time::SystemTime;

/// Headers stripped from upstream responses before persistence + replay.
/// Lower-cased exact match. Hop-by-hop and auth-bearing headers must never round-trip
/// through a shared cache.
const SENSITIVE_HEADERS: &[&str] = &[
    "authorization",
    "proxy-authenticate",
    "proxy-authorization",
    "www-authenticate",
    "set-cookie",
    "cookie",
    "connection",
    "keep-alive",
    "transfer-encoding",
    "te",
    "trailer",
    "upgrade",
];

fn sanitize_headers(headers: &[(String, String)]) -> Vec<(String, String)> {
    headers
        .iter()
        .filter(|(k, _)| {
            let lower = k.to_ascii_lowercase();
            !SENSITIVE_HEADERS.contains(&lower.as_str())
        })
        .cloned()
        .collect()
}

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub namespace: String,
    pub similarity_threshold: Threshold,
    pub ttl: Ttl,
    pub exact_caching: bool,
    pub bypass_on_stream: bool,
    pub cache_response_status_codes: Vec<u16>,
    pub failure_mode: FailureMode,
}

pub struct Engine {
    pub config: EngineConfig,
    pub embedder: Rc<dyn Embedder>,
    pub store: Rc<dyn VectorStore>,
    pub extractor: PromptExtractor,
}

#[derive(Debug)]
pub enum RequestDecision {
    /// Forward upstream; on response, possibly write the entry.
    Forward { key: CacheKey, embedding: Option<Vec<f32>> },
    /// Forward upstream and do not write back.
    ForwardNoWrite { event: CacheEvent },
    /// Short-circuit with a cached payload.
    ShortCircuit { response: ReplayResponse, event: CacheEvent },
    /// Return a 503 immediately (fail_closed terminal error).
    FailClosed { reason: String },
}

impl Engine {
    pub async fn on_request(&self, body: &[u8]) -> RequestDecision {
        let parsed: Value = match serde_json::from_slice(body) {
            Ok(v) => v,
            Err(_) => return RequestDecision::ForwardNoWrite { event: CacheEvent::SkipUnparseable },
        };
        let extracted = match self.extractor.extract(&parsed) {
            ExtractOutcome::Ok(e) => e,
            ExtractOutcome::Skip(_) => return RequestDecision::ForwardNoWrite { event: CacheEvent::SkipUnparseable },
        };
        if self.config.bypass_on_stream && extracted.stream {
            return RequestDecision::ForwardNoWrite { event: CacheEvent::BypassStream };
        }
        let key = CacheKey::new(&self.config.namespace, &extracted.model, &extracted.prompt);

        if self.config.exact_caching {
            match self.store.get_exact(&key).await {
                Ok(Some(payload)) => {
                    let resp = build_replay(&payload, CacheEvent::HitExact, None, &self.config.namespace, &key.correlation_prefix());
                    return RequestDecision::ShortCircuit { response: resp, event: CacheEvent::HitExact };
                }
                Ok(None) => {}
                Err(e) => {
                    if self.config.failure_mode == FailureMode::FailClosed {
                        return RequestDecision::FailClosed { reason: format!("store: {e}") };
                    }
                    logger::warn!("exact-cache get failed (fail-open): {e}");
                }
            }
        }

        let embedding = match self.embedder.embed(extracted.prompt.as_str()).await {
            Ok(v) => v,
            Err(e) => {
                if self.config.failure_mode == FailureMode::FailClosed {
                    return RequestDecision::FailClosed { reason: format!("embedder: {e}") };
                }
                logger::warn!("bypass-error: embedder failed: {e}");
                return RequestDecision::ForwardNoWrite { event: CacheEvent::BypassError };
            }
        };

        match self.store.query_topk(&self.config.namespace, &extracted.model, &embedding, 1).await {
            Ok(matches) => {
                if let Some(top) = matches.first() {
                    if top.score >= self.config.similarity_threshold.value() {
                        let resp = build_replay(
                            &top.payload,
                            CacheEvent::HitSemantic,
                            Some(top.score),
                            &self.config.namespace,
                            &top.key.correlation_prefix(),
                        );
                        return RequestDecision::ShortCircuit { response: resp, event: CacheEvent::HitSemantic };
                    }
                }
            }
            Err(e) => {
                if self.config.failure_mode == FailureMode::FailClosed {
                    return RequestDecision::FailClosed { reason: format!("store: {e}") };
                }
                logger::warn!("bypass-error: vector query failed: {e}");
                return RequestDecision::ForwardNoWrite { event: CacheEvent::BypassError };
            }
        }

        RequestDecision::Forward { key, embedding: Some(embedding) }
    }

    /// Best-effort write of the upstream response back to the store.
    ///
    /// Returns `Ok(())` when no write was attempted (decision was not `Forward`, or status not
    /// in the allow-list) or when the write succeeded. Returns `Err(StoreError)` when the
    /// upstream response was eligible to cache but the upsert failed — the caller decides
    /// whether to log, surface, or ignore. The client response is unaffected either way.
    pub async fn on_response(
        &self,
        decision: &RequestDecision,
        upstream_status: u16,
        upstream_headers: &[(String, String)],
        upstream_body: &[u8],
        usage_total_tokens: Option<u64>,
    ) -> Result<(), StoreError> {
        let (key, embedding) = match decision {
            RequestDecision::Forward { key, embedding: Some(e) } => (key, e),
            _ => return Ok(()),
        };
        if !self.config.cache_response_status_codes.contains(&upstream_status) {
            return Ok(());
        }
        let payload = CachedPayload {
            status: upstream_status,
            headers: sanitize_headers(upstream_headers),
            body: upstream_body.to_vec(),
            usage_total_tokens,
            written_at: SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        };
        self.store.upsert(key, embedding, &payload, self.config.ttl.as_duration()).await
    }
}

// -------- Test fakes --------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{EmbedError, StoreError};
    use crate::store::Match;
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::time::Duration;

    struct FakeEmbedder { fixed: Vec<f32>, fail: bool }
    #[async_trait(?Send)]
    impl Embedder for FakeEmbedder {
        async fn embed(&self, _text: &str) -> Result<Vec<f32>, EmbedError> {
            if self.fail { Err(EmbedError::Timeout) } else { Ok(self.fixed.clone()) }
        }
        fn dimension(&self) -> usize { self.fixed.len() }
    }

    #[derive(Default)]
    struct FakeStore {
        exact: Mutex<HashMap<String, CachedPayload>>,
        topk: Mutex<HashMap<(String, String), Vec<Match>>>,
        upserts: Mutex<u32>,
        fail_query: bool,
    }
    #[async_trait(?Send)]
    impl VectorStore for FakeStore {
        async fn upsert(&self, _key: &CacheKey, _v: &[f32], _p: &CachedPayload, _ttl: Duration) -> Result<(), StoreError> {
            *self.upserts.lock().unwrap() += 1; Ok(())
        }
        async fn query_topk(&self, ns: &str, model: &str, _v: &[f32], _k: usize) -> Result<Vec<Match>, StoreError> {
            if self.fail_query { return Err(StoreError::Timeout); }
            Ok(self.topk.lock().unwrap().get(&(ns.to_string(), model.to_string())).cloned().unwrap_or_default())
        }
        async fn get_exact(&self, key: &CacheKey) -> Result<Option<CachedPayload>, StoreError> {
            Ok(self.exact.lock().unwrap().get(&key.as_redis_key()).cloned())
        }
        async fn delete(&self, _key: &CacheKey) -> Result<(), StoreError> { Ok(()) }
    }

    fn cfg() -> EngineConfig {
        EngineConfig {
            namespace: "ns".to_string(),
            similarity_threshold: Threshold::new(0.92).unwrap(),
            ttl: Ttl::from_seconds(3600),
            exact_caching: true,
            bypass_on_stream: true,
            cache_response_status_codes: vec![200],
            failure_mode: FailureMode::FailOpen,
        }
    }

    fn engine(emb: FakeEmbedder, st: FakeStore) -> Engine {
        Engine {
            config: cfg(),
            embedder: Rc::new(emb),
            store: Rc::new(st),
            extractor: PromptExtractor::Openai,
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn miss_returns_forward_with_embedding() {
        let body = br#"{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}]}"#;
        let e = engine(FakeEmbedder { fixed: vec![0.1; 4], fail: false }, FakeStore::default());
        match e.on_request(body).await {
            RequestDecision::Forward { embedding: Some(v), .. } => assert_eq!(v.len(), 4),
            other => panic!("expected Forward, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn bypass_on_stream_short_circuits_no_lookup() {
        let body = br#"{"model":"gpt-4o","stream":true,"messages":[{"role":"user","content":"hi"}]}"#;
        let e = engine(FakeEmbedder { fixed: vec![0.1; 4], fail: true }, FakeStore::default());
        match e.on_request(body).await {
            RequestDecision::ForwardNoWrite { event: CacheEvent::BypassStream } => {}
            other => panic!("expected BypassStream, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unparseable_body_short_circuits() {
        let body = b"not json";
        let e = engine(FakeEmbedder { fixed: vec![0.1; 4], fail: false }, FakeStore::default());
        match e.on_request(body).await {
            RequestDecision::ForwardNoWrite { event: CacheEvent::SkipUnparseable } => {}
            other => panic!("expected SkipUnparseable, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fail_open_on_embedder_error_means_forward_no_write() {
        let body = br#"{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}]}"#;
        let e = engine(FakeEmbedder { fixed: vec![0.1; 4], fail: true }, FakeStore::default());
        match e.on_request(body).await {
            RequestDecision::ForwardNoWrite { event: CacheEvent::BypassError } => {}
            other => panic!("expected BypassError, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn fail_closed_on_embedder_error_returns_failclosed() {
        let body = br#"{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}]}"#;
        let mut e = engine(FakeEmbedder { fixed: vec![0.1; 4], fail: true }, FakeStore::default());
        e.config.failure_mode = FailureMode::FailClosed;
        match e.on_request(body).await {
            RequestDecision::FailClosed { .. } => {}
            other => panic!("expected FailClosed, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn semantic_hit_above_threshold() {
        use crate::key::CanonicalPrompt;
        let body = br#"{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}]}"#;
        let store = FakeStore::default();
        let prompt = CanonicalPrompt::from_messages(&[("user", "hi")]);
        let key = CacheKey::new("ns", "gpt-4o", &prompt);
        store.topk.lock().unwrap().insert(
            ("ns".to_string(), "gpt-4o".to_string()),
            vec![Match {
                key,
                score: 0.95,
                payload: CachedPayload { status: 200, headers: vec![], body: b"cached".to_vec(), usage_total_tokens: Some(10), written_at: 0 },
            }],
        );
        let mut e = engine(FakeEmbedder { fixed: vec![0.1; 4], fail: false }, FakeStore::default());
        e.store = Rc::new(store);
        match e.on_request(body).await {
            RequestDecision::ShortCircuit { event: CacheEvent::HitSemantic, response } => {
                assert_eq!(response.body, b"cached");
            }
            other => panic!("expected HitSemantic, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn semantic_miss_below_threshold() {
        use crate::key::CanonicalPrompt;
        let body = br#"{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}]}"#;
        let store = FakeStore::default();
        let key = CacheKey::new("ns", "gpt-4o", &CanonicalPrompt::from_messages(&[("user","hi")]));
        store.topk.lock().unwrap().insert(
            ("ns".to_string(), "gpt-4o".to_string()),
            vec![Match { key, score: 0.85, payload: CachedPayload { status: 200, headers: vec![], body: b"old".to_vec(), usage_total_tokens: None, written_at: 0 } }],
        );
        let mut e = engine(FakeEmbedder { fixed: vec![0.1; 4], fail: false }, FakeStore::default());
        e.store = Rc::new(store);
        match e.on_request(body).await {
            RequestDecision::Forward { .. } => {}
            other => panic!("expected Forward, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn exact_hit_short_circuits_without_embedding() {
        use crate::key::CanonicalPrompt;
        let body = br#"{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}]}"#;
        let store = FakeStore::default();
        let key = CacheKey::new("ns", "gpt-4o", &CanonicalPrompt::from_messages(&[("user","hi")]));
        store.exact.lock().unwrap().insert(key.as_redis_key(), CachedPayload { status: 200, headers: vec![], body: b"x".to_vec(), usage_total_tokens: None, written_at: 0 });
        let mut e = engine(FakeEmbedder { fixed: vec![0.0; 4], fail: true }, FakeStore::default());
        e.store = Rc::new(store);
        match e.on_request(body).await {
            RequestDecision::ShortCircuit { event: CacheEvent::HitExact, .. } => {}
            other => panic!("expected HitExact, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn on_response_writes_only_for_allowed_status() {
        let body = br#"{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}]}"#;
        let store = Rc::new(FakeStore::default());
        let mut e = engine(FakeEmbedder { fixed: vec![0.1; 4], fail: false }, FakeStore::default());
        e.store = store.clone();
        let dec = e.on_request(body).await;
        e.on_response(&dec, 429, &[], b"err", None).await.unwrap();
        assert_eq!(*store.upserts.lock().unwrap(), 0);
        e.on_response(&dec, 200, &[], b"ok", Some(50)).await.unwrap();
        assert_eq!(*store.upserts.lock().unwrap(), 1);
    }

    #[test]
    fn sanitize_strips_sensitive_and_hop_by_hop_headers() {
        let headers = vec![
            ("Content-Type".into(), "application/json".into()),
            ("Authorization".into(), "Bearer secret".into()),
            ("Set-Cookie".into(), "sid=abc".into()),
            ("WWW-Authenticate".into(), "Basic".into()),
            ("Connection".into(), "keep-alive".into()),
            ("Transfer-Encoding".into(), "chunked".into()),
            ("X-Request-Id".into(), "r-1".into()),
        ];
        let out = sanitize_headers(&headers);
        let names: Vec<&str> = out.iter().map(|(k, _)| k.as_str()).collect();
        assert!(names.contains(&"Content-Type"));
        assert!(names.contains(&"X-Request-Id"));
        assert!(!names.iter().any(|k| k.eq_ignore_ascii_case("authorization")));
        assert!(!names.iter().any(|k| k.eq_ignore_ascii_case("set-cookie")));
        assert!(!names.iter().any(|k| k.eq_ignore_ascii_case("www-authenticate")));
        assert!(!names.iter().any(|k| k.eq_ignore_ascii_case("connection")));
        assert!(!names.iter().any(|k| k.eq_ignore_ascii_case("transfer-encoding")));
    }
}
