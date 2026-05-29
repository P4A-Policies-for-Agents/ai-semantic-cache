// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Salesforce, Inc.
//
// Integration tests that exercise the Qdrant variant against a real
// Qdrant OSS container (`qdrant/qdrant:latest`) instead of canned
// httpmock responses. Asserts vector-store behavior end-to-end:
//
//   - upserts on miss land in Qdrant (visible via collections endpoint)
//   - hits above similarity threshold short-circuit upstream
//
// **Prerequisite: Docker Desktop** (mac/Windows). The gateway inside the
// pdk-test composite reaches the host-side Qdrant container via
// Docker Desktop's auto-resolved `host.docker.internal`. On plain Linux
// Docker the gateway would need `--add-host=host.docker.internal:host-gateway`,
// which `pdk-test` doesn't currently expose; the family-level docs name
// Docker Desktop as a hard prerequisite for local dev + CI.
//
// Run with: `make test` (or `cargo test --test qdrant_local`).

mod common;

use common::qdrant_local as qd;
use common::*;
use httpmock::MockServer;
use pdk_test::services::flex::{ApiConfig, Flex, FlexConfig, PolicyConfig};
use pdk_test::services::httpmock::{HttpMock, HttpMockConfig};
use pdk_test::{pdk_test, TestComposite};
use serde_json::json;

const COLLECTION: &str = "ai-semantic-cache-test";
const DIMENSION: u32 = 3;

#[pdk_test]
async fn miss_then_qdrant_records_upsert() -> anyhow::Result<()> {
    let _qdrant = qd::QdrantLocal::start()?;
    qd::wait_ready().await?;
    qd::create_collection(COLLECTION, DIMENSION).await?;

    let httpmock_config = HttpMockConfig::builder()
        .port(80)
        .version("latest")
        .hostname("backend")
        .build();

    let policy_config = PolicyConfig::builder()
        .name(POLICY_NAME)
        .configuration(json!({
            "namespace": "qd-ns",
            "embedder": {
                "baseUrl": "http://backend:80/v1",
                "model": "text-embedding-3-small",
                "apiKey": "sk-test",
                "dimension": DIMENSION,
                "timeoutMs": 5000
            },
            "vectordb": {
                "baseUrl": qd::BASE_URL_FOR_FLEX,
                "collection": COLLECTION,
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
        .port(8385)
        .path("/")
        .upstream(&httpmock_config)
        .policies([policy_config])
        .build();

    let flex_config = FlexConfig::builder()
        .version("1.11.0")
        .hostname("local-flex-qd")
        .with_api(api_config)
        .config_mounts([(POLICY_DIR, "policy"), (COMMON_CONFIG_DIR, "common")])
        .build();

    let composite = TestComposite::builder()
        .with_service(flex_config)
        .with_service(httpmock_config)
        .build()
        .await?;

    let flex: Flex = composite.service()?;
    let api_url = flex.external_url(8385).unwrap();
    let backend: HttpMock = composite.service()?;
    let mock_server = MockServer::connect_async(backend.socket()).await;

    let prompt = "real-qdrant-miss-prompt";

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
                    "id": "chatcmpl-real-qd",
                    "choices": [{"message": {"content": "fresh"}}],
                    "usage": {"total_tokens": 9}
                }).to_string());
        })
        .await;

    let initial = qd::count_points(COLLECTION).await?;
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

    // Qdrant points are sync via `?wait=true` from the policy, but the
    // engine writes from response_filter where outcalls are best-effort.
    // Poll briefly for visibility.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut count = 0usize;
    while std::time::Instant::now() < deadline {
        count = qd::count_points(COLLECTION).await?;
        if count > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    assert!(count >= 1, "expected at least one point upserted, got {count}");

    drop(composite);
    Ok(())
}

#[pdk_test]
async fn pre_seeded_point_short_circuits_with_hit_semantic() -> anyhow::Result<()> {
    let _qdrant = qd::QdrantLocal::start()?;
    qd::wait_ready().await?;
    qd::create_collection(COLLECTION, DIMENSION).await?;

    let namespace = "qd-hit-ns";
    let model = "gpt-4o-mini";
    let prompt_sha = "abc123";
    let id = qd::point_id(namespace, model, prompt_sha);

    let cached_payload = json!({
        "status": 200,
        "headers": [["content-type", "application/json"]],
        "body": "{\"id\":\"chatcmpl-cached\",\"choices\":[{\"message\":{\"content\":\"replayed\"}}],\"usage\":{\"total_tokens\":7}}".as_bytes().to_vec(),
        "usage_total_tokens": 7,
        "written_at": 1716300000_u64
    }).to_string();

    qd::upsert(
        COLLECTION,
        namespace,
        model,
        &id,
        prompt_sha,
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
            "namespace": namespace,
            "embedder": {
                "baseUrl": "http://backend:80/v1",
                "model": "text-embedding-3-small",
                "apiKey": "sk-test",
                "dimension": DIMENSION,
                "timeoutMs": 5000
            },
            "vectordb": {
                "baseUrl": qd::BASE_URL_FOR_FLEX,
                "collection": COLLECTION,
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
        .port(8386)
        .path("/")
        .upstream(&httpmock_config)
        .policies([policy_config])
        .build();

    let flex_config = FlexConfig::builder()
        .version("1.11.0")
        .hostname("local-flex-qd-hit")
        .with_api(api_config)
        .config_mounts([(POLICY_DIR, "policy"), (COMMON_CONFIG_DIR, "common")])
        .build();

    let composite = TestComposite::builder()
        .with_service(flex_config)
        .with_service(httpmock_config)
        .build()
        .await?;

    let flex: Flex = composite.service()?;
    let api_url = flex.external_url(8386).unwrap();
    let backend: HttpMock = composite.service()?;
    let mock_server = MockServer::connect_async(backend.socket()).await;

    let prompt = "real-qdrant-hit-prompt";

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
            "model": model,
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
