// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Migration step wrapper structs — one per sequential migration in [`super::MIGRATIONS`].
//!
//! Steps 1–35 are pre-existing; steps 36–38 were added for the stable-defaults flip;
//! step 39 adds optional Qdrant API key; step 40 adds MCP `max_connect_attempts`;
//! steps 41–45 add goal lifecycle, TACO compression, orchestrator provider, per-provider
//! admission control, and `GonkaGate` advisory notice; step 46 is an advisory notice for Cocoon;
//! steps 47–48 add trace metadata and five-signal SYNAPSE config; step 49 renames
//! `embed_provider` → `embedding_provider` (#4480); step 50 adds MCP
//! `startup_retry_backoff_ms` and `tool_timeout_secs` (#4004); step 51 adds
//! `embed_timeout_secs` and `compress_timeout_secs` to `[memory.fidelity]` (#4645, #4651);
//! step 52 adds `[session] persist_provider_overrides` (#4654);
//! step 53 adds `[cocoon] show_balance` advisory notice (#4649);
//! step 54 adds `[worktree]` section with defaults (#4679);
//! step 55 adds `git_timeout_secs` to `[worktree]` (#4704);
//! step 56 adds `[llm.stream_limits]` advisory notice (#4750);
//! step 57 adds `[durable]` execution-layer section (spec-064, #4949);
//! step 58 renames `eval_model` → `eval_provider` (#4987);
//! step 59 adds `[caveman]` ultra-compressed output section (#4985);
//! step 60 adds `[tools.shell]` checkpoints (#4990);
//! step 61 adds `[knowledge]` section advisory notice (spec-067, #5017);
//! step 62 adds `[deep_link]` advisory notice (spec-066, #5011);
//! step 63 adds `recall_include_imported` to `[memory.graph]` (#5015);
//! step 64 adds `policy_provider` and `utility_window` advisory comments (#5067);
//! step 65 adds `[tui.theme]` advisory block (Theme System 2.0, #5087);
//! step 66 inserts active `name`/`color_mode` defaults into `[tui.theme]` (#5091);
//! step 67 adds `[tui] delights` advisory comment;
//! step 68 adds `mouse = false` advisory comment under `[tui]` (#5103);
//! step 69 adds `default_asset_sensitivity` advisory comment under `[orchestration]` (spec-068, #3934);
//! step 70 adds session-persistence keys and a `[session.condense]` advisory block (spec-068, #5343);
//! step 71 adds a `[serve]` advisory block for `zeph serve` (spec-068 §9, #5343);
//! step 72 adds a `[security.content_isolation.nli]` advisory block (#5438);
//! step 73 adds a `[security.content_isolation.secret_masking]` advisory block (#5437);
//! step 74 adds a commented `filter_names = false` advisory to an existing
//! `[security.pii_filter]` table (#5530);
//! step 75 adds a commented `qdrant_timeout_secs = 10` advisory under `[memory]`;
//! step 76 adds a commented `high_gain_tools = []` advisory under `[tools.utility]` (#5659);
//! step 77 adds a commented `[[acp.auth_clients]]` advisory block (#5868);
//! step 78 adds a commented `[skills.registry]` advisory block (spec-045, #5869);
//! step 79 adds a commented `shared_db = false` advisory to an existing active `[durable]`
//! table (INV-8 `encryption_gate`, #5996);
//! step 80 adds a commented `require_integrity_check_on_promote = true` advisory to an
//! existing active `[skills.trust]` table (#6087);
//! step 81 adds a commented `[security.shadow_sentinel]` advisory block (spec 050, #5934).
//!
//! Each struct is a zero-size type that delegates to the corresponding free function in
//! `super`. They exist solely to satisfy the object-safe [`super::Migration`] trait so the
//! registry can hold `Box<dyn Migration>` values.

