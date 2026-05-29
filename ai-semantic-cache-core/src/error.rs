// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Salesforce, Inc.

use std::fmt;

#[derive(Debug)]
pub enum EmbedError {
    Unauthorized,
    Upstream { status: u16, body: String },
    Timeout,
    Malformed(String),
    DimensionMismatch { expected: usize, got: usize },
}

impl fmt::Display for EmbedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EmbedError::Unauthorized => write!(f, "embedder: unauthorized"),
            EmbedError::Upstream { status, body } => {
                write!(f, "embedder: upstream {status}: {body}")
            }
            EmbedError::Timeout => write!(f, "embedder: timeout"),
            EmbedError::Malformed(m) => write!(f, "embedder: malformed response: {m}"),
            EmbedError::DimensionMismatch { expected, got } => write!(
                f,
                "embedder: dimension mismatch: expected {expected}, got {got}"
            ),
        }
    }
}

impl std::error::Error for EmbedError {}

#[derive(Debug)]
pub enum StoreError {
    Upstream { status: u16, body: String },
    Timeout,
    Malformed(String),
    NotFound,
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::Upstream { status, body } => write!(f, "store: upstream {status}: {body}"),
            StoreError::Timeout => write!(f, "store: timeout"),
            StoreError::Malformed(m) => write!(f, "store: malformed: {m}"),
            StoreError::NotFound => write!(f, "store: not found"),
        }
    }
}

impl std::error::Error for StoreError {}

#[derive(Debug)]
pub enum CacheError {
    Embed(EmbedError),
    Store(StoreError),
    Extract(String),
}

impl fmt::Display for CacheError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CacheError::Embed(e) => write!(f, "{e}"),
            CacheError::Store(e) => write!(f, "{e}"),
            CacheError::Extract(m) => write!(f, "extract: {m}"),
        }
    }
}

impl std::error::Error for CacheError {}

impl From<EmbedError> for CacheError {
    fn from(e: EmbedError) -> Self {
        CacheError::Embed(e)
    }
}
impl From<StoreError> for CacheError {
    fn from(e: StoreError) -> Self {
        CacheError::Store(e)
    }
}
