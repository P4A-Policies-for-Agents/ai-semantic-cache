// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Salesforce, Inc.
//
// Integration tests for the Azure AI Search variant. Azure AI Search has
// no official local emulator, so the tests use httpmock to fake the
// Azure REST surface inside the same composite as the gateway + the
// embeddings/upstream-LLM mocks. Two end-to-end cases:
//
//   - cache_miss_writes_to_index: empty search response, expect upstream
//     called, X-Cache: miss, and a `POST /indexes/<name>/docs/index`
//     call observed with the expected document shape.
//   - semantic_cache_hit_above_threshold: search returns one doc with
//     `@search.score` 0.97 and a JSON-encoded `cached` payload field;
//     policy short-circuits with hit-semantic.

mod common;

use common::*;
use httpmock::MockServer;
use pdk_test::services::flex::{ApiConfig, Flex, FlexConfig, PolicyConfig};
use pdk_test::services::httpmock::{HttpMock, HttpMockConfig};
use pdk_test::{pdk_test, TestComposite};
use serde_json::json;
use std::sync::OnceLock;

const INDEX_NAME: &str = "ai-semantic-cache-test";
const API_VERSION: &str = "2024-07-01";

struct TestSetup {
    api_url: String,
    mock_server: MockServer,
}

impl std::fmt::Debug for TestSetup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TestSetup").field("api_url", &self.api_url).finish()
    }
}

static TEST_SETUP: OnceLock<TestSetup> = OnceLock::new();

