// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Salesforce, Inc.

use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FailureMode {
    FailOpen,
    FailClosed,
}

impl Default for FailureMode {
    fn default() -> Self {
        FailureMode::FailOpen
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum DistanceMetric {
    Cosine,
    L2,
    Ip,
}

impl Default for DistanceMetric {
    fn default() -> Self {
        DistanceMetric::Cosine
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PromptExtractorKind {
    Openai,
    Anthropic,
    Cohere,
    Mistral,
    Bedrock,
    Vertex,
    Dataweave,
}

#[derive(Debug, Clone, Copy)]
pub struct Threshold(f32);

impl Threshold {
    /// Accepts values in `[0.0, 1.5]`. Cosine-similarity scores live in
    /// `[-1, 1]`, so any value `> 1.0` makes every semantic comparison
    /// fail — operators use `1.01` (or higher) as the documented
    /// "disable semantic, run as exact-cache only" convention. The
    /// upper bound matches the policy's `gcl.yaml` schema cap.
    pub fn new(v: f32) -> Result<Self, String> {
        if !(0.0..=1.5).contains(&v) || v.is_nan() {
            return Err(format!("threshold must be in [0.0, 1.5], got {v}"));
        }
        Ok(Self(v))
    }
    pub fn value(&self) -> f32 {
        self.0
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Ttl(Duration);

impl Ttl {
    pub fn from_seconds(secs: u64) -> Self {
        Self(Duration::from_secs(secs))
    }
    pub fn as_duration(&self) -> Duration {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn threshold_accepts_valid_range() {
        assert!(Threshold::new(0.0).is_ok());
        assert!(Threshold::new(0.92).is_ok());
        assert!(Threshold::new(1.0).is_ok());
        assert!(Threshold::new(1.01).is_ok()); // documented "disable semantic" convention
        assert!(Threshold::new(1.5).is_ok());
    }

    #[test]
    fn threshold_rejects_out_of_range() {
        assert!(Threshold::new(-0.1).is_err());
        assert!(Threshold::new(1.51).is_err());
        assert!(Threshold::new(f32::NAN).is_err());
    }

    #[test]
    fn failure_mode_defaults_to_fail_open() {
        assert_eq!(FailureMode::default(), FailureMode::FailOpen);
    }

    #[test]
    fn distance_metric_defaults_to_cosine() {
        assert_eq!(DistanceMetric::default(), DistanceMetric::Cosine);
    }
}
