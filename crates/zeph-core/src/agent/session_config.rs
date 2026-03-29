// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::sync::Arc;

use crate::config::{
    Config, DebugConfig, DocumentConfig, GraphConfig, LearningConfig, OrchestrationConfig,
    SecurityConfig, TimeoutConfig,
};
use crate::vault::Secret;

/// Reserve ratio for `with_context_budget`: fraction of budget reserved for LLM reply.
///
/// Extracted from the hardcoded `0.20` literal used in both `spawn_acp_agent` and `runner.rs`.
pub const CONTEXT_BUDGET_RESERVE_RATIO: f32 = 0.20;

/// All config-derived fields needed to configure an `Agent` session.
///
/// This is the single source of truth for config → agent wiring.
/// Adding a new config field requires exactly three changes:
///
/// 1. Add the field here.
/// 2. Map it in [`AgentSessionConfig::from_config`].
/// 3. Apply it in [`super::Agent::apply_session_config`] (destructure triggers a compile error if
///    you forget step 3 — see the S4 note in the critic handoff).
///
/// ## What is NOT here
///
/// - **Shared runtime objects** (`provider`, `registry`, `memory`, `mcp_manager`, etc.) — these
///   are expensive to create and shared across sessions; they stay in `SharedAgentDeps`.
/// - **ACP-specific fields** (`acp_max_sessions`, bearer token, etc.) — transport-level, not
///   agent-level.
/// - **Optional runtime providers** (`summary_provider`, `judge_provider`,
///   `quarantine_provider`) — these contain HTTP client pools (`AnyProvider`) that carry runtime
///   state; callers wire them separately via `with_summary_provider` / `with_judge_provider` /
///   `apply_quarantine_provider`.
/// - **`mcp_config`** — passed alongside runtime MCP objects in `with_mcp()`; separating it
///   from `mcp_tools` / `mcp_manager` would make the call site awkward.
/// - **Runner-only fields** (`compression`, `routing`, `autosave`, `hybrid_search`, `trust_config`,
///   `disambiguation_threshold`, `logging_config`, `subagent`, `experiment`, `instruction`,
///   `lsp_hooks`, `response_cache`, `cost_tracker`) — not used in ACP sessions; keeping them out
///   avoids unused-field noise and prevents inadvertent ACP behavior changes.
/// - **Scheduler runtime objects** (`scheduler_executor`, broadcast senders) — runtime state,
///   not config-derived values.
#[derive(Clone)]
#[allow(clippy::struct_excessive_bools)]
pub struct AgentSessionConfig {
    // Tool behavior
    pub max_tool_iterations: usize,
    pub max_tool_retries: usize,
    pub max_retry_duration_secs: u64,
    pub retry_base_ms: u64,
    pub retry_max_ms: u64,
    pub parameter_reformat_provider: String,
    pub tool_repeat_threshold: usize,
    pub tool_summarization: bool,
    pub tool_call_cutoff: usize,
    pub overflow_config: zeph_tools::OverflowConfig,
    pub permission_policy: zeph_tools::PermissionPolicy,

    // Model
    pub model_name: String,
    pub embed_model: String,

    // Semantic cache
    pub semantic_cache_enabled: bool,
    pub semantic_cache_threshold: f32,
    pub semantic_cache_max_candidates: u32,

    // Memory / compaction
    pub budget_tokens: usize,
    pub soft_compaction_threshold: f32,
    pub hard_compaction_threshold: f32,
    pub compaction_preserve_tail: usize,
    pub compaction_cooldown_turns: u8,
    pub prune_protect_tokens: usize,
    pub redact_credentials: bool,

    // Security
    pub security: SecurityConfig,
    pub timeouts: TimeoutConfig,

    // Feature configs
    pub learning: LearningConfig,
    pub document_config: DocumentConfig,
    pub graph_config: GraphConfig,
    pub anomaly_config: zeph_tools::AnomalyConfig,
    pub result_cache_config: zeph_tools::ResultCacheConfig,
    pub orchestration_config: OrchestrationConfig,
    pub debug_config: DebugConfig,
    pub server_compaction: bool,

    /// Custom secrets from config.
    ///
    /// Stored as `Arc` because `Secret` intentionally does not implement `Clone` —
    /// the wrapper prevents accidental duplication. Iteration produces new `Secret`
    /// values via `Secret::new(v.expose())` on the consumption side.
    pub secrets: Arc<[(String, Secret)]>,
}

