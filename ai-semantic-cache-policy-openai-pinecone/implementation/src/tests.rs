// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Salesforce, Inc.
//
// pdk-unit tests for the openai-pinecone variant. These exercise
// configure-time validation (without Docker) — specifically the
// startup-time `reject_service_path_with_suffix` guard added to
// catch operator misconfigurations that would silently produce
// double-suffix request paths.
//
// Lifecycle / vector-store tests live in the integration suites:
//  - tests/requests.rs       (httpmock-backed)
//  - tests/pinecone_local.rs (real Pinecone Local container)

#[cfg(test)]
mod tests {
    use pdk_unit::{UnitHttpRequest, UnitHttpResponse, UnitTestBuilder};
    use serde_json::json;

    fn cfg(base_url: &str, index_host: &str) -> String {
        json!({
            "namespace": "test-ns",
            "embedder": {
                "baseUrl": base_url,
                "model": "text-embedding-3-small",
                "apiKey": "sk-test",
                "dimension": 3,
                "timeoutMs": 5000
            },
            "vectordb": {
                "indexHost": index_host,
                "apiKey": "pc-test",
                "timeoutMs": 5000
            },
            "similarityThreshold": 0.92,
            "ttlSeconds": 3600,
            "exactCaching": false,
            "bypassOnStream": true,
            "promptExtractor": "openai",
            "cacheResponseStatusCodes": [200],
            "failureMode": "fail_open"
        })
        .to_string()
    }

    fn run(config: &str) -> UnitHttpResponse {
        let mut tester = UnitTestBuilder::default()
            .with_config(config)
            .with_backend(
                UnitHttpResponse::new(200)
                    .with_header("content-type", "application/json")
                    .with_body(r#"{"ok":true}"#),
            )
            .with_entrypoint(crate::configure);
        tester.request(
            UnitHttpRequest::post()
                .with_path("/v1/chat/completions")
                .with_header("content-type", "application/json")
                .with_body(
                    r#"{"model":"gpt-4o-mini","messages":[{"role":"user","content":"hi"}]}"#,
                ),
        )
    }

    #[test]
    fn configure_accepts_well_formed_urls() {
        let response = run(&cfg(
            "https://api.openai.com/v1",
            "https://my-index.svc.aws-us-east-1.pinecone.io",
        ));
        assert_eq!(response.status_code(), 200);
    }

    #[test]
    fn configure_rejects_baseurl_ending_in_embeddings() {
        // Common operator mistake: pasting the full embeddings URL into
        // baseUrl. The helper appends `/embeddings` itself, so this would
        // produce `/v1/embeddings/embeddings`. Must fail at startup.
        let response = run(&cfg(
            "https://api.openai.com/v1/embeddings",
            "https://my-index.svc.aws-us-east-1.pinecone.io",
        ));
        assert_ne!(
            response.status_code(),
            200,
            "baseUrl ending in `/embeddings` must fail policy init"
        );
    }

    #[test]
    fn configure_rejects_indexhost_ending_in_query() {
        let response = run(&cfg(
            "https://api.openai.com/v1",
            "https://my-index.svc.aws-us-east-1.pinecone.io/query",
        ));
        assert_ne!(
            response.status_code(),
            200,
            "indexHost ending in `/query` must fail policy init"
        );
    }

    #[test]
    fn configure_rejects_indexhost_ending_in_vectors_upsert() {
        let response = run(&cfg(
            "https://api.openai.com/v1",
            "https://my-index.svc.aws-us-east-1.pinecone.io/vectors/upsert",
        ));
        assert_ne!(
            response.status_code(),
            200,
            "indexHost ending in `/vectors/upsert` must fail policy init"
        );
    }
}