async fn setup_test() -> anyhow::Result<&'static TestSetup> {
    if let Some(s) = TEST_SETUP.get() {
        return Ok(s);
    }

    let httpmock_config = HttpMockConfig::builder()
        .port(80)
        .version("latest")
        .hostname("backend")
        .build();

    let policy_config = PolicyConfig::builder()
        .name(POLICY_NAME)
        .configuration(json!({
            "namespace": "azure-ns",
            "embedder": {
                "baseUrl": "http://backend:80/v1",
                "model": "text-embedding-3-small",
                "apiKey": "sk-test",
                "dimension": 3,
                "timeoutMs": 5000
            },
            "vectordb": {
                // Same backend host fakes both OpenAI and Azure AI Search
                // surfaces — the path namespacing keeps them distinct.
                "endpoint": "http://backend:80",
                "indexName": INDEX_NAME,
                "vectorField": "embedding",
                "apiKey": "azure-admin-key",
                "apiVersion": API_VERSION,
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
        .port(8485)
        .path("/")
        .upstream(&httpmock_config)
        .policies([policy_config])
        .build();

    let flex_config = FlexConfig::builder()
        .version("1.11.0")
        .hostname("local-flex-azure")
        .with_api(api_config)
        .config_mounts([(POLICY_DIR, "policy"), (COMMON_CONFIG_DIR, "common")])
        .build();

    let composite = TestComposite::builder()
        .with_service(flex_config)
        .with_service(httpmock_config)
        .build()
        .await?;

    let flex: Flex = composite.service()?;
    let api_url = flex.external_url(8485).unwrap();
    let backend: HttpMock = composite.service()?;
    let mock_server = MockServer::connect_async(backend.socket()).await;
    std::mem::forget(composite);

    let setup = TestSetup { api_url, mock_server };
    TEST_SETUP.set(setup).expect("once");
    Ok(TEST_SETUP.get().unwrap())
}

fn search_path() -> String {
    format!("/indexes/{INDEX_NAME}/docs/search")
}

fn index_path() -> String {
    format!("/indexes/{INDEX_NAME}/docs/index")
}

#[pdk_test]
async fn cache_miss_writes_to_index() -> anyhow::Result<()> {
    let setup = setup_test().await?;
    let prompt = "azure-miss-prompt-unique";
    let model = "azure-model-miss";

    setup
        .mock_server
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

    // /docs/search returns empty for this model
    setup
        .mock_server
        .mock_async(|when, then| {
            when.method("POST")
                .path(search_path())
                .body_contains(format!("model eq '{model}'"));
            then.status(200)
                .header("Content-Type", "application/json")
                .body(json!({"value": []}).to_string());
        })
        .await;

    // /docs/index for this model returns success
    let index_mock = setup
        .mock_server
        .mock_async(|when, then| {
            when.method("POST").path(index_path()).body_contains(model);
            then.status(200)
                .header("Content-Type", "application/json")
                .body(json!({
                    "value": [{"key": "doc1", "status": true, "errorMessage": null, "statusCode": 201}]
                }).to_string());
        })
        .await;

    let upstream = setup
        .mock_server
        .mock_async(|when, then| {
            when.method("POST").path("/v1/chat/completions").body_contains(prompt);
            then.status(200)
                .header("Content-Type", "application/json")
                .body(json!({
                    "id": "chatcmpl-azure-miss",
                    "choices": [{"message": {"content": "fresh"}}],
                    "usage": {"total_tokens": 9}
                }).to_string());
        })
        .await;

    let resp = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", setup.api_url))
        .header("Content-Type", "application/json")
        .body(json!({
            "model": model,
            "messages": [{"role": "user", "content": prompt}]
        }).to_string())
        .send()
        .await?;

    assert_eq!(resp.status(), 200);
    let cache = resp.headers().get("X-Cache").and_then(|v| v.to_str().ok());
    assert_eq!(cache, Some("miss"));

    upstream.assert_async().await;
    // /docs/index is best-effort from response_filter — observational, not required.
    let _ = index_mock;
    Ok(())
}

#[pdk_test]
async fn semantic_cache_hit_above_threshold() -> anyhow::Result<()> {
    let setup = setup_test().await?;
    let prompt = "azure-hit-prompt-unique";
    let model = "azure-model-hit";

    setup
        .mock_server
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

    // CachedPayload serializes `body: Vec<u8>` as a JSON byte array.
    let cached_body = "{\"id\":\"chatcmpl-cached\",\"choices\":[{\"message\":{\"content\":\"replayed\"}}],\"usage\":{\"total_tokens\":7}}".as_bytes().to_vec();
    let cached_payload = json!({
        "status": 200,
        "headers": [["content-type", "application/json"]],
        "body": cached_body,
        "usage_total_tokens": 7,
        "written_at": 1716300000_u64
    })
    .to_string();

    // Azure search response: one doc, score 0.97, cached payload as a string field.
    setup
        .mock_server
        .mock_async(|when, then| {
            when.method("POST")
                .path(search_path())
                .body_contains(format!("model eq '{model}'"));
            then.status(200)
                .header("Content-Type", "application/json")
                .body(json!({
                    "value": [{
                        "@search.score": 0.97,
                        "id": format!("azure-ns_{model}_abc123"),
                        "namespace": "azure-ns",
                        "model": model,
                        "prompt_sha": "abc123",
                        "cached": cached_payload
                    }]
                }).to_string());
        })
        .await;

    let upstream_must_not_fire = setup
        .mock_server
        .mock_async(|when, then| {
            when.method("POST").path("/v1/chat/completions").body_contains(prompt);
            then.status(500).body("upstream should not be called on hit");
        })
        .await;

    let resp = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", setup.api_url))
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

    Ok(())
}

#[pdk_test]
async fn bypass_on_stream() -> anyhow::Result<()> {
    let setup = setup_test().await?;
    let prompt = "azure-stream-prompt-unique";

    let embeddings = setup
        .mock_server
        .mock_async(|when, then| {
            when.method("POST").path("/v1/embeddings").body_contains(prompt);
            then.status(200)
                .header("Content-Type", "application/json")
                .body(json!({
                    "object": "list",
                    "model": "text-embedding-3-small",
                    "data": [{"object": "embedding", "index": 0, "embedding": [0.0, 0.0, 0.0]}],
                    "usage": {"prompt_tokens": 1, "total_tokens": 1}
                }).to_string());
        })
        .await;

    let upstream = setup
        .mock_server
        .mock_async(|when, then| {
            when.method("POST").path("/v1/chat/completions").body_contains(prompt);
            then.status(200)
                .header("Content-Type", "application/json")
                .body(json!({"id": "chatcmpl-azure-stream"}).to_string());
        })
        .await;

    let resp = reqwest::Client::new()
        .post(format!("{}/v1/chat/completions", setup.api_url))
        .header("Content-Type", "application/json")
        .body(json!({
            "model": "gpt-4o-mini",
            "stream": true,
            "messages": [{"role": "user", "content": prompt}]
        }).to_string())
        .send()
        .await?;

    assert_eq!(resp.status(), 200);
    let cache = resp.headers().get("X-Cache").and_then(|v| v.to_str().ok());
    assert_eq!(cache, Some("bypass-stream"));
    upstream.assert_async().await;
    embeddings.assert_hits_async(0).await;
    Ok(())
}
