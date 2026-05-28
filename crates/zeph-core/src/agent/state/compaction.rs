// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Context compaction and summarization state for the agent's memory subsystem.
//!
//! [`MemoryCompactionState`] groups fields that control how the agent compresses its context
//! window: summarization thresholds, shutdown summary behaviour, structured vs prose summaries,
//! session digests, and the context assembly strategy.

/// Summarization thresholds, compression guidelines, shutdown summary, and context strategy.
///
/// These fields are primarily accessed together in context summarization and digest operations.
/// Isolating them in their own struct reduces cognitive load when reasoning about compaction logic.
pub(crate) struct MemoryCompactionState {
    /// Number of unsummarized messages that triggers a compaction pass.
    pub(crate) summarization_threshold: usize,
    /// Configuration for compression guidelines injected into the summarization prompt.
    pub(crate) compression_guidelines_config: zeph_config::memory::CompressionGuidelinesConfig,
    /// When `true`, a shutdown summary is generated when the agent exits cleanly.
    pub(crate) shutdown_summary: bool,
    /// Minimum number of messages required to generate a shutdown summary.
    pub(crate) shutdown_summary_min_messages: usize,
    /// Maximum number of messages included in a shutdown summary.
    pub(crate) shutdown_summary_max_messages: usize,
    /// Timeout (in seconds) for the shutdown summary LLM call.
    pub(crate) shutdown_summary_timeout_secs: u64,
    /// Provider name for shutdown summarization LLM calls.
    ///
    /// Empty string → fall back to the primary provider via `resolve_background_provider`.
    pub(crate) shutdown_summary_provider: String,
    /// Provider name for deferred tool-pair summarization (context compaction).
    ///
    /// Empty string → fall back to the primary provider via `resolve_background_provider`.
    pub(crate) compaction_provider_name: String,
    /// When `true`, hard compaction uses `AnchoredSummary` (structured JSON) instead of
    /// free-form prose. Falls back to prose on any LLM or validation failure.
    pub(crate) structured_summaries: bool,
    /// Session digest configuration (#2289).
    pub(crate) digest_config: crate::config::DigestConfig,
    /// Cached session digest text and its token count, loaded at session start.
    pub(crate) cached_session_digest: Option<(String, usize)>,
    /// Context assembly strategy (#2288).
    pub(crate) context_strategy: crate::config::ContextStrategy,
    /// Turn threshold for `Adaptive` strategy crossover (#2288).
    pub(crate) crossover_turn_threshold: u32,
    /// CAM fidelity scoring configuration (#4547).
    ///
    /// `None` means fidelity scoring is not configured (disabled).
    pub(crate) fidelity_config: Option<zeph_config::FidelityConfig>,
}

impl Default for MemoryCompactionState {
    fn default() -> Self {
        Self {
            summarization_threshold: 50,
            compression_guidelines_config:
                zeph_config::memory::CompressionGuidelinesConfig::default(),
            shutdown_summary: true,
            shutdown_summary_min_messages: 4,
            shutdown_summary_max_messages: 20,
            shutdown_summary_timeout_secs: 30,
            shutdown_summary_provider: String::new(),
            compaction_provider_name: String::new(),
            structured_summaries: false,
            digest_config: crate::config::DigestConfig::default(),
            cached_session_digest: None,
            context_strategy: crate::config::ContextStrategy::default(),
            crossover_turn_threshold: 20,
            fidelity_config: None,
        }
    }
}
