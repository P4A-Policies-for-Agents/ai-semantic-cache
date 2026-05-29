// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Salesforce, Inc.
//
//! PDK driver glue shared by every variant binary.
//!
//! Each variant's `lib.rs` reduces to:
//! 1. Build a `DriverConfig` from its generated config.
//! 2. Build an [`Embedder`] factory and a [`VectorStore`] factory that
//!    target the variant's REST surface.
//! 3. Hand both to [`run`] inside the `#[entrypoint]`.
//!
//! The driver owns:
//! - The platform-shared-KV-backed exact-cache fast path (request-side
//!   lookup before invoking the engine; response-side write best-effort).
//! - Sensitive-header stripping on cached responses.
//! - The `request_filter` / `response_filter` lifecycle, including
//!   `X-Cache*` header injection on every code path that reaches the
//!   client.
//! - Logging of cache events.

use crate::config::{FailureMode, Threshold, Ttl};
use crate::embed::Embedder;
use crate::engine::{Engine, EngineConfig, RequestDecision};
use crate::extract::PromptExtractor;
use crate::replay::{
    build_replay, CacheEvent, ReplayResponse, X_CACHE, X_CACHE_KEY, X_CACHE_NAMESPACE,
    X_CACHE_SCORE,
};
use crate::store::{CachedPayload, VectorStore};
use anyhow::Result;
use pdk::data_storage::{DataStorage, DataStorageBuilder, RemoteDataStorage, StoreMode};
use pdk::hl::*;
use pdk::logger;
use pdk::policy_violation::PolicyViolations;
use std::rc::Rc;

const EXACT_CACHE_KV_NAMESPACE: &str = "ai-semantic-cache:exact";

/// Reject Service URIs whose path already includes a sub-path the helper
/// is going to append (causing `/v1/embeddings/embeddings` style 404s).
///
/// PDK's `HttpClient.request(...).path(s)` REPLACES the Service URI's
/// path component; helpers in this family read the Service URI path
/// and APPEND the per-endpoint suffix to it (so `baseUrl: http://x/v1`
/// + helper-side `/embeddings` → `/v1/embeddings`). If an operator
/// puts the *full* URL in the Service (`baseUrl: http://x/v1/embeddings`),
/// the helper appends its suffix on top and the request goes to the
/// wrong path. Catch that at startup with a clear error.
///
/// Variant code calls this from its `#[entrypoint]` for each Service
/// it hands to a store/embedder helper.
pub fn reject_service_path_with_suffix(
    field_name: &str,
    service_path: &str,
    forbidden_suffixes: &[&str],
) -> anyhow::Result<()> {
    let trimmed = service_path.trim_end_matches('/');
    for suffix in forbidden_suffixes {
        if trimmed.ends_with(suffix) {
            anyhow::bail!(
                "{field_name} must NOT include `{suffix}` — the policy appends it. \
                 Got path `{service_path}`. Set the URL to the resource root \
                 (e.g. `https://api.openai.com/v1`) and let the policy append the \
                 endpoint sub-path."
            );
        }
    }
    Ok(())
}

/// Configuration the driver needs from each variant's generated config.
///
/// Variants build this once at policy-init time. Field names use
/// driver-internal conventions; the variant's gcl.yaml field naming is
/// the variant's responsibility.
#[derive(Clone, Debug)]
pub struct DriverConfig {
    pub policy_name: &'static str,
    pub namespace: String,
    pub similarity_threshold: f32,
    pub ttl_seconds: u64,
    pub exact_caching: bool,
    pub bypass_on_stream: bool,
    pub prompt_extractor: String,
    pub cache_response_status_codes: Vec<u16>,
    pub failure_mode: String,
}