use super::{
    MigrateError, Migration, MigrationResult, migrate_acp_auth_clients_config,
    migrate_acp_subagents_config, migrate_agent_budget_hint, migrate_agent_retry_to_tools_retry,
    migrate_autodream_config, migrate_caveman_config, migrate_cocoon_provider_notice,
    migrate_cocoon_show_balance, migrate_compression_predictor_config, migrate_database_url,
    migrate_deep_link_config, migrate_durable_config, migrate_durable_shared_db,
    migrate_egress_config, migrate_embed_provider_rename, migrate_eval_model_to_provider,
    migrate_fidelity_timeout_defaults, migrate_five_signal_config,
    migrate_focus_auto_consolidate_min_window, migrate_forgetting_config, migrate_goals_config,
    migrate_hooks_permission_denied_config, migrate_hooks_turn_complete_config,
    migrate_knowledge_config, migrate_llm_stream_limits, migrate_magic_docs_config,
    migrate_mcp_elicitation_config, migrate_mcp_max_connect_attempts,
    migrate_mcp_retry_and_tool_timeout, migrate_mcp_trust_levels, migrate_memory_graph_config,
    migrate_memory_graph_recall_include_imported, migrate_memory_hebbian_config,
    migrate_memory_hebbian_consolidation_config, migrate_memory_hebbian_spread_config,
    migrate_memory_persona_config, migrate_memory_reasoning_config,
    migrate_memory_reasoning_judge_config, migrate_memory_retrieval_config,
    migrate_memory_retrieval_query_bias, migrate_microcompact_config, migrate_nli_config,
    migrate_orchestration_asset_sensitivity, migrate_orchestration_orchestrator_provider,
    migrate_orchestration_persistence, migrate_otel_filter, migrate_pii_filter_names,
    migrate_planner_model_to_provider, migrate_policy_provider_and_utility_window,
    migrate_provider_max_concurrent, migrate_qdrant_api_key, migrate_qdrant_timeout_secs,
    migrate_quality_config, migrate_sandbox_config, migrate_sandbox_egress_filter,
    migrate_scheduler_daemon_config, migrate_secret_masking_config, migrate_serve_config,
    migrate_session_persist_provider_overrides, migrate_session_persistence_config,
    migrate_session_provider_persistence, migrate_session_recap_config,
    migrate_shadow_sentinel_config, migrate_shell_checkpoints_config, migrate_shell_transactional,
    migrate_skill_trust_require_check, migrate_skills_registry, migrate_stt_to_provider,
    migrate_supervisor_config, migrate_telemetry_config, migrate_tools_compression_config,
    migrate_trace_metadata, migrate_tui_delights, migrate_tui_mouse, migrate_tui_theme_config,
    migrate_tui_theme_defaults, migrate_utility_high_gain_tools, migrate_vigil_config,
    migrate_worktree_config, migrate_worktree_git_timeout,
};

// ── Wrapper structs for all 73 sequential migration steps ───────────────────────────────────────

pub(super) struct MigrateSttToProvider;
impl Migration for MigrateSttToProvider {
    fn name(&self) -> &'static str {
        "migrate_stt_to_provider"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_stt_to_provider(toml_src)
    }
}

pub(super) struct MigratePlannerModelToProvider;
impl Migration for MigratePlannerModelToProvider {
    fn name(&self) -> &'static str {
        "migrate_planner_model_to_provider"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_planner_model_to_provider(toml_src)
    }
}

pub(super) struct MigrateMcpTrustLevels;
impl Migration for MigrateMcpTrustLevels {
    fn name(&self) -> &'static str {
        "migrate_mcp_trust_levels"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_mcp_trust_levels(toml_src)
    }
}

pub(super) struct MigrateAgentRetryToToolsRetry;
impl Migration for MigrateAgentRetryToToolsRetry {
    fn name(&self) -> &'static str {
        "migrate_agent_retry_to_tools_retry"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_agent_retry_to_tools_retry(toml_src)
    }
}

pub(super) struct MigrateDatabaseUrl;
impl Migration for MigrateDatabaseUrl {
    fn name(&self) -> &'static str {
        "migrate_database_url"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_database_url(toml_src)
    }
}

pub(super) struct MigrateShellTransactional;
impl Migration for MigrateShellTransactional {
    fn name(&self) -> &'static str {
        "migrate_shell_transactional"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_shell_transactional(toml_src)
    }
}

