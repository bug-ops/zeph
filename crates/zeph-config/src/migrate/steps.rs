// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Migration step wrapper structs — one per sequential migration in [`super::MIGRATIONS`].
//!
//! Steps 1–35 are pre-existing; steps 36–38 were added for the stable-defaults flip;
//! step 39 adds optional Qdrant API key; step 40 adds MCP `max_connect_attempts`;
//! steps 41–45 add goal lifecycle, TACO compression, orchestrator provider, per-provider
//! admission control, and `GonkaGate` advisory notice; step 46 is an advisory notice for Cocoon.
//!
//! Each struct is a zero-size type that delegates to the corresponding free function in
//! `super`. They exist solely to satisfy the object-safe [`super::Migration`] trait so the
//! registry can hold `Box<dyn Migration>` values.

use super::{
    MigrateError, Migration, MigrationResult, migrate_acp_subagents_config,
    migrate_agent_budget_hint, migrate_agent_retry_to_tools_retry, migrate_autodream_config,
    migrate_cocoon_provider_notice, migrate_compression_predictor_config, migrate_database_url,
    migrate_egress_config, migrate_focus_auto_consolidate_min_window, migrate_forgetting_config,
    migrate_goals_config, migrate_hooks_permission_denied_config,
    migrate_hooks_turn_complete_config, migrate_magic_docs_config, migrate_mcp_elicitation_config,
    migrate_mcp_max_connect_attempts, migrate_mcp_trust_levels, migrate_memory_graph_config,
    migrate_memory_hebbian_config, migrate_memory_hebbian_consolidation_config,
    migrate_memory_hebbian_spread_config, migrate_memory_persona_config,
    migrate_memory_reasoning_config, migrate_memory_reasoning_judge_config,
    migrate_memory_retrieval_config, migrate_memory_retrieval_query_bias,
    migrate_microcompact_config, migrate_orchestration_orchestrator_provider,
    migrate_orchestration_persistence, migrate_otel_filter, migrate_planner_model_to_provider,
    migrate_provider_max_concurrent, migrate_qdrant_api_key, migrate_quality_config,
    migrate_sandbox_config, migrate_sandbox_egress_filter, migrate_scheduler_daemon_config,
    migrate_session_provider_persistence, migrate_session_recap_config,
    migrate_shell_transactional, migrate_stt_to_provider, migrate_supervisor_config,
    migrate_telemetry_config, migrate_tools_compression_config, migrate_vigil_config,
};

// ── Wrapper structs for all 35 sequential migration steps ───────────────────────────────────────

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