impl DriverConfig {
    fn engine_config(&self) -> Result<EngineConfig> {
        let threshold = Threshold::new(self.similarity_threshold).map_err(|e| anyhow::anyhow!(e))?;
        let failure_mode = match self.failure_mode.as_str() {
            "fail_open" => FailureMode::FailOpen,
            "fail_closed" => FailureMode::FailClosed,
            other => anyhow::bail!("unknown failure_mode: {other}"),
        };
        Ok(EngineConfig {
            namespace: self.namespace.clone(),
            similarity_threshold: threshold,
            ttl: Ttl::from_seconds(self.ttl_seconds),
            // The driver runs its own exact-cache lookup against the
            // platform shared KV before invoking the engine, so the
            // engine must not also try its (vector-store-backed)
            // exact path.
            exact_caching: false,
            bypass_on_stream: self.bypass_on_stream,
            cache_response_status_codes: self.cache_response_status_codes.clone(),
            failure_mode,
        })
    }

    fn extractor(&self) -> Result<PromptExtractor> {
        Ok(match self.prompt_extractor.as_str() {
            "openai" => PromptExtractor::Openai,
            "anthropic" => PromptExtractor::Anthropic,
            "cohere" => PromptExtractor::Cohere,
            "mistral" => PromptExtractor::Mistral,
            "bedrock" => PromptExtractor::Bedrock,
            "vertex" => PromptExtractor::Vertex,
            // `dataweave` was advertised in earlier schemas but never
            // actually evaluated — the variant returns `Skip` and every
            // request gets `skip-unparseable`. Reject it explicitly with
            // a v2-backlog pointer so operators don't silently lose
            // caching.
            "dataweave" => anyhow::bail!(
                "prompt_extractor=dataweave is not yet implemented (v2 backlog); \
                 use one of: openai, anthropic, cohere, mistral, bedrock, vertex"
            ),
            other => anyhow::bail!("unknown prompt_extractor: {other}"),
        })
    }
}

/// Factory closure that produces an [`Embedder`] given a per-request
/// PDK `HttpClient` (which is `!Send + !Sync` and request-scoped).
pub type EmbedderFactory = Rc<dyn Fn(Rc<HttpClient>) -> Rc<dyn Embedder>>;

/// Factory closure that produces a [`VectorStore`].
pub type StoreFactory = Rc<dyn Fn(Rc<HttpClient>) -> Rc<dyn VectorStore>>;

