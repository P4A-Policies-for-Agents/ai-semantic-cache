// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Salesforce, Inc.
//
// Lightweight harness around the Qdrant OSS Docker image
// (`qdrant/qdrant:latest`) for integration tests.
//
// `pdk-test` doesn't let third-party crates implement its `Config` trait
// (the module is private), so the container is started via `docker run`
// directly. From inside the pdk-test composite network, the gateway reaches it
// via `http://host.docker.internal:6333` — Docker Desktop (mac/Windows)
// provides this hostname out of the box; Linux needs
// `--add-host=host.docker.internal:host-gateway` on the gateway container,
// which `pdk-test` does not currently support — so this harness is gated
// to macOS/Docker-Desktop runners.

use std::process::Command;

const IMAGE: &str = "qdrant/qdrant:latest";
pub const HOST_PORT: u16 = 6333;

/// Reachable from inside the pdk-test docker network.
pub const BASE_URL_FOR_FLEX: &str = "http://host.docker.internal:6333";
/// Reachable from the host (cargo-test process).
pub const BASE_URL_FOR_HOST: &str = "http://localhost:6333";

pub struct QdrantLocal {
    container_id: String,
}

impl QdrantLocal {
    pub fn start() -> anyhow::Result<Self> {
        // Kill any leftover from a prior aborted test run.
        let _ = Command::new("docker")
            .args(["rm", "-f", "ai-semantic-cache-qd-local"])
            .output();

        let out = Command::new("docker")
            .args([
                "run",
                "-d",
                "--rm",
                "--name",
                "ai-semantic-cache-qd-local",
                "-p",
                &format!("{HOST_PORT}:6333"),
                IMAGE,
            ])
            .output()?;
        if !out.status.success() {
            anyhow::bail!(
                "docker run qdrant failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Ok(Self { container_id: id })
    }

    pub fn stop(&self) {
        let _ = Command::new("docker")
            .args(["rm", "-f", &self.container_id])
            .output();
    }
}

/// Reclaim the host-side container automatically when the test fixture
/// goes out of scope, including on panic. Without this, a panicking
/// test leaves the container running until the next `start()` clobbers
/// it.
impl Drop for QdrantLocal {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Block until Qdrant responds to `GET /` (returns the title/version).
pub async fn wait_ready() -> anyhow::Result<()> {
    let url = format!("{BASE_URL_FOR_HOST}/");
    let client = reqwest::Client::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while std::time::Instant::now() < deadline {
        if let Ok(r) = client.get(&url).send().await {
            if r.status().is_success() {
                return Ok(());
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    anyhow::bail!("Qdrant at {url} not ready after 20s")
}

pub async fn create_collection(name: &str, dimension: u32) -> anyhow::Result<()> {
    let url = format!("{BASE_URL_FOR_HOST}/collections/{name}");
    let body = serde_json::json!({
        "vectors": {"size": dimension, "distance": "Cosine"}
    });
    let resp = reqwest::Client::new().put(&url).json(&body).send().await?;
    if !resp.status().is_success() {
        let s = resp.status();
        let t = resp.text().await.unwrap_or_default();
        anyhow::bail!("create_collection {name}: {s} {t}");
    }
    Ok(())
}

pub async fn upsert(
    collection: &str,
    namespace: &str,
    model: &str,
    point_id: &str,
    prompt_sha: &str,
    vector: Vec<f32>,
    cached_payload: &str,
) -> anyhow::Result<()> {
    let url = format!("{BASE_URL_FOR_HOST}/collections/{collection}/points?wait=true");
    let body = serde_json::json!({
        "points": [{
            "id": point_id,
            "vector": vector,
            "payload": {
                "namespace": namespace,
                "model": model,
                "prompt_sha": prompt_sha,
                "cached": cached_payload,
            }
        }]
    });
    let resp = reqwest::Client::new().put(&url).json(&body).send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("upsert: {}", resp.status());
    }
    Ok(())
}

pub async fn count_points(collection: &str) -> anyhow::Result<usize> {
    let url = format!("{BASE_URL_FOR_HOST}/collections/{collection}");
    let resp = reqwest::Client::new().get(&url).send().await?;
    let v: serde_json::Value = resp.json().await?;
    Ok(v.get("result")
        .and_then(|r| r.get("points_count"))
        .and_then(|n| n.as_u64())
        .unwrap_or(0) as usize)
}

/// Deterministic UUIDv5-shaped point id. Mirrors the policy's
/// derivation in `PdkQdrantStore::point_id`. Same input → same id.
pub fn point_id(namespace: &str, model: &str, prompt_sha: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(namespace.as_bytes());
    h.update(b":");
    h.update(model.as_bytes());
    h.update(b":");
    h.update(prompt_sha.as_bytes());
    let digest = h.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3],
        bytes[4], bytes[5],
        bytes[6], bytes[7],
        bytes[8], bytes[9],
        bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
    )
}
