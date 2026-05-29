// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Salesforce, Inc.
//
// Integration tests that exercise the policy against a real Pinecone Local
// emulator (`ghcr.io/pinecone-io/pinecone-local`) instead of canned httpmock
// responses. Asserts vector-store behavior end-to-end:
//
//   - upserts on miss land in Pinecone (visible via describe_index_stats)
//   - hits above similarity threshold short-circuit upstream
//
// **Prerequisite: Docker Desktop** (mac/Windows). The gateway inside the
// pdk-test composite reaches the host-side Pinecone Local container via
// Docker Desktop's auto-resolved `host.docker.internal`. On plain Linux
// Docker the gateway would need `--add-host=host.docker.internal:host-gateway`,
// which `pdk-test` doesn't currently expose; the family-level docs name
// Docker Desktop as a hard prerequisite for local dev + CI.
//
// Run with: `make test` (or `cargo test --test pinecone_local`).

mod common;

use common::pinecone_local as pc;
use common::*;
use httpmock::MockServer;
use pdk_test::services::flex::{ApiConfig, Flex, FlexConfig, PolicyConfig};
use pdk_test::services::httpmock::{HttpMock, HttpMockConfig};
use pdk_test::{pdk_test, TestComposite};
use serde_json::json;

const INDEX_NAME: &str = "ai-semantic-cache-test";
const DIMENSION: u32 = 3;

#[pdk_test]
async fn miss_then_pinecone_records_upsert() -> anyhow::Result<()> {
    let _pinecone = pc::PineconeLocal::start()?;
    pc::wait_ready().await?;
    pc::create_index(INDEX_NAME, DIMENSION).await?;

    let httpmock_config = HttpMockConfig::builder()
        .port(80)
        .version("latest")
        .hostname("backend")
        .build();

    let policy_config = PolicyConfig::builder()
        .name(POLICY_NAME)
        .configuration(json!({
            "namespace": "pcl-ns",
            "embedder": {
                "baseUrl": "http://backend:80/v1",
                "model": "text-embedding-3-small",
                "apiKey": "sk-test",
                "dimension": DIMENSION,
                "timeoutMs": 5000
            },
            "vectordb": {
                // The gateway reaches the host-side Pinecone Local container via
                // Docker Desktop's auto-resolved host.docker.internal.
                "indexHost": pc::INDEX_HOST_FOR_FLEX,
                "apiKey": "test",
                "timeoutMs": 5000
            },
            "similarityThreshold": 0.92,
            "ttlSeconds": 3600,
            "exactCaching": false,
            "bypassOnStream": true,
            "promptExtractor": "openai",
            "cacheResponseStatusCodes": [200],
            "failureMode": "fail_open"
        }))
        .build();

    let api_config = ApiConfig::builder()
        .name("llm-api")
        .port(8285)
        .path("/")
        .upstream(&httpmock_config)
        .policies([policy_config])
        .build();

    let flex_config = FlexConfig::builder()
        .version("1.11.0")
        .hostname("local-flex-pcl")
        .with_api(api_config)
        .config_mounts([(POLICY_DIR, "policy"), (COMMON_CONFIG_DIR, "common")])
        .build();

    let composite = TestComposite::builder()
        .with_service(flex_config)
        .with_service(httpmock_config)
        .build()
        .await?;

    let flex: Flex = composite.service()?;
    let api_url = flex.external_url(8285).unwrap();
    let backend: HttpMock = composite.service()?;
    let mock_server = MockServer::connect_async(backend.socket()).await;

    let prompt = "real-pinecone-miss-prompt";

    mock_server
        .mock_async(|when, then| {
            when.method("POST").path("/v1/embeddings").body_contains(prompt);
            then.status(200)
                .header("Content-Type", "application/json")
                .body(json!({
                    "object": "list",
                    "model": "text-embedding-3-small",
                    "data": [{"object": "embedding", "index": 0, "embedding": [0.1, 0.2, 0.3]}],
                    "usage": {"prompt_tokens": 1, "total_tokens": 1}
                }).to_string());
        })
        .await;

    mock_server
        .mock_async(|when, then| {
            when.method("POST").path("/v1/chat/completions").body_contains(prompt);
            then.status(200)
                .header("Content-Type", "application/json")
                .body(json!({
                    "id": "chatcmpl-real",
                    "choices": [{"message": {"content": "fresh"}}],
                    "usage": {"total_tokens": 9}
                }).to_string());
        })
        .await;

    let initial = pc::count_vectors().await?;
    assert_eq!(initial, 0);

    let resp = reqwest::Client::new()
        .post(format!("{api_url}/v1/chat/completions"))
        .header("Content-Type", "application/json")
        .body(json!({
            "model": "gpt-4o-mini",
            "messages": [{"role": "user", "content": prompt}]
        }).to_string())
        .send()
        .await?;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get("X-Cache").and_then(|v| v.to_str().ok()),
        Some("miss")
    );

    // Pinecone Local is eventually consistent for newly-upserted vectors;
    // poll briefly for visibility.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut count = 0usize;
    while std::time::Instant::now() < deadline {
        count = pc::count_vectors().await?;
        if count > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    assert!(count >= 1, "expected at least one vector upserted, got {count}");

    drop(composite);
    Ok(())
}

