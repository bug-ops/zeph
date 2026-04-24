// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use zeph_common::secret::Secret;
use zeph_tools::ToolsConfig;

use crate::agent::{AgentConfig, FocusConfig, SubAgentConfig};
use crate::channels::{A2aServerConfig, DiscordConfig, McpConfig, SlackConfig, TelegramConfig};
use crate::classifiers::ClassifiersConfig;
use crate::cli::CliConfig;
use crate::defaults::{default_skill_paths, default_sqlite_path_field};
use crate::experiment::{ExperimentConfig, OrchestrationConfig};
use crate::features::{
    CostConfig, DaemonConfig, DebugConfig, GatewayConfig, IndexConfig, SchedulerConfig,
    SkillPromptMode, SkillsConfig, VaultConfig,
};
use crate::hooks::HooksConfig;
use crate::learning::LearningConfig;
use crate::logging::LoggingConfig;
use crate::memory::{
    CompressionConfig, DocumentConfig, GraphConfig, MagicDocsConfig, MemoryConfig, SemanticConfig,
    SessionsConfig, SidequestConfig, TierConfig, VectorBackend,
};
use crate::metrics::MetricsConfig;
use crate::providers::{
    LlmConfig, get_default_embedding_model, get_default_response_cache_ttl_secs,
    get_default_router_ema_alpha, get_default_router_reorder_interval,
};
use crate::security::TrustConfig;
use crate::security::{SecurityConfig, TimeoutConfig};
use crate::telemetry::TelemetryConfig;
use crate::ui::LspConfig;
use crate::ui::{AcpConfig, TuiConfig};

/// Top-level agent configuration.
///
/// Loaded from a TOML file via [`Config::load`]. Env-var overrides can be applied
/// via `apply_env_overrides`. Secret resolution via `VaultProvider`
/// is handled in `zeph-core` through the `SecretResolver` trait.
#[derive(Debug, Deserialize, Serialize)]
pub struct Config {
    pub agent: AgentConfig,
    pub llm: LlmConfig,
    pub skills: SkillsConfig,
    pub memory: MemoryConfig,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub telegram: Option<TelegramConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub discord: Option<DiscordConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slack: Option<SlackConfig>,
    #[serde(default)]
    pub tools: ToolsConfig,
    #[serde(default)]
    pub a2a: A2aServerConfig,
    #[serde(default)]
    pub mcp: McpConfig,
    #[serde(default)]
    pub index: IndexConfig,
    #[serde(default)]
    pub vault: VaultConfig,
    #[serde(default)]
    pub security: SecurityConfig,
    #[serde(default)]
    pub timeouts: TimeoutConfig,
    #[serde(default)]
    pub cost: CostConfig,
    #[serde(default)]
    pub gateway: GatewayConfig,
    #[serde(default)]
    pub daemon: DaemonConfig,
    #[serde(default)]
    pub scheduler: SchedulerConfig,
    #[serde(default)]
    pub tui: TuiConfig,
    #[serde(default)]
    pub acp: AcpConfig,
    #[serde(default)]
    pub agents: SubAgentConfig,
    #[serde(default)]
    pub orchestration: OrchestrationConfig,
    #[serde(default)]
    pub classifiers: ClassifiersConfig,
    #[serde(default)]
    pub experiments: ExperimentConfig,
    #[serde(default)]
    pub debug: DebugConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
    #[serde(default)]
    pub hooks: HooksConfig,
    #[serde(default)]
    pub lsp: LspConfig,
    /// `MagicDocs` auto-maintained markdown (#2702).
    #[serde(default)]
    pub magic_docs: MagicDocsConfig,
    /// Profiling and distributed tracing configuration.
    #[serde(default)]
    pub telemetry: TelemetryConfig,
    /// Prometheus metrics export configuration.
    #[serde(default)]
    pub metrics: MetricsConfig,
    /// Session UX settings (recap-on-resume, etc.).
    #[serde(default)]
    pub session: crate::session::SessionConfig,
    /// Session-scoped CLI overrides (bare mode, JSON output, auto-approve).
    #[serde(default)]
    pub cli: CliConfig,
    /// MARCH self-check quality pipeline configuration.
    #[serde(default)]
    pub quality: crate::quality::QualityConfig,
    /// Resolved secrets from vault. Never serialized — populated at runtime.
    #[serde(skip)]
    pub secrets: ResolvedSecrets,
}

/// Secrets resolved from the vault at runtime.
///
/// Populated by `SecretResolver::resolve_secrets()` in `zeph-core`.
/// Never serialized to TOML.
#[derive(Debug, Default)]
pub struct ResolvedSecrets {
    pub claude_api_key: Option<Secret>,
    pub openai_api_key: Option<Secret>,
    pub gemini_api_key: Option<Secret>,
    pub compatible_api_keys: HashMap<String, Secret>,
    pub discord_token: Option<Secret>,
    pub slack_bot_token: Option<Secret>,
    pub slack_signing_secret: Option<Secret>,
    /// Arbitrary skill secrets resolved from `ZEPH_SECRET_*` vault keys.
    /// Key is the lowercased name after stripping the prefix (e.g. `github_token`).
    pub custom: HashMap<String, Secret>,
}

