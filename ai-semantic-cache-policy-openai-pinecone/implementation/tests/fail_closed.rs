// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Salesforce, Inc.
//
// Separate test binary because the policy config differs from `requests.rs`
// (failureMode=fail_closed) and a single FlexConfig composite is shared
// across all tests in a binary via OnceLock.

mod common;

use httpmock::MockServer;
use pdk_test::services::flex::{ApiConfig, Flex, FlexConfig, PolicyConfig};
use pdk_test::services::httpmock::{HttpMock, HttpMockConfig};
use pdk_test::{pdk_test, TestComposite};
use serde_json::json;
use std::sync::OnceLock;

use common::*;

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
    if let Some(setup) = TEST_SETUP.get() {
        return Ok(setup);
    }
    let httpmock_config = HttpMockConfig::builder()
        .port(80)
        .version("latest")
        .hostname("backend")
        .build();

    let policy_config = PolicyConfig::builder()
        .name(POLICY_NAME)
        .configuration(json!({
            "namespace": "fail-closed-ns",
            "embedder": {
                "baseUrl": "http://backend:80/v1",
                "model": "text-embedding-3-small",
                "apiKey": "sk-test",
                "dimension": 3,
                "timeoutMs": 5000
            },
            "vectordb": {
                "indexHost": "http://backend:80",
                "apiKey": "pc-test",
                "timeoutMs": 5000
            },
            "similarityThreshold": 0.92,
            "ttlSeconds": 3600,
            "exactCaching": false,
            "bypassOnStream": true,
            "promptExtractor": "openai",
            "cacheResponseStatusCodes": [200],
            "failureMode": "fail_closed"
        }))
        .build();

    let api_config = ApiConfig::builder()
        .name("llm-api")
        .port(8186)
        .path("/")
        .upstream(&httpmock_config)
        .policies([policy_config])
        .build();

    let flex_config = FlexConfig::builder()
        .version("1.11.0")
        .hostname("local-flex-fc")
        .with_api(api_config)
        .config_mounts([(POLICY_DIR, "policy"), (COMMON_CONFIG_DIR, "common")])
        .build();

    let composite = TestComposite::builder()
        .with_service(flex_config)
        .with_service(httpmock_config)
        .build()
        .await?;

    let flex: Flex = composite.service()?;
    let api_url = flex.external_url(8186).unwrap();
    let backend: HttpMock = composite.service()?;
    let mock_server = MockServer::connect_async(backend.socket()).await;
    std::mem::forget(composite);

    let setup = TestSetup { api_url, mock_server };
    TEST_SETUP.set(setup).expect("once");
    Ok(TEST_SETUP.get().unwrap())
}

#[pdk_test]
async fn fail_closed_on_store_error() -> anyhow::Result<()> {
    let setup = setup_test().await?;

    setup
        .mock_server
        .mock_async(|when, then| {
            when.method("POST").path("/v1/embeddings");
            then.status(200)
                .header("Content-Type", "application/json")
                .body(json!({
                    "object": "list",
                    "model": "text-embedding-3-small",
                    "data": [{"object":"embedding","index":0,"embedding":[0.1,0.2,0.3]}],
                    "usage": {"prompt_tokens":1,"total_tokens":1}
                }).to_string());
        })
        .await;

    setup
        .mock_server
        .mock_async(|when, then| {
            when.method("POST").path("/query");
            then.status(503).body("pinecone is sad");
        })
        .await;

    let upstream_must_not_fire = setup
        .mock_server
        .mock_async(|when, then| {
            when.method("POST").path("/v1/chat/completions");
            then.status(200).body("upstream should not be reached");
        })
        .await;

    let client = reqwest::Client::new();
    let url = format!("{}/v1/chat/completions", setup.api_url);
    let resp = client
        .post(&url)
        .header("Content-Type", "application/json")
        .body(json!({
            "model": "gpt-4o-mini",
            "messages": [{"role":"user","content":"fail closed me"}]
        }).to_string())
        .send()
        .await?;

    assert_eq!(resp.status(), 503, "fail_closed must surface 503 on store error");
    upstream_must_not_fire.assert_hits_async(0).await;
    Ok(())
}