/// Wires the variant's [`Embedder`] and [`VectorStore`] factories to PDK
/// request / response filters and starts the policy. Call this from the
/// variant's `#[entrypoint]`.
pub async fn run(
    launcher: Launcher,
    cfg: DriverConfig,
    store_builder: DataStorageBuilder,
    policy_violations: PolicyViolations,
    embedder: EmbedderFactory,
    store: StoreFactory,
) -> Result<()> {
    let engine_cfg = cfg.engine_config()?;
    let extractor = cfg.extractor()?;

    let exact_kv = if cfg.exact_caching {
        let ttl_ms = (cfg.ttl_seconds as u32).saturating_mul(1000);
        Some(Rc::new(store_builder.remote(EXACT_CACHE_KV_NAMESPACE, ttl_ms)))
    } else {
        None
    };

    let pv = Rc::new(policy_violations);
    let cfg = Rc::new(cfg);
    let engine_cfg = Rc::new(engine_cfg);

    let filter = on_request({
        let cfg = cfg.clone();
        let engine_cfg = engine_cfg.clone();
        let extractor = extractor.clone();
        let pv = pv.clone();
        let exact_kv = exact_kv.clone();
        let embedder = embedder.clone();
        let store = store.clone();
        move |req, http_client| {
            let cfg = cfg.clone();
            let engine_cfg = engine_cfg.clone();
            let extractor = extractor.clone();
            let pv = pv.clone();
            let exact_kv = exact_kv.clone();
            let embedder = embedder.clone();
            let store = store.clone();
            async move {
                request_filter(
                    req,
                    http_client,
                    cfg,
                    engine_cfg,
                    extractor,
                    pv,
                    exact_kv,
                    embedder,
                    store,
                )
                .await
            }
        }
    })
    .on_response({
        let cfg = cfg.clone();
        let engine_cfg = engine_cfg.clone();
        let extractor = extractor.clone();
        let exact_kv = exact_kv.clone();
        let store = store.clone();
        move |resp, request_data, http_client| {
            let cfg = cfg.clone();
            let engine_cfg = engine_cfg.clone();
            let extractor = extractor.clone();
            let exact_kv = exact_kv.clone();
            let store = store.clone();
            async move {
                response_filter(
                    resp,
                    request_data,
                    http_client,
                    cfg,
                    engine_cfg,
                    extractor,
                    exact_kv,
                    store,
                )
                .await
            }
        }
    });
    launcher.launch(filter).await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn request_filter(
    req: RequestState,
    http_client: HttpClient,
    cfg: Rc<DriverConfig>,
    engine_cfg: Rc<EngineConfig>,
    extractor: PromptExtractor,
    pv: Rc<PolicyViolations>,
    exact_kv: Option<Rc<RemoteDataStorage>>,
    embedder_factory: EmbedderFactory,
    store_factory: StoreFactory,
) -> Flow<Option<(RequestDecision, Vec<u8>)>> {
    let body_state = req.into_body_state().await;
    let body = body_state.handler().body();

    // Platform-KV exact-cache fast path: try BEFORE building the engine
    // so a byte-identical second request skips embedder + vector store.
    if let Some(kv) = exact_kv.as_ref() {
        if let Some((payload, sha_prefix)) =
            exact_lookup(kv, &cfg.namespace, &body, cfg.policy_name).await
        {
            log_event(cfg.policy_name, CacheEvent::HitExact, None);
            let response = build_replay(
                &payload,
                CacheEvent::HitExact,
                None,
                &cfg.namespace,
                &sha_prefix,
            );
            return Flow::Break(replay_to_pdk(&response));
        }
    }

    let http = Rc::new(http_client);
    let embedder = embedder_factory(http.clone());
    let store = store_factory(http);
    let engine = Engine {
        config: (*engine_cfg).clone(),
        embedder,
        store,
        extractor,
    };
    let decision = engine.on_request(&body).await;

    match &decision {
        RequestDecision::ShortCircuit { response, event } => {
            log_event(cfg.policy_name, *event, decoration_score(response));
            Flow::Break(replay_to_pdk(response))
        }
        RequestDecision::FailClosed { reason } => {
            logger::warn!("[{}] fail_closed: {}", cfg.policy_name, reason);
            pv.generate_policy_violation();
            Flow::Break(
                Response::new(503).with_body(b"semantic cache infrastructure unavailable".to_vec()),
            )
        }
        RequestDecision::ForwardNoWrite { event } => {
            log_event(cfg.policy_name, *event, None);
            Flow::Continue(Some((decision, body.to_vec())))
        }
        RequestDecision::Forward { .. } => {
            log_event(cfg.policy_name, CacheEvent::Miss, None);
            Flow::Continue(Some((decision, body.to_vec())))
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn response_filter(
    resp: ResponseState,
    request_data: RequestData<Option<(RequestDecision, Vec<u8>)>>,
    http_client: HttpClient,
    cfg: Rc<DriverConfig>,
    engine_cfg: Rc<EngineConfig>,
    extractor: PromptExtractor,
    exact_kv: Option<Rc<RemoteDataStorage>>,
    store_factory: StoreFactory,
) {
    let (decision, request_body) = match request_data {
        RequestData::Continue(Some(d)) => d,
        _ => return,
    };
    let headers_state = resp.into_headers_state().await;
    let upstream_status = headers_state.status_code() as u16;
    let upstream_headers = headers_state.handler().headers();

    // Always inject X-Cache, X-Cache-Namespace, X-Cache-Key on the
    // upstream response per architecture §10.1. ShortCircuit/FailClosed
    // never reach response_filter — they break out earlier — so the
    // exhaustive match below treats them as `unreachable!()` to make
    // any future RequestDecision variant a compiler error rather than
    // a silent miss-by-default.
    let (event, key_prefix) = match &decision {
        RequestDecision::Forward { key, .. } => {
            (CacheEvent::Miss, key.correlation_prefix())
        }
        RequestDecision::ForwardNoWrite { event } => {
            (*event, body_sha_prefix(&request_body))
        }
        RequestDecision::ShortCircuit { .. } | RequestDecision::FailClosed { .. } => {
            unreachable!("ShortCircuit/FailClosed are Flow::Break-d in request_filter")
        }
    };
    let handler = headers_state.handler();
    handler.set_header(X_CACHE, event.header_value());
    handler.set_header(X_CACHE_NAMESPACE, &cfg.namespace);
    handler.set_header(X_CACHE_KEY, &key_prefix);

    let body_state = headers_state.into_body_state().await;
    let upstream_body = body_state.handler().body();
    let usage = extract_usage_total_tokens(&upstream_body);

    // Cache to platform KV (exact-cache) and the variant's vector store
    // (semantic), best-effort.
    if let RequestDecision::Forward { .. } = &decision {
        if cfg.cache_response_status_codes.iter().any(|&c| c == upstream_status) {
            if let Some(kv) = exact_kv.as_ref() {
                exact_store(
                    kv,
                    &cfg.namespace,
                    &request_body,
                    upstream_status,
                    &upstream_headers,
                    &upstream_body,
                    usage,
                    cfg.policy_name,
                )
                .await;
            }
        }
    }

    // Engine.on_response only needs the store, not the embedder.
    let http = Rc::new(http_client);
    let store = store_factory(http);
    let engine = Engine {
        config: (*engine_cfg).clone(),
        // Embedder is unused on the response side; supply a placeholder
        // that panics if anything ever reaches for it.
        embedder: Rc::new(NoopEmbedder),
        store,
        extractor,
    };
    if let Err(e) = engine
        .on_response(&decision, upstream_status, &upstream_headers, &upstream_body, usage)
        .await
    {
        logger::warn!("[{}] semantic cache write failed: {}", cfg.policy_name, e);
    }
}

// =====================================================================
// Exact-cache (PDK shared KV)
// =====================================================================

#[derive(serde::Serialize, serde::Deserialize)]
struct ExactCacheEntry {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    usage_total_tokens: Option<u64>,
}

fn body_sha_full(body: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(body);
    hex::encode(h.finalize())
}

fn body_sha_prefix(body: &[u8]) -> String {
    body_sha_full(body)[..12].to_string()
}

fn exact_kv_key(namespace: &str, body: &[u8]) -> String {
    format!("{}:{}", namespace, body_sha_full(body))
}

async fn exact_lookup(
    kv: &RemoteDataStorage,
    namespace: &str,
    body: &[u8],
    policy_name: &'static str,
) -> Option<(CachedPayload, String)> {
    let k = exact_kv_key(namespace, body);
    let prefix = body_sha_prefix(body);
    match kv.get::<ExactCacheEntry>(&k).await {
        Ok(Some((entry, _version))) => Some((
            CachedPayload {
                status: entry.status,
                headers: entry.headers,
                body: entry.body,
                usage_total_tokens: entry.usage_total_tokens,
                written_at: 0,
            },
            prefix,
        )),
        Ok(None) => None,
        Err(e) => {
            logger::warn!("[{policy_name}] exact-cache lookup failed: {e}");
            None
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn exact_store(
    kv: &RemoteDataStorage,
    namespace: &str,
    request_body: &[u8],
    status: u16,
    headers: &[(String, String)],
    body: &[u8],
    usage_total_tokens: Option<u64>,
    policy_name: &'static str,
) {
    let safe_headers = strip_sensitive_headers(headers);
    let entry = ExactCacheEntry {
        status,
        headers: safe_headers,
        body: body.to_vec(),
        usage_total_tokens,
    };
    let k = exact_kv_key(namespace, request_body);
    if let Err(e) = kv.store(&k, &StoreMode::Always, &entry).await {
        logger::warn!("[{policy_name}] exact-cache store failed: {e}");
    }
}

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

fn strip_sensitive_headers(headers: &[(String, String)]) -> Vec<(String, String)> {
    headers
        .iter()
        .filter(|(k, _)| {
            let lower = k.to_ascii_lowercase();
            !SENSITIVE_HEADERS.contains(&lower.as_str())
        })
        .cloned()
        .collect()
}

// =====================================================================
// Helpers
// =====================================================================

fn extract_usage_total_tokens(body: &[u8]) -> Option<u64> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    v.get("usage")
        .and_then(|u| u.get("total_tokens"))
        .and_then(|t| t.as_u64())
}

fn replay_to_pdk(r: &ReplayResponse) -> Response {
    let mut headers = r.headers.clone();
    Response::new(r.status as u32)
        .with_headers(std::mem::take(&mut headers))
        .with_body(r.body.clone())
}

fn decoration_score(response: &ReplayResponse) -> Option<f32> {
    response
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(X_CACHE_SCORE))
        .and_then(|(_, v)| v.parse::<f32>().ok())
}

fn log_event(policy_name: &'static str, event: CacheEvent, score: Option<f32>) {
    let mut obj = serde_json::Map::new();
    obj.insert(
        "policy".into(),
        serde_json::Value::String(policy_name.to_string()),
    );
    obj.insert(
        "event".into(),
        serde_json::Value::String(event.header_value().to_string()),
    );
    if let Some(s) = score {
        obj.insert(
            "score".into(),
            serde_json::Number::from_f64(s as f64)
                .map(serde_json::Value::Number)
                .unwrap_or(serde_json::Value::Null),
        );
    }
    let line = serde_json::Value::Object(obj).to_string();
    logger::info!("{line}");
}

// Placeholder embedder for the response-side `Engine`, which only ever
// calls `store.upsert` — never `embedder.embed`. If anything ever does
// reach for the embedder on the response path, the panic surfaces in
// tests immediately rather than silently succeeding.
struct NoopEmbedder;

#[async_trait::async_trait(?Send)]
impl Embedder for NoopEmbedder {
    #[cold]
    async fn embed(&self, _text: &str) -> Result<Vec<f32>, crate::error::EmbedError> {
        debug_assert!(
            false,
            "NoopEmbedder::embed reached on the response path — \
             Engine.on_response should not call the embedder"
        );
        Err(crate::error::EmbedError::Malformed(
            "NoopEmbedder should never be called".into(),
        ))
    }

    fn dimension(&self) -> usize {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_sha_prefix_is_12_hex_chars() {
        let p = body_sha_prefix(b"hello");
        assert_eq!(p.len(), 12);
        assert!(p.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn exact_kv_key_includes_namespace_and_full_sha() {
        let k = exact_kv_key("ns", b"hello");
        assert!(k.starts_with("ns:"));
        assert_eq!(k.len(), "ns:".len() + 64);
    }

    #[test]
    fn strip_sensitive_headers_drops_auth_and_cookies() {
        let h = vec![
            ("Content-Type".into(), "application/json".into()),
            ("Authorization".into(), "Bearer x".into()),
            ("Set-Cookie".into(), "a=b".into()),
            ("X-Request-Id".into(), "abc".into()),
        ];
        let out = strip_sensitive_headers(&h);
        assert_eq!(out.len(), 2);
        assert!(out.iter().any(|(k, _)| k == "Content-Type"));
        assert!(out.iter().any(|(k, _)| k == "X-Request-Id"));
    }
}
