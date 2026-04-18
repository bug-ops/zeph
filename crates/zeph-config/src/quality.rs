// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Configuration for the MARCH self-check quality pipeline.
//!
//! Add a `[quality]` section to `config.toml`:
//!
//! ```toml
//! [quality]
//! self_check = true
//! trigger = "has_retrieval"
//! async_run = false
//! ```

use serde::{Deserialize, Serialize};

/// When to trigger the self-check pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TriggerPolicy {
    /// Run only when the turn has retrieved context.
    #[default]
    HasRetrieval,
    /// Always run regardless of retrieved context.
    Always,
    /// Never run automatically.
    Manual,
}

/// Configuration for the MARCH self-check quality pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QualityConfig {
    /// Enable post-response self-check pipeline.
    #[serde(default)]
    pub self_check: bool,

    /// Advisory: preferred provider for the Proposer role (MVP: no-op).
    #[serde(default)]
    pub proposer_provider: String,

    /// Advisory: preferred provider for the Checker role (MVP: no-op).
    #[serde(default)]
    pub checker_provider: String,

    /// When to trigger the pipeline.
    #[serde(default)]
    pub trigger: TriggerPolicy,

    /// Minimum evidence strength to avoid flagging an assertion (0.0–1.0).
    #[serde(default = "default_min_evidence")]
    pub min_evidence: f32,

    /// If `false` (default), pipeline blocks response until done.
    #[serde(default)]
    pub async_run: bool,

    /// Hard ceiling on total pipeline latency in milliseconds.
    #[serde(default = "default_latency_budget_ms")]
    pub latency_budget_ms: u64,

    /// Per-LLM-call timeout in milliseconds.
    #[serde(default = "default_per_call_timeout_ms")]
    pub per_call_timeout_ms: u64,

    /// Maximum assertions to extract from one response.
    #[serde(default = "default_max_assertions")]
    pub max_assertions: usize,

    /// Skip pipeline when response exceeds this many characters.
    #[serde(default = "default_max_response_chars")]
    pub max_response_chars: usize,

    /// Suppress prompt-cache emission on Checker provider.
    #[serde(default = "default_cache_disabled_for_checker")]
    pub cache_disabled_for_checker: bool,

    /// Marker appended to response when issues are flagged.
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
            proposer_provider: String::new(),
            checker_provider: String::new(),
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
