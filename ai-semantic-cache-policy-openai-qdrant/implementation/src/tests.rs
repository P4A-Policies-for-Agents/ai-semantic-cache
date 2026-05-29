// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Salesforce, Inc.
//
// pdk-unit tests for the openai-qdrant variant. Configure-time
// validation only; lifecycle / vector-store tests live in
// `tests/qdrant_local.rs` (real Qdrant container under Docker
// Desktop).

#[cfg(test)]
mod tests {
    use pdk_unit::{UnitHttpRequest, UnitHttpResponse, UnitTestBuilder};
    use serde_json::json;

    fn cfg(base_url: &str, vectordb_base_url: &str) -> String {
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
                "baseUrl": vectordb_base_url,
                "collection": "test-collection",
                "apiKey": "qd-test",
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
        let response = run(&cfg("https://api.openai.com/v1", "http://qdrant:6333"));
        assert_eq!(response.status_code(), 200);
    }

    #[test]
    fn configure_rejects_baseurl_ending_in_embeddings() {
        let response = run(&cfg(
            "https://api.openai.com/v1/embeddings",
            "http://qdrant:6333",
        ));
        assert_ne!(
            response.status_code(),
            200,
            "embedder.baseUrl ending in `/embeddings` must fail policy init"
        );
    }

    #[test]
    fn configure_rejects_vectordb_baseurl_ending_in_collections() {
        let response = run(&cfg(
            "https://api.openai.com/v1",
            "http://qdrant:6333/collections",
        ));
        assert_ne!(
            response.status_code(),
            200,
            "vectordb.baseUrl ending in `/collections` must fail policy init"
        );
    }

    #[test]
    fn configure_rejects_vectordb_baseurl_ending_in_points_search() {
        let response = run(&cfg(
            "https://api.openai.com/v1",
            "http://qdrant:6333/points/search",
        ));
        assert_ne!(
            response.status_code(),
            200,
            "vectordb.baseUrl ending in `/points/search` must fail policy init"
        );
    }
}
