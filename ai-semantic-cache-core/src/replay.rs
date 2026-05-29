// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Salesforce, Inc.

use crate::store::CachedPayload;

pub const X_CACHE: &str = "x-cache";
pub const X_CACHE_SCORE: &str = "x-cache-score";
pub const X_CACHE_NAMESPACE: &str = "x-cache-namespace";
pub const X_CACHE_KEY: &str = "x-cache-key";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheEvent {
    HitExact,
    HitSemantic,
    Miss,
    BypassStream,
    BypassError,
    SkipUnparseable,
}

impl CacheEvent {
    pub fn header_value(&self) -> &'static str {
        match self {
            CacheEvent::HitExact => "hit-exact",
            CacheEvent::HitSemantic => "hit-semantic",
            CacheEvent::Miss => "miss",
            CacheEvent::BypassStream => "bypass-stream",
            CacheEvent::BypassError => "bypass-error",
            CacheEvent::SkipUnparseable => "skip-unparseable",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ReplayResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

pub fn build_replay(
    payload: &CachedPayload,
    event: CacheEvent,
    score: Option<f32>,
    namespace: &str,
    correlation_prefix: &str,
) -> ReplayResponse {
    let mut headers = payload.headers.clone();
    headers.push((X_CACHE.to_string(), event.header_value().to_string()));
    if let Some(s) = score {
        headers.push((X_CACHE_SCORE.to_string(), format!("{:.4}", s)));
    }
    headers.push((X_CACHE_NAMESPACE.to_string(), namespace.to_string()));
    headers.push((X_CACHE_KEY.to_string(), correlation_prefix.to_string()));
    ReplayResponse {
        status: payload.status,
        headers,
        body: payload.body.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload() -> CachedPayload {
        CachedPayload {
            status: 200,
            headers: vec![("content-type".into(), "application/json".into())],
            body: b"{}".to_vec(),
            usage_total_tokens: Some(123),
            written_at: 0,
        }
    }

    #[test]
    fn replay_preserves_status_and_body() {
        let r = build_replay(
            &payload(),
            CacheEvent::HitSemantic,
            Some(0.95),
            "ns",
            "abc123def456",
        );
        assert_eq!(r.status, 200);
        assert_eq!(r.body, b"{}".to_vec());
    }

    #[test]
    fn replay_appends_cache_headers() {
        let r = build_replay(&payload(), CacheEvent::HitExact, None, "ns", "abc123def456");
        assert!(r
            .headers
            .iter()
            .any(|(k, v)| k == X_CACHE && v == "hit-exact"));
        assert!(r
            .headers
            .iter()
            .any(|(k, v)| k == X_CACHE_NAMESPACE && v == "ns"));
        assert!(r
            .headers
            .iter()
            .any(|(k, v)| k == X_CACHE_KEY && v == "abc123def456"));
        assert!(!r.headers.iter().any(|(k, _)| k == X_CACHE_SCORE));
    }

    #[test]
    fn replay_includes_score_only_for_semantic() {
        let r = build_replay(&payload(), CacheEvent::HitSemantic, Some(0.953), "ns", "x");
        assert!(r
            .headers
            .iter()
            .any(|(k, v)| k == X_CACHE_SCORE && v == "0.9530"));
    }

    #[test]
    fn replay_preserves_existing_content_type() {
        let r = build_replay(&payload(), CacheEvent::HitExact, None, "ns", "x");
        assert!(r
            .headers
            .iter()
            .any(|(k, v)| k == "content-type" && v == "application/json"));
    }
}