#[pdk_test]
async fn pre_seeded_vector_short_circuits_with_hit_semantic() -> anyhow::Result<()> {
    let _pinecone = pc::PineconeLocal::start()?;
    pc::wait_ready().await?;
    pc::create_index(INDEX_NAME, DIMENSION).await?;

    let cached_payload = json!({
        "status": 200,
        "headers": [["content-type", "application/json"]],
        "body": "{\"id\":\"chatcmpl-cached\",\"choices\":[{\"message\":{\"content\":\"replayed\"}}],\"usage\":{\"total_tokens\":7}}".as_bytes().to_vec(),
        "usage_total_tokens": 7,
        "written_at": 1716300000_u64
    }).to_string();

    pc::upsert(
        "pcl-hit-ns",
        "gpt-4o-mini",
        "pcl-hit-ns:gpt-4o-mini:abc123",
        vec![0.4, 0.5, 0.6],
        &cached_payload,
    )
    .await?;

    let httpmock_config = HttpMockConfig::builder()
        .port(80)
        .version("latest")
        .hostname("backend")
        .build();

    let policy_config = PolicyConfig::builder()
        .name(POLICY_NAME)
        .configuration(json!({
            "namespace": "pcl-hit-ns",
            "embedder": {
                "baseUrl": "http://backend:80/v1",
                "model": "text-embedding-3-small",
                "apiKey": "sk-test",
                "dimension": DIMENSION,
                "timeoutMs": 5000
            },
            "vectordb": {
                "indexHost": pc::INDEX_HOST_FOR_FLEX,
                "apiKey": "test",
                "timeoutMs": 5000
            },
            "similarityThreshold": 0.92,
            "ttlSeconds": 3600,
            "exactCaching": false,
            "bypassOnStream": true,
            "promptExtractor": "openai",
            "cacheResponseStatusCodes": [200],
            "failureMode": "fail_open"
        }))
        .build();

    let api_config = ApiConfig::builder()
        .name("llm-api")
        .port(8286)
        .path("/")
        .upstream(&httpmock_config)
        .policies([policy_config])
        .build();

    let flex_config = FlexConfig::builder()
        .version("1.11.0")
        .hostname("local-flex-pcl-hit")
        .with_api(api_config)
        .config_mounts([(POLICY_DIR, "policy"), (COMMON_CONFIG_DIR, "common")])
        .build();

    let composite = TestComposite::builder()
        .with_service(flex_config)
        .with_service(httpmock_config)
        .build()
        .await?;

    let flex: Flex = composite.service()?;
    let api_url = flex.external_url(8286).unwrap();
    let backend: HttpMock = composite.service()?;
    let mock_server = MockServer::connect_async(backend.socket()).await;

    let prompt = "real-pinecone-hit-prompt";

    mock_server
        .mock_async(|when, then| {
            when.method("POST").path("/v1/embeddings").body_contains(prompt);
            then.status(200)
                .header("Content-Type", "application/json")
                .body(json!({
                    "object": "list",
                    "model": "text-embedding-3-small",
                    "data": [{"object": "embedding", "index": 0, "embedding": [0.4, 0.5, 0.6]}],
                    "usage": {"prompt_tokens": 1, "total_tokens": 1}
                }).to_string());
        })
        .await;

    let upstream_must_not_fire = mock_server
        .mock_async(|when, then| {
            when.method("POST").path("/v1/chat/completions").body_contains(prompt);
            then.status(500).body("upstream should not be called on semantic hit");
        })
        .await;

    let resp = reqwest::Client::new()
        .post(format!("{api_url}/v1/chat/completions"))
        .header("Content-Type", "application/json")
        .body(json!({
            "model": "gpt-4o-mini",
            "messages": [{"role": "user", "content": prompt}]
        }).to_string())
        .send()
        .await?;

    assert_eq!(resp.status(), 200);
    let cache = resp.headers().get("X-Cache").and_then(|v| v.to_str().ok()).unwrap_or("");
    assert!(
        cache.starts_with("hit-semantic"),
        "expected hit-semantic*, got {cache}"
    );
    upstream_must_not_fire.assert_hits_async(0).await;

    drop(composite);
    Ok(())
}