pub(super) struct MigrateAgentBudgetHint;
impl Migration for MigrateAgentBudgetHint {
    fn name(&self) -> &'static str {
        "migrate_agent_budget_hint"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_agent_budget_hint(toml_src)
    }
}

pub(super) struct MigrateForgettingConfig;
impl Migration for MigrateForgettingConfig {
    fn name(&self) -> &'static str {
        "migrate_forgetting_config"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_forgetting_config(toml_src)
    }
}

pub(super) struct MigrateCompressionPredictorConfig;
impl Migration for MigrateCompressionPredictorConfig {
    fn name(&self) -> &'static str {
        "migrate_compression_predictor_config"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_compression_predictor_config(toml_src)
    }
}

pub(super) struct MigrateMicrocompactConfig;
impl Migration for MigrateMicrocompactConfig {
    fn name(&self) -> &'static str {
        "migrate_microcompact_config"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_microcompact_config(toml_src)
    }
}

pub(super) struct MigrateAutodreamConfig;
impl Migration for MigrateAutodreamConfig {
    fn name(&self) -> &'static str {
        "migrate_autodream_config"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_autodream_config(toml_src)
    }
}

pub(super) struct MigrateMagicDocsConfig;
impl Migration for MigrateMagicDocsConfig {
    fn name(&self) -> &'static str {
        "migrate_magic_docs_config"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_magic_docs_config(toml_src)
    }
}

pub(super) struct MigrateTelemetryConfig;
impl Migration for MigrateTelemetryConfig {
    fn name(&self) -> &'static str {
        "migrate_telemetry_config"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_telemetry_config(toml_src)
    }
}

pub(super) struct MigrateSupervisorConfig;
impl Migration for MigrateSupervisorConfig {
    fn name(&self) -> &'static str {
        "migrate_supervisor_config"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_supervisor_config(toml_src)
    }
}

pub(super) struct MigrateOtelFilter;
impl Migration for MigrateOtelFilter {
    fn name(&self) -> &'static str {
        "migrate_otel_filter"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_otel_filter(toml_src)
    }
}

pub(super) struct MigrateEgressConfig;
impl Migration for MigrateEgressConfig {
    fn name(&self) -> &'static str {
        "migrate_egress_config"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_egress_config(toml_src)
    }
}

pub(super) struct MigrateVigilConfig;
impl Migration for MigrateVigilConfig {
    fn name(&self) -> &'static str {
        "migrate_vigil_config"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_vigil_config(toml_src)
    }
}

pub(super) struct MigrateSandboxConfig;
impl Migration for MigrateSandboxConfig {
    fn name(&self) -> &'static str {
        "migrate_sandbox_config"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_sandbox_config(toml_src)
    }
}

pub(super) struct MigrateSandboxEgressFilter;
impl Migration for MigrateSandboxEgressFilter {
    fn name(&self) -> &'static str {
        "migrate_sandbox_egress_filter"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_sandbox_egress_filter(toml_src)
    }
}

pub(super) struct MigrateOrchestrationPersistence;
impl Migration for MigrateOrchestrationPersistence {
    fn name(&self) -> &'static str {
        "migrate_orchestration_persistence"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_orchestration_persistence(toml_src)
    }
}

pub(super) struct MigrateSessionRecapConfig;
impl Migration for MigrateSessionRecapConfig {
    fn name(&self) -> &'static str {
        "migrate_session_recap_config"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_session_recap_config(toml_src)
    }
}

pub(super) struct MigrateMcpElicitationConfig;
impl Migration for MigrateMcpElicitationConfig {
    fn name(&self) -> &'static str {
        "migrate_mcp_elicitation_config"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_mcp_elicitation_config(toml_src)
    }
}

pub(super) struct MigrateQualityConfig;
impl Migration for MigrateQualityConfig {
    fn name(&self) -> &'static str {
        "migrate_quality_config"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_quality_config(toml_src)
    }
}

