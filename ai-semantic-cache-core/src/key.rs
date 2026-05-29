// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Salesforce, Inc.

use sha2::{Digest, Sha256};

/// A canonicalized prompt that is safe to hash deterministically.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalPrompt(String);

impl CanonicalPrompt {
    /// Canonicalize a `(role, content)` message list into a single string.
    /// Rules:
    /// - Trim leading/trailing whitespace per message.
    /// - Collapse internal whitespace runs to single space (idempotent).
    /// - Preserve case.
    /// - Concatenate as `role\u{1F}content` joined by `\u{1E}`.
    pub fn from_messages(messages: &[(&str, &str)]) -> Self {
        let parts: Vec<String> = messages
            .iter()
            .map(|(role, content)| {
                let trimmed = content.trim();
                let collapsed: String = trimmed.split_whitespace().collect::<Vec<_>>().join(" ");
                format!("{role}\u{1F}{collapsed}")
            })
            .collect();
        Self(parts.join("\u{1E}"))
    }

    /// Single-string variant for non-messages providers (Bedrock InvokeModel etc.)
    pub fn from_text(text: &str) -> Self {
        let trimmed = text.trim();
        let collapsed: String = trimmed.split_whitespace().collect::<Vec<_>>().join(" ");
        Self(collapsed)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn sha256_hex(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.0.as_bytes());
        hex::encode(hasher.finalize())
    }
}

/// Composite cache key: `namespace:model:sha256(prompt)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheKey {
    pub namespace: String,
    pub model: String,
    pub prompt_sha: String,
}

impl CacheKey {
    pub fn new(namespace: &str, model: &str, prompt: &CanonicalPrompt) -> Self {
        Self {
            namespace: namespace.to_string(),
            model: model.to_string(),
            prompt_sha: prompt.sha256_hex(),
        }
    }

    pub fn as_redis_key(&self) -> String {
        format!("{}:{}:{}", self.namespace, self.model, self.prompt_sha)
    }

    /// Short prefix safe to expose in `X-Cache-Key` header for support correlation.
    pub fn correlation_prefix(&self) -> String {
        self.prompt_sha.chars().take(12).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonicalization_idempotent_across_whitespace() {
        let a = CanonicalPrompt::from_messages(&[("user", "  hello   world  ")]);
        let b = CanonicalPrompt::from_messages(&[("user", "hello world")]);
        assert_eq!(a, b);
    }

    #[test]
    fn canonicalization_preserves_case() {
        let a = CanonicalPrompt::from_messages(&[("user", "Hello")]);
        let b = CanonicalPrompt::from_messages(&[("user", "hello")]);
        assert_ne!(a, b);
    }

    #[test]
    fn cache_key_changes_with_model() {
        let p = CanonicalPrompt::from_messages(&[("user", "hi")]);
        let k1 = CacheKey::new("ns", "gpt-4o", &p);
        let k2 = CacheKey::new("ns", "gpt-3.5-turbo", &p);
        assert_ne!(k1.as_redis_key(), k2.as_redis_key());
    }

    #[test]
    fn cache_key_changes_with_namespace() {
        let p = CanonicalPrompt::from_messages(&[("user", "hi")]);
        let k1 = CacheKey::new("team-a", "gpt-4o", &p);
        let k2 = CacheKey::new("team-b", "gpt-4o", &p);
        assert_ne!(k1.as_redis_key(), k2.as_redis_key());
    }

    #[test]
    fn sha256_hex_is_64_chars() {
        let p = CanonicalPrompt::from_text("hello");
        assert_eq!(p.sha256_hex().len(), 64);
    }

    #[test]
    fn correlation_prefix_is_12_chars() {
        let p = CanonicalPrompt::from_text("hello");
        let k = CacheKey::new("ns", "gpt-4o", &p);
        assert_eq!(k.correlation_prefix().len(), 12);
    }
}
