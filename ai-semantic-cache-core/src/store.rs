// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Salesforce, Inc.

//! Vector-store contract shared by every variant binary.
//!
//! ## Design: trait + dynamic dispatch (vs free per-backend functions)
//!
//! Each variant exposes its REST surface through a [`VectorStore`] trait
//! impl rather than free async functions. The [`Engine`](crate::engine::Engine)
//! is generic over the trait, so:
//! - Core can be unit-tested against in-process fakes without depending
//!   on PDK or any specific vector-DB backend.
//! - Each variant's `Pdk*Store` impl is compact REST glue (50–80 LOC)
//!   plus a thin entrypoint that hands an `Rc<dyn VectorStore>` to
//!   [`crate::driver::run`] via the per-request [`StoreFactory`](crate::driver::StoreFactory).
//!
//! There is exactly one `VectorStore` impl per variant binary, so the
//! `dyn` dispatch cost is one vtable slot per request — negligible
//! relative to the network round-trip it dispatches.

use crate::error::StoreError;
use crate::key::CacheKey;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedPayload {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    /// Serde encodes/decodes as a base64 string rather than a JSON array of
    /// byte integers. Cuts on-wire size ~3× and keeps individual Pinecone
    /// vector metadata under the 40 KB per-vector cap on typical chat
    /// completion responses.
    #[serde(with = "body_base64")]
    pub body: Vec<u8>,
    /// Used for `tokens_saved_estimate` telemetry on hits.
    pub usage_total_tokens: Option<u64>,
    /// UNIX seconds at which this entry was written.
    pub written_at: u64,
}

mod body_base64 {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        STANDARD.decode(s).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone)]
pub struct Match {
    pub key: CacheKey,
    pub score: f32,
    pub payload: CachedPayload,
}

/// Implementations target single-threaded wasm runtimes — see `Embedder` for
/// the same rationale. `?Send` futures, no `Send + Sync` super-bounds.
#[async_trait(?Send)]
pub trait VectorStore {
    async fn upsert(
        &self,
        key: &CacheKey,
        vector: &[f32],
        payload: &CachedPayload,
        ttl: Duration,
    ) -> Result<(), StoreError>;

    async fn query_topk(
        &self,
        namespace: &str,
        model: &str,
        vector: &[f32],
        k: usize,
    ) -> Result<Vec<Match>, StoreError>;

    async fn get_exact(&self, key: &CacheKey) -> Result<Option<CachedPayload>, StoreError>;

    async fn delete(&self, key: &CacheKey) -> Result<(), StoreError>;
}