pub(super) struct MigrateAcpSubagentsConfig;
impl Migration for MigrateAcpSubagentsConfig {
    fn name(&self) -> &'static str {
        "migrate_acp_subagents_config"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_acp_subagents_config(toml_src)
    }
}

pub(super) struct MigrateHooksPermissionDeniedConfig;
impl Migration for MigrateHooksPermissionDeniedConfig {
    fn name(&self) -> &'static str {
        "migrate_hooks_permission_denied_config"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_hooks_permission_denied_config(toml_src)
    }
}

pub(super) struct MigrateMemoryGraph;
impl Migration for MigrateMemoryGraph {
    fn name(&self) -> &'static str {
        "migrate_memory_graph_config"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_memory_graph_config(toml_src)
    }
}

pub(super) struct MigrateSchedulerDaemon;
impl Migration for MigrateSchedulerDaemon {
    fn name(&self) -> &'static str {
        "migrate_scheduler_daemon_config"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_scheduler_daemon_config(toml_src)
    }
}

pub(super) struct MigrateMemoryRetrieval;
impl Migration for MigrateMemoryRetrieval {
    fn name(&self) -> &'static str {
        "migrate_memory_retrieval_config"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_memory_retrieval_config(toml_src)
    }
}

pub(super) struct MigrateMemoryReasoning;
impl Migration for MigrateMemoryReasoning {
    fn name(&self) -> &'static str {
        "migrate_memory_reasoning_config"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_memory_reasoning_config(toml_src)
    }
}

pub(super) struct MigrateMemoryReasoningJudge;
impl Migration for MigrateMemoryReasoningJudge {
    fn name(&self) -> &'static str {
        "migrate_memory_reasoning_judge_config"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_memory_reasoning_judge_config(toml_src)
    }
}

pub(super) struct MigrateMemoryHebbian;
impl Migration for MigrateMemoryHebbian {
    fn name(&self) -> &'static str {
        "migrate_memory_hebbian_config"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_memory_hebbian_config(toml_src)
    }
}

pub(super) struct MigrateMemoryHebbianConsolidation;
impl Migration for MigrateMemoryHebbianConsolidation {
    fn name(&self) -> &'static str {
        "migrate_memory_hebbian_consolidation_config"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_memory_hebbian_consolidation_config(toml_src)
    }
}

pub(super) struct MigrateMemoryHebbianSpread;
impl Migration for MigrateMemoryHebbianSpread {
    fn name(&self) -> &'static str {
        "migrate_memory_hebbian_spread_config"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_memory_hebbian_spread_config(toml_src)
    }
}

pub(super) struct MigrateHooksTurnComplete;
impl Migration for MigrateHooksTurnComplete {
    fn name(&self) -> &'static str {
        "migrate_hooks_turn_complete_config"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_hooks_turn_complete_config(toml_src)
    }
}

pub(super) struct MigrateFocusAutoConsolidateMinWindow;
impl Migration for MigrateFocusAutoConsolidateMinWindow {
    fn name(&self) -> &'static str {
        "migrate_focus_auto_consolidate_min_window"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_focus_auto_consolidate_min_window(toml_src)
    }
}

pub(super) struct MigrateSessionProviderPersistence;
impl Migration for MigrateSessionProviderPersistence {
    fn name(&self) -> &'static str {
        "migrate_session_provider_persistence"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_session_provider_persistence(toml_src)
    }
}

pub(super) struct MigrateMemoryRetrievalQueryBias;
impl Migration for MigrateMemoryRetrievalQueryBias {
    fn name(&self) -> &'static str {
        "migrate_memory_retrieval_query_bias"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_memory_retrieval_query_bias(toml_src)
    }
}

pub(super) struct MigrateMemoryPersonaConfig;
impl Migration for MigrateMemoryPersonaConfig {
    fn name(&self) -> &'static str {
        "migrate_memory_persona_config"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_memory_persona_config(toml_src)
    }
}

pub(super) struct MigrateQdrantApiKey;
impl Migration for MigrateQdrantApiKey {
    fn name(&self) -> &'static str {
        "migrate_qdrant_api_key"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_qdrant_api_key(toml_src)
    }
}