impl Default for Config {
    #[allow(clippy::too_many_lines)] // flat struct literal with one field per config section — no meaningful split exists
    fn default() -> Self {
        Self {
            agent: AgentConfig {
                name: "Zeph".into(),
                max_tool_iterations: 10,
                auto_update_check: true,
                instruction_files: Vec::new(),
                instruction_auto_detect: true,
                max_tool_retries: 2,
                tool_repeat_threshold: 2,
                max_retry_duration_secs: 30,
                focus: FocusConfig::default(),
                tool_filter: crate::agent::ToolFilterConfig::default(),
                budget_hint_enabled: true,
                supervisor: crate::agent::TaskSupervisorConfig::default(),
            },
            llm: LlmConfig {
                providers: Vec::new(),
                routing: crate::providers::LlmRoutingStrategy::None,
                embedding_model: get_default_embedding_model(),
                candle: None,
                stt: None,
                response_cache_enabled: false,
                response_cache_ttl_secs: get_default_response_cache_ttl_secs(),
                semantic_cache_enabled: false,
                semantic_cache_threshold: 0.95,
                semantic_cache_max_candidates: 10,
                router_ema_enabled: false,
                router_ema_alpha: get_default_router_ema_alpha(),
                router_reorder_interval: get_default_router_reorder_interval(),
                router: None,
                instruction_file: None,
                summary_model: None,
                summary_provider: None,
                complexity_routing: None,
                coe: None,
            },
            skills: SkillsConfig {
                paths: default_skill_paths(),
                max_active_skills: 5,
                disambiguation_threshold: 0.20,
                min_injection_score: 0.20,
                cosine_weight: 0.7,
                hybrid_search: true,
                learning: LearningConfig::default(),
                trust: TrustConfig::default(),
                prompt_mode: SkillPromptMode::Auto,
                two_stage_matching: false,
                confusability_threshold: 0.0,
                rl_routing_enabled: false,
                rl_learning_rate: 0.01,
                rl_weight: 0.3,
                rl_persist_interval: 10,
                rl_warmup_updates: 50,
                rl_embed_dim: None,
                generation_provider: crate::providers::ProviderName::default(),
                generation_output_dir: None,
                mining: crate::features::SkillMiningConfig::default(),
                evaluation: crate::features::SkillEvaluationConfig::default(),
                proactive_exploration: crate::features::ProactiveExplorationConfig::default(),
            },
            memory: MemoryConfig {
                sqlite_path: default_sqlite_path_field(),
                history_limit: 50,
                qdrant_url: "http://localhost:6334".into(),
                semantic: SemanticConfig::default(),
                summarization_threshold: 50,
                context_budget_tokens: 0,
                soft_compaction_threshold: 0.60,
                hard_compaction_threshold: 0.90,
                compaction_preserve_tail: 6,
                compaction_cooldown_turns: 2,
                auto_budget: true,
                prune_protect_tokens: 40_000,
                cross_session_score_threshold: 0.35,
                vector_backend: VectorBackend::default(),
                token_safety_margin: 1.0,
                redact_credentials: true,
                autosave_assistant: true,
                autosave_min_length: 20,
                tool_call_cutoff: 6,
                sqlite_pool_size: 5,
                sessions: SessionsConfig::default(),
                documents: DocumentConfig::default(),
                eviction: zeph_memory::EvictionConfig::default(),
                compression: CompressionConfig::default(),
                sidequest: SidequestConfig::default(),
                graph: GraphConfig::default(),
                compression_guidelines: zeph_memory::CompressionGuidelinesConfig::default(),
                shutdown_summary: true,
                shutdown_summary_min_messages: 4,
                shutdown_summary_max_messages: 20,
                shutdown_summary_timeout_secs: 10,
                structured_summaries: false,
                tiers: TierConfig::default(),
                admission: crate::memory::AdmissionConfig::default(),
                digest: crate::memory::DigestConfig::default(),
                context_strategy: crate::memory::ContextStrategy::default(),
                crossover_turn_threshold: 20,
                consolidation: crate::memory::ConsolidationConfig::default(),
                forgetting: crate::memory::ForgettingConfig::default(),
                database_url: None,
                store_routing: crate::memory::StoreRoutingConfig::default(),
                persona: crate::memory::PersonaConfig::default(),
                trajectory: crate::memory::TrajectoryConfig::default(),
                category: crate::memory::CategoryConfig::default(),
                tree: crate::memory::TreeConfig::default(),
                microcompact: crate::memory::MicrocompactConfig::default(),
                autodream: crate::memory::AutoDreamConfig::default(),
                key_facts_dedup_threshold: 0.95,
                compression_spectrum: crate::features::CompressionSpectrumConfig::default(),
                retrieval: crate::memory::RetrievalConfig::default(),
            },
            telegram: None,
            discord: None,
            slack: None,
            tools: ToolsConfig::default(),
            a2a: A2aServerConfig::default(),
            mcp: McpConfig::default(),
            index: IndexConfig::default(),
            vault: VaultConfig::default(),
            security: SecurityConfig::default(),
            timeouts: TimeoutConfig::default(),
            cost: CostConfig::default(),
            gateway: GatewayConfig::default(),
            daemon: DaemonConfig::default(),
            scheduler: SchedulerConfig::default(),
            tui: TuiConfig::default(),
            acp: AcpConfig::default(),
            agents: SubAgentConfig::default(),
            orchestration: OrchestrationConfig::default(),
            classifiers: ClassifiersConfig::default(),
            experiments: ExperimentConfig::default(),
            debug: DebugConfig::default(),
            logging: LoggingConfig::default(),
            lsp: LspConfig::default(),
            hooks: HooksConfig::default(),
            magic_docs: MagicDocsConfig::default(),
            telemetry: TelemetryConfig::default(),
            metrics: MetricsConfig::default(),
            session: crate::session::SessionConfig::default(),
            cli: CliConfig::default(),
            quality: crate::quality::QualityConfig::default(),
            secrets: ResolvedSecrets::default(),
        }
    }
}
