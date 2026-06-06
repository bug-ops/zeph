// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Reasoning, probing, and memory chain-of-thought configuration.
//!
//! Memory-augmented reasoning ([`ReasoningConfig`]), compaction probes, `MemCoT`
//! semantic state, auto-dream consolidation, and magic-docs synthesis.

use crate::providers::ProviderName;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// autoDream background memory consolidation configuration (#2697).
///
/// When `enabled = true`, a constrained consolidation subagent runs after
/// a session ends if both `min_sessions` and `min_hours` gates pass.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(default)]
pub struct AutoDreamConfig {
    /// Enable autoDream consolidation. Default: `false`.
    pub enabled: bool,
    /// Minimum number of sessions between consolidations. Default: `3`.
    pub min_sessions: u32,
    /// Minimum hours between consolidations. Default: `24`.
    pub min_hours: u32,
    /// Provider name from `[[llm.providers]]` for consolidation LLM calls.
    /// Falls back to the primary provider when empty. Default: `""`.
    pub consolidation_provider: ProviderName,
    /// Maximum agent loop iterations for the consolidation subagent. Default: `8`.
    pub max_iterations: u8,
    /// LLM call timeout per `propose_merge_op` invocation, in seconds. Default: `30`.
    #[serde(default = "default_autodream_llm_timeout_secs")]
    pub llm_timeout_secs: u64,
}

impl Default for AutoDreamConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            min_sessions: 3,
            min_hours: 24,
            consolidation_provider: ProviderName::default(),
            max_iterations: 8,
            llm_timeout_secs: default_autodream_llm_timeout_secs(),
        }
    }
}

fn default_autodream_llm_timeout_secs() -> u64 {
    30
}

/// `MagicDocs` auto-maintained markdown configuration (#2702).
///
/// When `enabled = true`, files read via file tools that contain a `# MAGIC DOC:` header
/// are registered and periodically updated by a constrained subagent.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(default)]
pub struct MagicDocsConfig {
    /// Enable `MagicDocs` auto-maintenance. Default: `false`.
    pub enabled: bool,
    /// Minimum turns between updates for a given doc path. Default: `5`.
    pub min_turns_between_updates: u32,
    /// Provider name from `[[llm.providers]]` for doc update LLM calls.
    /// Falls back to the primary provider when empty. Default: `""`.
    pub update_provider: ProviderName,
    /// Maximum agent loop iterations per doc update. Default: `4`.
    pub max_iterations: u8,
}

impl Default for MagicDocsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            min_turns_between_updates: 5,
            update_provider: ProviderName::default(),
            max_iterations: 4,
        }
    }
}

/// `ReasoningBank`: distilled reasoning strategy memory configuration (#3342).
///
/// When `enabled = true`, each completed agent turn is evaluated by a self-judge LLM call.
/// Successful and failed reasoning chains are compressed into short, generalizable strategy
/// summaries. At context-build time, top-k strategies are retrieved by embedding similarity
/// and injected into the prompt preamble.
///
/// All LLM work (self-judge, distillation) runs asynchronously — never on the turn thread.
///
/// # Example
///
/// ```toml
/// [memory.reasoning]
/// enabled = true
/// extract_provider = "fast"
/// distill_provider = "fast"
/// top_k = 3
/// store_limit = 1000
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ReasoningConfig {
    /// Enable the reasoning-bank pipeline. Default: `false`.
    pub enabled: bool,
    /// Provider name from `[[llm.providers]]` for the self-judge step.
    /// Falls back to the primary provider when empty. Default: `""`.
    pub extract_provider: ProviderName,
    /// Provider name from `[[llm.providers]]` for the distillation step.
    /// Falls back to the primary provider when empty. Default: `""`.
    pub distill_provider: ProviderName,
    /// Number of strategies retrieved per turn for context injection. Default: `3`.
    pub top_k: usize,
    /// Maximum stored strategies; oldest unused are evicted when limit is reached. Default: `1000`.
    pub store_limit: usize,
    /// Maximum number of recent messages passed to the self-judge LLM. Default: `6`.
    pub max_messages: usize,
    /// Per-message content truncation limit (chars) before building the judge transcript. Default: `2000`.
    pub max_message_chars: usize,
    /// Maximum token budget for injected reasoning strategies in context. Default: `500`.
    pub context_budget_tokens: usize,
    /// Minimum number of messages required before self-judge fires. Default: `2`.
    pub min_messages: usize,
    /// Timeout in seconds for the self-judge LLM call. Default: `30`.
    pub extraction_timeout_secs: u64,
    /// Timeout in seconds for the distillation LLM call. Default: `30`.
    pub distill_timeout_secs: u64,
    /// Maximum number of recent messages passed to the self-judge evaluator.
    /// Narrowing to the last user+assistant pair improves classification accuracy.
    /// Default: `2`.
    pub self_judge_window: usize,
    /// Minimum characters in the assistant response to trigger self-judge.
    /// Short or trivial responses are skipped. Default: `50`.
    pub min_assistant_chars: usize,
}

impl Default for ReasoningConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            extract_provider: ProviderName::default(),
            distill_provider: ProviderName::default(),
            top_k: 3,
            store_limit: 1000,
            max_messages: 6,
            max_message_chars: 2000,
            context_budget_tokens: 500,
            min_messages: 2,
            extraction_timeout_secs: 30,
            distill_timeout_secs: 30,
            self_judge_window: 2,
            min_assistant_chars: 50,
        }
    }
}

// ── Compaction probe config (moved from zeph-memory) ─────────────────────────