pub(super) struct MigrateMcpMaxConnectAttempts;
impl Migration for MigrateMcpMaxConnectAttempts {
    fn name(&self) -> &'static str {
        "migrate_mcp_max_connect_attempts"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_mcp_max_connect_attempts(toml_src)
    }
}

pub(super) struct MigrateGoalsConfig;
impl Migration for MigrateGoalsConfig {
    fn name(&self) -> &'static str {
        "migrate_goals_config"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_goals_config(toml_src)
    }
}

pub(super) struct MigrateToolsCompressionConfig;
impl Migration for MigrateToolsCompressionConfig {
    fn name(&self) -> &'static str {
        "migrate_tools_compression_config"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_tools_compression_config(toml_src)
    }
}

pub(super) struct MigrateOrchestratorProvider;
impl Migration for MigrateOrchestratorProvider {
    fn name(&self) -> &'static str {
        "migrate_orchestrator_provider"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_orchestration_orchestrator_provider(toml_src)
    }
}

pub(super) struct MigrateProviderMaxConcurrent;
impl Migration for MigrateProviderMaxConcurrent {
    fn name(&self) -> &'static str {
        "migrate_provider_max_concurrent"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_provider_max_concurrent(toml_src)
    }
}

pub(super) struct MigrateGonkagateToGonka;
impl Migration for MigrateGonkagateToGonka {
    fn name(&self) -> &'static str {
        "migrate_gonkagate_to_gonka"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        Ok(super::migrate_gonkagate_to_gonka(toml_src))
    }
}

pub(super) struct MigrateCocoonProviderNotice;
impl Migration for MigrateCocoonProviderNotice {
    fn name(&self) -> &'static str {
        "migrate_cocoon_provider_notice"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_cocoon_provider_notice(toml_src)
    }
}

pub(super) struct MigrateTraceMetadata;
impl Migration for MigrateTraceMetadata {
    fn name(&self) -> &'static str {
        "migrate_trace_metadata"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_trace_metadata(toml_src)
    }
}

pub(super) struct MigrateFiveSignalConfig;
impl Migration for MigrateFiveSignalConfig {
    fn name(&self) -> &'static str {
        "migrate_five_signal_config"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_five_signal_config(toml_src)
    }
}

pub(super) struct MigrateEmbedProviderRename;
impl Migration for MigrateEmbedProviderRename {
    fn name(&self) -> &'static str {
        "migrate_embed_provider_rename"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_embed_provider_rename(toml_src)
    }
}

pub(super) struct MigrateMcpRetryAndToolTimeout;
impl Migration for MigrateMcpRetryAndToolTimeout {
    fn name(&self) -> &'static str {
        "migrate_mcp_retry_and_tool_timeout"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_mcp_retry_and_tool_timeout(toml_src)
    }
}

pub(super) struct MigrateFidelityTimeoutDefaults;
impl Migration for MigrateFidelityTimeoutDefaults {
    fn name(&self) -> &'static str {
        "migrate_fidelity_timeout_defaults"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_fidelity_timeout_defaults(toml_src)
    }
}

pub(super) struct MigrateSessionPersistProviderOverrides;
impl Migration for MigrateSessionPersistProviderOverrides {
    fn name(&self) -> &'static str {
        "migrate_session_persist_provider_overrides"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_session_persist_provider_overrides(toml_src)
    }
}

pub(super) struct MigrateCocoonShowBalance;
impl Migration for MigrateCocoonShowBalance {
    fn name(&self) -> &'static str {
        "migrate_cocoon_show_balance"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_cocoon_show_balance(toml_src)
    }
}

pub(super) struct MigrateWorktreeConfig;
impl Migration for MigrateWorktreeConfig {
    fn name(&self) -> &'static str {
        "migrate_worktree_config"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_worktree_config(toml_src)
    }
}

pub(super) struct MigrateWorktreeGitTimeout;
impl Migration for MigrateWorktreeGitTimeout {
    fn name(&self) -> &'static str {
        "migrate_worktree_git_timeout"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_worktree_git_timeout(toml_src)
    }
}

