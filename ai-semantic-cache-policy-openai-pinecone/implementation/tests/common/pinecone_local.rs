// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Salesforce, Inc.
//
// Lightweight harness around Pinecone Local
// (`ghcr.io/pinecone-io/pinecone-local:latest`) for integration tests.
//
// `pdk-test` doesn't let third-party crates implement its `Config` trait
// (the module is private), so the container is started via `docker run`
// directly. From inside the pdk-test composite network, the gateway reaches it
// via `http://host.docker.internal:<data-plane-port>` — Docker Desktop
// (mac/Windows) provides this hostname out of the box; Linux needs
// `--add-host=host.docker.internal:host-gateway` on the gateway container,
// which pdk-test currently does not support — so this harness is gated to
// macOS/Docker-Desktop CI runners.

use std::process::Command;

const IMAGE: &str = "ghcr.io/pinecone-io/pinecone-local:latest";
pub const CONTROL_PLANE_HOST_PORT: u16 = 5080;
pub const DATA_PLANE_HOST_PORT: u16 = 5081;

/// Reachable from inside the pdk-test docker network.
pub const INDEX_HOST_FOR_FLEX: &str = "http://host.docker.internal:5081";
/// Reachable from the host (cargo-test process).
pub const INDEX_HOST_FOR_HOST: &str = "http://localhost:5081";
pub const CONTROL_PLANE_FOR_HOST: &str = "http://localhost:5080";

pub struct PineconeLocal {
    container_id: String,
}

impl PineconeLocal {
    /// Start the container in the background. Idempotent — if a previous
    /// test left one running on the same name, kills it first.
    pub fn start() -> anyhow::Result<Self> {
        // Kill any leftover from a prior aborted test run.
        let _ = Command::new("docker")
            .args(["rm", "-f", "ai-semantic-cache-pc-local"])
            .output();

        let out = Command::new("docker")
            .args([
                "run",
                "-d",
                "--rm",
                "--name",
                "ai-semantic-cache-pc-local",
                "-p",
                &format!("{CONTROL_PLANE_HOST_PORT}:5080"),
                "-p",
                &format!("{DATA_PLANE_HOST_PORT}:5081"),
                "-e",
                "PORT=5080",
                "-e",
                "PINECONE_HOST=localhost",
                IMAGE,
            ])
            .output()?;
        if !out.status.success() {
            anyhow::bail!(
                "docker run pinecone-local failed: {}",
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
impl Drop for PineconeLocal {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Block until the control plane responds to `GET /indexes`.
pub async fn wait_ready() -> anyhow::Result<()> {
    let url = format!("{CONTROL_PLANE_FOR_HOST}/indexes");
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
    anyhow::bail!("Pinecone Local at {url} not ready after 20s")
}

pub async fn create_index(name: &str, dimension: u32) -> anyhow::Result<()> {
    let url = format!("{CONTROL_PLANE_FOR_HOST}/indexes");
    let body = serde_json::json!({
        "name": name,
        "dimension": dimension,
        "metric": "cosine",
        "spec": {"serverless": {"cloud": "aws", "region": "us-east-1"}}
    });
    let resp = reqwest::Client::new()
        .post(&url)
        .header("Api-Key", "test")
        .json(&body)
        .send()
        .await?;
    if !resp.status().is_success() {
        let s = resp.status();
        let t = resp.text().await.unwrap_or_default();
        anyhow::bail!("create_index {name}: {s} {t}");
    }
    Ok(())
}

pub async fn upsert(
    namespace: &str,
    model: &str,
    id: &str,
    vector: Vec<f32>,
    payload_json: &str,
) -> anyhow::Result<()> {
    let url = format!("{INDEX_HOST_FOR_HOST}/vectors/upsert");
    let body = serde_json::json!({
        "vectors": [{
            "id": id,
            "values": vector,
            "metadata": {"namespace": namespace, "model": model, "payload": payload_json}
        }]
    });
    let resp = reqwest::Client::new()
        .post(&url)
        .header("Api-Key", "test")
        .json(&body)
        .send()
        .await?;
    if !resp.status().is_success() {
        anyhow::bail!("upsert: {}", resp.status());
    }
    Ok(())
}

pub async fn count_vectors() -> anyhow::Result<usize> {
    // Use describe_index_stats (POST /describe_index_stats).
    let url = format!("{INDEX_HOST_FOR_HOST}/describe_index_stats");
    let resp = reqwest::Client::new()
        .post(&url)
        .header("Api-Key", "test")
        .json(&serde_json::json!({}))
        .send()
        .await?;
    let v: serde_json::Value = resp.json().await?;
    Ok(v.get("totalVectorCount").and_then(|n| n.as_u64()).unwrap_or(0) as usize)
}
