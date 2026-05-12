// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Configuration types for the MARCH self-check quality pipeline.

use serde::{Deserialize, Serialize};
use thiserror::Error;
use zeph_config::providers::ProviderName;

/// When to run the self-check pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TriggerPolicy {
    /// Run only when the turn has retrieved context (semantic recall, summaries, cross-session).
    #[default]
    HasRetrieval,
    /// Always run regardless of retrieved context.
    Always,
    /// Never run automatically; only via explicit command.
    Manual,
}

/// Configuration for the MARCH self-check quality pipeline.
///
/// Add a `[quality]` section to your `config.toml` to enable:
///
/// ```toml
/// [quality]
/// self_check = true
/// trigger = "has_retrieval"
/// async_run = false
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QualityConfig {
    /// Enable post-response self-check pipeline.
    #[serde(default)]
    pub self_check: bool,

    /// Advisory: preferred provider for the Proposer role.
    ///
    /// In MVP this field is parsed but not acted upon — the main provider is used.
    /// Tracked as a follow-up issue.
    #[serde(default)]
    pub proposer_provider: ProviderName,

    /// Advisory: preferred provider for the Checker role.
    ///
    /// In MVP this field is parsed but not acted upon — the main provider is used.
    /// Tracked as a follow-up issue.
    #[serde(default)]
    pub checker_provider: ProviderName,

    /// When to trigger the self-check pipeline.
    #[serde(default)]
    pub trigger: TriggerPolicy,

    /// Minimum evidence strength to consider an assertion supported (0.0–1.0).
    ///
    /// Assertions where `status != Irrelevant && evidence < min_evidence` are flagged.
    #[serde(default = "default_min_evidence")]
    pub min_evidence: f32,

    /// If `false` (default), self-check blocks the response until complete.
    /// If `true`, it runs in the background and emits a visible closing marker.
    #[serde(default)]
    pub async_run: bool,

    /// Hard ceiling on total pipeline latency in milliseconds (sync mode).
    #[serde(default = "default_latency_budget_ms")]
    pub latency_budget_ms: u64,

    /// Per-LLM-call timeout in milliseconds. Must be ≤ `latency_budget_ms` / 2.
    #[serde(default = "default_per_call_timeout_ms")]
    pub per_call_timeout_ms: u64,

    /// Maximum number of assertions to extract from one response.
    #[serde(default = "default_max_assertions")]
    pub max_assertions: usize,

    /// Skip pipeline when assistant response exceeds this many characters.
    #[serde(default = "default_max_response_chars")]
    pub max_response_chars: usize,

    /// If `true`, Checker provider clones without prompt-cache emission (recommended).
    #[serde(default = "default_cache_disabled_for_checker")]
    pub cache_disabled_for_checker: bool,

    /// String appended to the assistant response when the pipeline flags issues.
    #[serde(default = "default_flag_marker")]
    pub flag_marker: String,
}

fn default_min_evidence() -> f32 {
    0.6
}
fn default_latency_budget_ms() -> u64 {
    4_000
}
fn default_per_call_timeout_ms() -> u64 {
    2_000
}
fn default_max_assertions() -> usize {
    12
}
fn default_max_response_chars() -> usize {
    8_000
}
fn default_cache_disabled_for_checker() -> bool {
    true
}
fn default_flag_marker() -> String {
    "[verify]".into()
}

impl Default for QualityConfig {
    fn default() -> Self {
        Self {
            self_check: false,
            proposer_provider: ProviderName::default(),
            checker_provider: ProviderName::default(),
            trigger: TriggerPolicy::default(),
            min_evidence: default_min_evidence(),
            async_run: false,
            latency_budget_ms: default_latency_budget_ms(),
            per_call_timeout_ms: default_per_call_timeout_ms(),
            max_assertions: default_max_assertions(),
            max_response_chars: default_max_response_chars(),
            cache_disabled_for_checker: default_cache_disabled_for_checker(),
            flag_marker: default_flag_marker(),
        }
    }
}

/// Errors returned by [`QualityConfig::validate`].
#[derive(Debug, Error)]
pub enum QualityConfigError {
    #[error("per_call_timeout_ms ({per_call}) × 2 must be ≤ latency_budget_ms ({budget})")]
    TimeoutExceedsBudget { per_call: u64, budget: u64 },
    #[error("min_evidence must be in 0.0..=1.0, got {0}")]
    InvalidMinEvidence(f32),
}

impl QualityConfig {
    /// Validate consistency constraints.
    ///
    /// # Errors
    ///
    /// Returns an error if `2 * per_call_timeout_ms > latency_budget_ms` or
    /// `min_evidence` is outside `[0.0, 1.0]`.
    pub fn validate(&self) -> Result<(), QualityConfigError> {
        if 2 * self.per_call_timeout_ms > self.latency_budget_ms {
            return Err(QualityConfigError::TimeoutExceedsBudget {
                per_call: self.per_call_timeout_ms,
                budget: self.latency_budget_ms,
            });
        }
        if !(0.0..=1.0).contains(&self.min_evidence) {
            return Err(QualityConfigError::InvalidMinEvidence(self.min_evidence));
        }
        Ok(())
    }
}

impl From<&zeph_config::QualityConfig> for QualityConfig {
    fn from(c: &zeph_config::QualityConfig) -> Self {
        Self {
            self_check: c.self_check,
            proposer_provider: c.proposer_provider.clone(),
            checker_provider: c.checker_provider.clone(),
            trigger: match c.trigger {
                zeph_config::TriggerPolicy::HasRetrieval => TriggerPolicy::HasRetrieval,
                zeph_config::TriggerPolicy::Always => TriggerPolicy::Always,
                zeph_config::TriggerPolicy::Manual => TriggerPolicy::Manual,
            },
            min_evidence: c.min_evidence,
            async_run: c.async_run,
            latency_budget_ms: c.latency_budget_ms,
            per_call_timeout_ms: c.per_call_timeout_ms,
            max_assertions: c.max_assertions,
            max_response_chars: c.max_response_chars,
            cache_disabled_for_checker: c.cache_disabled_for_checker,
            flag_marker: c.flag_marker.clone(),
        }
    }
}