pub(super) struct MigrateLlmStreamLimits;
impl Migration for MigrateLlmStreamLimits {
    fn name(&self) -> &'static str {
        "migrate_llm_stream_limits"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_llm_stream_limits(toml_src)
    }
}

pub(super) struct MigrateDurableConfig;
impl Migration for MigrateDurableConfig {
    fn name(&self) -> &'static str {
        "migrate_durable_config"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_durable_config(toml_src)
    }
}

pub(super) struct MigrateEvalModelToProvider;
impl Migration for MigrateEvalModelToProvider {
    fn name(&self) -> &'static str {
        "migrate_eval_model_to_provider"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_eval_model_to_provider(toml_src)
    }
}

pub(super) struct MigrateCavemanConfig;
impl Migration for MigrateCavemanConfig {
    fn name(&self) -> &'static str {
        "migrate_caveman_config"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_caveman_config(toml_src)
    }
}

pub(super) struct MigrateShellCheckpointsConfig;
impl Migration for MigrateShellCheckpointsConfig {
    fn name(&self) -> &'static str {
        "migrate_shell_checkpoints_config"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_shell_checkpoints_config(toml_src)
    }
}

pub(super) struct MigrateKnowledgeConfig;
impl Migration for MigrateKnowledgeConfig {
    fn name(&self) -> &'static str {
        "migrate_knowledge_config"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_knowledge_config(toml_src)
    }
}

pub(super) struct MigrateDeepLinkConfig;
impl Migration for MigrateDeepLinkConfig {
    fn name(&self) -> &'static str {
        "migrate_deep_link_config"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_deep_link_config(toml_src)
    }
}

pub(super) struct MigrateMemoryGraphRecallIncludeImported;
impl Migration for MigrateMemoryGraphRecallIncludeImported {
    fn name(&self) -> &'static str {
        "migrate_memory_graph_recall_include_imported"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_memory_graph_recall_include_imported(toml_src)
    }
}

pub(super) struct MigratePolicyProviderAndUtilityWindow;
impl Migration for MigratePolicyProviderAndUtilityWindow {
    fn name(&self) -> &'static str {
        "migrate_policy_provider_and_utility_window"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_policy_provider_and_utility_window(toml_src)
    }
}

pub(super) struct MigrateTuiThemeConfig;
impl Migration for MigrateTuiThemeConfig {
    fn name(&self) -> &'static str {
        "migrate_tui_theme_config"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_tui_theme_config(toml_src)
    }
}

pub(super) struct MigrateTuiThemeDefaults;
impl Migration for MigrateTuiThemeDefaults {
    fn name(&self) -> &'static str {
        "migrate_tui_theme_defaults"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_tui_theme_defaults(toml_src)
    }
}

pub(super) struct MigrateTuiDelights;
impl Migration for MigrateTuiDelights {
    fn name(&self) -> &'static str {
        "migrate_tui_delights"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_tui_delights(toml_src)
    }
}

/// Step 68 — add `mouse = false` advisory comment under `[tui]` (#5103).
pub(super) struct MigrateTuiMouse;
impl Migration for MigrateTuiMouse {
    fn name(&self) -> &'static str {
        "migrate_tui_mouse"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_tui_mouse(toml_src)
    }
}

/// Step 69 — add `default_asset_sensitivity` advisory comment under `[orchestration]` (spec-068, #3934).
pub(super) struct MigrateOrchestrationAssetSensitivity;
impl Migration for MigrateOrchestrationAssetSensitivity {
    fn name(&self) -> &'static str {
        "migrate_orchestration_asset_sensitivity"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_orchestration_asset_sensitivity(toml_src)
    }
}

/// Step 78 — add a commented-out `[skills.registry]` advisory block (spec-045, #5869).
pub(super) struct MigrateSkillsRegistry;
impl Migration for MigrateSkillsRegistry {
    fn name(&self) -> &'static str {
        "migrate_skills_registry"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_skills_registry(toml_src)
    }
}