/// Functional category of a compaction probe question.
///
/// `zeph-memory` re-exports this type from here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum ProbeCategory {
    /// Did specific facts survive? (file paths, function names, values, decisions)
    Recall,
    /// Does the agent know which files/tools/URLs it used?
    Artifact,
    /// Can it pick up mid-task? (current step, next steps, blockers, open questions)
    Continuation,
    /// Are past reasoning traces intact? (why X over Y, trade-offs, constraints)
    Decision,
}

/// Configuration for the compaction probe.
///
/// `zeph-memory` re-exports this type from here.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CompactionProbeConfig {
    /// Enable compaction probe validation. Default: `false`.
    pub enabled: bool,
    /// Provider name from `[[llm.providers]]` for probe LLM calls.
    /// `None` (or `Some("")`) uses the summary provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe_provider: Option<ProviderName>,
    /// Minimum score to pass without warnings. Default: `0.6`.
    pub threshold: f32,
    /// Score below this triggers `HardFail` (block compaction). Default: `0.35`.
    pub hard_fail_threshold: f32,
    /// Maximum number of probe questions to generate. Default: `5`.
    pub max_questions: usize,
    /// Timeout for the entire probe (both LLM calls) in seconds. Default: `15`.
    pub timeout_secs: u64,
    /// Optional per-category weight multipliers for the overall score.
    #[serde(default)]
    pub category_weights: Option<HashMap<ProbeCategory, f32>>,
}

impl Default for CompactionProbeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            probe_provider: None,
            threshold: 0.6,
            hard_fail_threshold: 0.35,
            max_questions: 5,
            timeout_secs: 15,
            category_weights: None,
        }
    }
}

// ── MemCoT semantic state config ─────────────────────────────────────────────

/// `MemCoT` semantic-state distillation configuration.
///
/// When `enabled = true`, the agent maintains a short rolling "semantic state" buffer
/// summarizing conceptual progress across turns. This buffer is injected into graph
/// recall queries to improve retrieval relevance.
///
/// All LLM work (distillation) runs asynchronously — never on the turn thread.
/// When `enabled = false`, this is a **complete no-op**: no allocation, no LLM calls.
///
/// # Config example
///
/// ```toml
/// [memory.memcot]
/// enabled = true
/// distill_provider = "fast"
/// distill_timeout_secs = 5
/// min_assistant_chars = 200
/// min_distill_interval_secs = 30
/// max_distills_per_session = 50
/// max_state_chars = 800
/// recall_view = "head"
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MemCotConfig {
    /// Enable the `MemCoT` semantic state pipeline. Default: `false`.
    ///
    /// When `false`, the accumulator is never allocated and no LLM calls are made.
    pub enabled: bool,
    /// Provider name from `[[llm.providers]]` for distillation.
    ///
    /// Must reference a **fast-tier** provider (e.g. `gpt-4o-mini`, `qwen3:8b`).
    /// A startup warning is emitted when the resolved model does not look fast-tier.
    /// Falls back to the primary provider when empty. Default: `""`.
    pub distill_provider: ProviderName,
    /// Timeout in seconds for each distillation LLM call. Default: `5`.
    pub distill_timeout_secs: u64,
    /// Minimum characters in the assistant response to trigger distillation.
    /// Short or trivial replies are skipped. Default: `200`.
    pub min_assistant_chars: usize,
    /// Minimum elapsed seconds between successive distillation spawns. Default: `30`.
    ///
    /// Prevents runaway costs on long sessions with rapid turns.
    /// Clearing `/new` resets this counter.
    pub min_distill_interval_secs: u64,
    /// Maximum distillation spawns per conversation session. Default: `50`.
    ///
    /// Once this cap is reached the accumulator stops distilling for the rest of the
    /// session. Counter is reset when the user sends `/new`.
    pub max_distills_per_session: u64,
    /// Maximum characters for the semantic state buffer (UTF-8 char boundary truncation).
    /// Default: `800`.
    pub max_state_chars: usize,
    /// Recall view applied when `MemCoT` is active. Default: `Head`.
    ///
    /// - `head`: standard retrieval, no enrichment (suitable for low-latency setups).
    /// - `zoom_in`: adds source-message provenance to each returned fact.
    /// - `zoom_out`: expands 1-hop neighbors per returned fact.
    ///
    /// TODO(F3): add a per-call override parameter on `recall_graph_view`.
    pub recall_view: RecallViewConfig,
    /// Maximum 1-hop neighbor facts per head fact in `zoom_out` view. Default: `3`.
    pub zoom_out_neighbor_cap: usize,
    /// Optional model name allowlist for the fast-tier soft validator (lowercase substring match).
    /// Empty (default) → falls back to the built-in `FAST_TIER_MODEL_HINTS` list.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fast_tier_models: Vec<String>,
}

/// Recall view variant exposed in config.
///
/// Maps 1-to-1 to `zeph_memory::RecallView`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RecallViewConfig {
    /// Standard retrieval — no enrichment. Byte-identical to legacy behaviour.
    #[default]
    Head,
    /// Adds source-message provenance to each returned fact.
    ZoomIn,
    /// Expands 1-hop neighbor facts per returned fact.
    ZoomOut,
}

impl Default for MemCotConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            distill_provider: ProviderName::default(),
            distill_timeout_secs: 5,
            min_assistant_chars: 200,
            min_distill_interval_secs: 30,
            max_distills_per_session: 50,
            max_state_chars: 800,
            recall_view: RecallViewConfig::Head,
            zoom_out_neighbor_cap: 3,
            fast_tier_models: Vec::new(),
        }
    }
}