impl AgentSessionConfig {
    /// Build from a resolved [`Config`] snapshot and a pre-computed `budget_tokens`.
    ///
    /// `budget_tokens` is passed as a parameter because its computation (`auto_budget_tokens`)
    /// depends on the active provider and must happen before `AgentSessionConfig` is constructed.
    #[must_use]
    pub fn from_config(config: &Config, budget_tokens: usize) -> Self {
        Self {
            max_tool_iterations: config.agent.max_tool_iterations,
            max_tool_retries: config.tools.retry.max_attempts,
            max_retry_duration_secs: config.tools.retry.budget_secs,
            retry_base_ms: config.tools.retry.base_ms,
            retry_max_ms: config.tools.retry.max_ms,
            parameter_reformat_provider: config.tools.retry.parameter_reformat_provider.clone(),
            tool_repeat_threshold: config.agent.tool_repeat_threshold,
            tool_summarization: config.tools.summarize_output,
            tool_call_cutoff: config.memory.tool_call_cutoff,
            overflow_config: config.tools.overflow.clone(),
            permission_policy: config
                .tools
                .permission_policy(config.security.autonomy_level),
            model_name: config.llm.effective_model().to_owned(),
            embed_model: crate::bootstrap::effective_embedding_model(config),
            semantic_cache_enabled: config.llm.semantic_cache_enabled,
            semantic_cache_threshold: config.llm.semantic_cache_threshold,
            semantic_cache_max_candidates: config.llm.semantic_cache_max_candidates,
            budget_tokens,
            soft_compaction_threshold: config.memory.soft_compaction_threshold,
            hard_compaction_threshold: config.memory.hard_compaction_threshold,
            compaction_preserve_tail: config.memory.compaction_preserve_tail,
            compaction_cooldown_turns: config.memory.compaction_cooldown_turns,
            prune_protect_tokens: config.memory.prune_protect_tokens,
            redact_credentials: config.memory.redact_credentials,
            security: config.security.clone(),
            timeouts: config.timeouts,
            learning: config.skills.learning.clone(),
            document_config: config.memory.documents.clone(),
            graph_config: config.memory.graph.clone(),
            anomaly_config: config.tools.anomaly.clone(),
            result_cache_config: config.tools.result_cache.clone(),
            orchestration_config: config.orchestration.clone(),
            debug_config: config.debug.clone(),
            server_compaction: config.llm.providers.iter().any(|e| e.server_compaction),
            secrets: config
                .secrets
                .custom
                .iter()
                .map(|(k, v)| (k.clone(), Secret::new(v.expose().to_owned())))
                .collect::<Vec<_>>()
                .into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_config_maps_all_fields() {
        let config = Config::default();
        let budget = 100_000;
        let sc = AgentSessionConfig::from_config(&config, budget);

        assert_eq!(sc.max_tool_iterations, config.agent.max_tool_iterations);
        assert_eq!(sc.max_tool_retries, config.tools.retry.max_attempts);
        assert_eq!(sc.max_retry_duration_secs, config.tools.retry.budget_secs);
        assert_eq!(sc.retry_base_ms, config.tools.retry.base_ms);
        assert_eq!(sc.retry_max_ms, config.tools.retry.max_ms);
        assert_eq!(
            sc.parameter_reformat_provider,
            config.tools.retry.parameter_reformat_provider
        );
        assert_eq!(sc.tool_repeat_threshold, config.agent.tool_repeat_threshold);
        assert_eq!(sc.tool_summarization, config.tools.summarize_output);
        assert_eq!(sc.tool_call_cutoff, config.memory.tool_call_cutoff);
        assert_eq!(sc.model_name, config.llm.effective_model());
        assert_eq!(
            sc.embed_model,
            crate::bootstrap::effective_embedding_model(&config)
        );
        assert_eq!(sc.semantic_cache_enabled, config.llm.semantic_cache_enabled);
        assert!(
            (sc.semantic_cache_threshold - config.llm.semantic_cache_threshold).abs()
                < f32::EPSILON
        );
        assert_eq!(
            sc.semantic_cache_max_candidates,
            config.llm.semantic_cache_max_candidates
        );
        assert_eq!(sc.budget_tokens, budget);
        assert!(
            (sc.soft_compaction_threshold - config.memory.soft_compaction_threshold).abs()
                < f32::EPSILON
        );
        assert!(
            (sc.hard_compaction_threshold - config.memory.hard_compaction_threshold).abs()
                < f32::EPSILON
        );
        assert_eq!(
            sc.compaction_preserve_tail,
            config.memory.compaction_preserve_tail
        );
        assert_eq!(
            sc.compaction_cooldown_turns,
            config.memory.compaction_cooldown_turns
        );
        assert_eq!(sc.prune_protect_tokens, config.memory.prune_protect_tokens);
        assert_eq!(sc.redact_credentials, config.memory.redact_credentials);
        assert_eq!(sc.graph_config.enabled, config.memory.graph.enabled);
        assert_eq!(
            sc.orchestration_config.enabled,
            config.orchestration.enabled
        );
        assert_eq!(
            sc.orchestration_config.max_tasks,
            config.orchestration.max_tasks
        );
        assert_eq!(sc.anomaly_config.enabled, config.tools.anomaly.enabled);
        assert_eq!(
            sc.result_cache_config.enabled,
            config.tools.result_cache.enabled
        );
        assert_eq!(
            sc.result_cache_config.ttl_secs,
            config.tools.result_cache.ttl_secs
        );
        assert_eq!(sc.debug_config.enabled, config.debug.enabled);
        assert_eq!(
            sc.document_config.rag_enabled,
            config.memory.documents.rag_enabled
        );
        assert_eq!(
            sc.overflow_config.threshold,
            config.tools.overflow.threshold
        );
        assert_eq!(
            sc.permission_policy.autonomy_level(),
            config.security.autonomy_level
        );
        assert_eq!(sc.security.autonomy_level, config.security.autonomy_level);
        assert_eq!(sc.timeouts.llm_seconds, config.timeouts.llm_seconds);
        assert_eq!(sc.learning.enabled, config.skills.learning.enabled);
        assert_eq!(
            sc.server_compaction,
            config.llm.providers.iter().any(|e| e.server_compaction)
        );
        assert_eq!(sc.secrets.len(), config.secrets.custom.len());
    }
}