/// Step 70 — add session-persistence keys and a `[session.condense]` advisory block
/// (spec-068-session-persistence, #5343).
pub(super) struct MigrateSessionPersistenceConfig;
impl Migration for MigrateSessionPersistenceConfig {
    fn name(&self) -> &'static str {
        "migrate_session_persistence_config"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_session_persistence_config(toml_src)
    }
}

/// Step 71 — add a `[serve]` advisory block for `zeph serve` (spec-068 §9, #5343).
pub(super) struct MigrateServeConfig;
impl Migration for MigrateServeConfig {
    fn name(&self) -> &'static str {
        "migrate_serve_config"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_serve_config(toml_src)
    }
}

/// Step 72 — add a `[security.content_isolation.nli]` advisory block for the SONAR NLI
/// entailment check stage (#5438).
pub(super) struct MigrateNliConfig;
impl Migration for MigrateNliConfig {
    fn name(&self) -> &'static str {
        "migrate_nli_config"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_nli_config(toml_src)
    }
}

/// Step 73 — add a `[security.content_isolation.secret_masking]` advisory block for the PAAC
/// secret placeholder masking registry (#5437).
pub(super) struct MigrateSecretMaskingConfig;
impl Migration for MigrateSecretMaskingConfig {
    fn name(&self) -> &'static str {
        "migrate_secret_masking_config"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_secret_masking_config(toml_src)
    }
}

/// Step 74 — add a commented `filter_names = false` advisory to an existing active
/// `[security.pii_filter]` table (compensating name-detection heuristic for weak NER-model
/// recall, opt-in by design, #5530).
pub(super) struct MigratePiiFilterNames;
impl Migration for MigratePiiFilterNames {
    fn name(&self) -> &'static str {
        "migrate_pii_filter_names"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_pii_filter_names(toml_src)
    }
}

/// Step 75 — adds a commented `qdrant_timeout_secs = 10` advisory under `[memory]`.
pub(super) struct MigrateQdrantTimeoutSecs;
impl Migration for MigrateQdrantTimeoutSecs {
    fn name(&self) -> &'static str {
        "migrate_qdrant_timeout_secs"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_qdrant_timeout_secs(toml_src)
    }
}

/// Step 76 — adds a commented `high_gain_tools = []` advisory under `[tools.utility]` (#5659).
pub(super) struct MigrateUtilityHighGainTools;
impl Migration for MigrateUtilityHighGainTools {
    fn name(&self) -> &'static str {
        "migrate_utility_high_gain_tools"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_utility_high_gain_tools(toml_src)
    }
}

/// Step 77 — adds a commented `[[acp.auth_clients]]` advisory block (#5868).
pub(super) struct MigrateAcpAuthClientsConfig;
impl Migration for MigrateAcpAuthClientsConfig {
    fn name(&self) -> &'static str {
        "migrate_acp_auth_clients_config"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_acp_auth_clients_config(toml_src)
    }
}

/// Step 79 — adds a commented `shared_db = false` advisory to an existing active `[durable]`
/// table (INV-8 `encryption_gate`, #5996).
pub(super) struct MigrateDurableSharedDb;
impl Migration for MigrateDurableSharedDb {
    fn name(&self) -> &'static str {
        "migrate_durable_shared_db"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_durable_shared_db(toml_src)
    }
}

/// Step 80 — adds a commented `require_integrity_check_on_promote = true` advisory to an
/// existing active `[skills.trust]` table (#6087).
pub(super) struct MigrateSkillTrustRequireCheck;
impl Migration for MigrateSkillTrustRequireCheck {
    fn name(&self) -> &'static str {
        "migrate_skill_trust_require_check"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_skill_trust_require_check(toml_src)
    }
}

/// Step 81 — adds a commented `[security.shadow_sentinel]` advisory block (spec 050, #5934).
pub(super) struct MigrateShadowSentinelConfig;
impl Migration for MigrateShadowSentinelConfig {
    fn name(&self) -> &'static str {
        "migrate_shadow_sentinel_config"
    }

    fn apply(&self, toml_src: &str) -> Result<MigrationResult, MigrateError> {
        migrate_shadow_sentinel_config(toml_src)
    }
}