#[pdk_test]
async fn exact_cache_round_trip_with_real_pinecone() -> anyhow::Result<()> {
    // Exercises the full integration: exactCaching=true, real Pinecone
    // Local as the semantic backend, identical second request must hit
    // the platform KV exact-cache (no embedder call, no Pinecone /query
    // call). Reviewer #6 — covers the gap between the request-only
    // exact-cache test (`exact_cache_hit_after_miss_warms_kv` in
    // requests.rs, which uses httpmock for Pinecone) and the
    // real-Pinecone semantic tests above (which set exactCaching=false).
    let _pinecone = pc::PineconeLocal::start()?;
    pc::wait_ready().await?;
    pc::create_index("ai-semantic-cache-roundtrip", DIMENSION).await?;

    let httpmock_config = HttpMockConfig::builder()
        .port(80)
        .version("latest")
        .hostname("backend")
        .build();

    let policy_config = PolicyConfig::builder()
        .name(POLICY_NAME)
        .configuration(json!({
            "namespace": "rt-ns",
            "embedder": {
                "baseUrl": "http://backend:80/v1",
                "model": "text-embedding-3-small",
                "apiKey": "sk-test",
                "dimension": DIMENSION,
                "timeoutMs": 5000
            },
            "vectordb": {
                "indexHost": pc::INDEX_HOST_FOR_FLEX,
                "apiKey": "test",
                "timeoutMs": 5000
            },
            "similarityThreshold": 0.92,
            "ttlSeconds": 3600,
            "exactCaching": true,
            "bypassOnStream": true,
            "promptExtractor": "openai",
            "cacheResponseStatusCodes": [200],
            "failureMode": "fail_open"
        }))
        .build();

    let api_config = ApiConfig::builder()
        .name("llm-api")
        .port(8287)
        .path("/")
        .upstream(&httpmock_config)
        .policies([policy_config])
        .build();

    let flex_config = FlexConfig::builder()
        .version("1.11.0")
        .hostname("local-flex-pcl-rt")
        .with_api(api_config)
        .config_mounts([(POLICY_DIR, "policy"), (COMMON_CONFIG_DIR, "common")])
        .build();

    let composite = TestComposite::builder()
        .with_service(flex_config)
        .with_service(httpmock_config)
        .build()
        .await?;

    let flex: Flex = composite.service()?;
    let api_url = flex.external_url(8287).unwrap();
    let backend: HttpMock = composite.service()?;
    let mock_server = MockServer::connect_async(backend.socket()).await;

    let prompt = "real-pinecone-roundtrip-prompt";

    let embedder_mock = mock_server
        .mock_async(|when, then| {
            when.method("POST").path("/v1/embeddings").body_contains(prompt);
            then.status(200)
                .header("Content-Type", "application/json")
                .body(json!({
                    "object": "list",
                    "model": "text-embedding-3-small",
                    "data": [{"object": "embedding", "index": 0, "embedding": [0.7, 0.8, 0.9]}],
                    "usage": {"prompt_tokens": 1, "total_tokens": 1}
                }).to_string());
        })
        .await;

    mock_server
        .mock_async(|when, then| {
            when.method("POST").path("/v1/chat/completions").body_contains(prompt);
            then.status(200)
                .header("Content-Type", "application/json")
                .body(json!({
                    "id": "chatcmpl-rt",
                    "choices": [{"message": {"content": "round-trip"}}],
                    "usage": {"total_tokens": 11}
                }).to_string());
        })
        .await;

    let url = format!("{api_url}/v1/chat/completions");
    let body = json!({
        "model": "gpt-4o-mini",
        "messages": [{"role": "user", "content": prompt}]
    })
    .to_string();
    let client = reqwest::Client::new();

    // First request: miss. Embedder + Pinecone called; both writes
    // (platform KV exact-cache + Pinecone upsert) issued best-effort.
    let first = client
        .post(&url)
        .header("Content-Type", "application/json")
        .body(body.clone())
        .send()
        .await?;
    assert_eq!(first.status(), 200);
    assert_eq!(
        first
            .headers()
            .get("X-Cache")
            .and_then(|v| v.to_str().ok()),
        Some("miss")
    );
    let embedder_hits_after_first = embedder_mock.hits_async().await;

    // Per architecture §9.1, both writes (platform KV exact-cache and
    // Pinecone upsert) are best-effort post-response. Wait for the
    // Pinecone upsert to land (visible via describe_index_stats) as a
    // proxy for "the response_filter outcalls have completed". The
    // platform KV is in-process and writes far faster than Pinecone's
    // network round-trip, so once Pinecone is visible the KV write is
    // guaranteed to be visible too.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if pc::count_vectors().await.unwrap_or(0) > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    }

    // Second identical request: must hit exact-cache. Because we polled
    // for write quiescence above, this should be a single attempt — any
    // retry here would falsely inflate the embedder hit count and
    // weaken the no-embedder-call assertion below.
    let second = client
        .post(&url)
        .header("Content-Type", "application/json")
        .body(body.clone())
        .send()
        .await?;
    assert_eq!(second.status(), 200);
    let second_cache = second
        .headers()
        .get("X-Cache")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert_eq!(
        second_cache, "hit-exact",
        "second identical request should hit platform-KV exact-cache after writes quiesce"
    );

    // hit-exact short-circuits BEFORE the embedder is consulted, so the
    // embedder hit count must NOT have grown.
    let embedder_hits_after_second = embedder_mock.hits_async().await;
    assert_eq!(
        embedder_hits_after_first, embedder_hits_after_second,
        "exact-cache hit must not call the embedder; got {embedder_hits_after_first} → {embedder_hits_after_second}"
    );

    drop(composite);
    Ok(())
}
