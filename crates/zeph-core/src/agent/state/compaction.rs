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
    pub(crate) compression_guidelines_config: zeph_memory::CompressionGuidelinesConfig,
    /// When `true`, a shutdown summary is generated when the agent exits cleanly.
    pub(crate) shutdown_summary: bool,
    /// Minimum number of messages required to generate a shutdown summary.
    pub(crate) shutdown_summary_min_messages: usize,
    /// Maximum number of messages included in a shutdown summary.
    pub(crate) shutdown_summary_max_messages: usize,
    /// Timeout (in seconds) for the shutdown summary LLM call.
    pub(crate) shutdown_summary_timeout_secs: u64,
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
}

impl Default for MemoryCompactionState {
    fn default() -> Self {
        Self {
            summarization_threshold: 50,
            compression_guidelines_config: zeph_memory::CompressionGuidelinesConfig::default(),
            shutdown_summary: true,
            shutdown_summary_min_messages: 4,
            shutdown_summary_max_messages: 20,
            shutdown_summary_timeout_secs: 10,
            structured_summaries: false,
            digest_config: crate::config::DigestConfig::default(),
            cached_session_digest: None,
            context_strategy: crate::config::ContextStrategy::default(),
            crossover_turn_threshold: 20,
        }
    }
}
