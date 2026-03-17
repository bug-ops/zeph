// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

mod agent;
mod channels;
mod defaults;
mod experiment;
mod features;
mod learning;
mod logging;
mod memory;
mod providers;
mod security;
mod ui;

#[cfg(test)]
mod tests;

pub use agent::{AgentConfig, FocusConfig, SubAgentConfig, SubAgentLifecycleHooks};
pub use channels::{
    A2aServerConfig, DiscordConfig, McpConfig, McpOAuthConfig, McpServerConfig, OAuthTokenStorage,
    SlackConfig, TelegramConfig,
};
pub use defaults::{
    DEFAULT_DEBUG_DIR, DEFAULT_LOG_FILE, DEFAULT_SKILLS_DIR, DEFAULT_SQLITE_PATH,
    default_debug_dir, default_log_file_path, default_skills_dir, default_sqlite_path,
    is_legacy_default_debug_dir, is_legacy_default_log_file, is_legacy_default_skills_path,
    is_legacy_default_sqlite_path,
};
pub use experiment::{ExperimentConfig, ExperimentSchedule, OrchestrationConfig};
pub use features::{
    CostConfig, DaemonConfig, DebugConfig, GatewayConfig, IndexConfig, ObservabilityConfig,
    ScheduledTaskConfig, ScheduledTaskKind, SchedulerConfig, SkillPromptMode, SkillsConfig,
    TraceConfig, VaultConfig,
};
pub use learning::{DetectorMode, LearningConfig};
pub use logging::{LogRotation, LoggingConfig};
pub use memory::{
    CompressionConfig, CompressionStrategy, DocumentConfig, GraphConfig, MemoryConfig,
    NoteLinkingConfig, PruningStrategy, RoutingConfig, RoutingStrategy, SemanticConfig,
    SessionsConfig, SidequestConfig, VectorBackend,
};
pub use providers::{
    CandleConfig, CascadeClassifierMode, CascadeConfig, CloudLlmConfig, CompatibleConfig,
    GeminiConfig, GenerationParams, LlmConfig, MAX_TOKENS_CAP, OllamaConfig, OpenAiConfig,
    OrchestratorConfig, OrchestratorProviderConfig, ProviderKind, RouterConfig,
    RouterStrategyConfig, SttConfig,
};
pub use providers::{default_stt_language, default_stt_model, default_stt_provider};
pub use security::{SecurityConfig, TimeoutConfig, TrustConfig};
pub use ui::{AcpConfig, AcpLspConfig, AcpTransport, TuiConfig};

#[cfg(feature = "lsp-context")]
pub use ui::{DiagnosticSeverity, DiagnosticsConfig, HoverConfig, LspConfig};

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use zeph_tools::ToolsConfig;

use crate::vault::Secret;

use defaults::{default_skill_paths, default_sqlite_path_field};

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
    pub observability: ObservabilityConfig,
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
    pub experiments: ExperimentConfig,
    #[serde(default)]
    pub debug: DebugConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
    #[cfg(feature = "lsp-context")]
    #[serde(default)]
    pub lsp: LspConfig,
    #[serde(skip)]
    pub secrets: ResolvedSecrets,
}

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
        use providers::{
            get_default_embedding_model, get_default_response_cache_ttl_secs,
            get_default_router_ema_alpha, get_default_router_reorder_interval,
        };
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
            },
            llm: LlmConfig {
                provider: ProviderKind::Ollama,
                base_url: "http://localhost:11434".into(),
                model: "qwen3:8b".into(),
                embedding_model: get_default_embedding_model(),
                cloud: None,
                ollama: None,
                openai: None,
                gemini: None,
                candle: None,
                orchestrator: None,
                compatible: None,
                router: None,
                stt: None,
                vision_model: None,
                response_cache_enabled: false,
                response_cache_ttl_secs: get_default_response_cache_ttl_secs(),
                router_ema_enabled: false,
                router_ema_alpha: get_default_router_ema_alpha(),
                router_reorder_interval: get_default_router_reorder_interval(),
                instruction_file: None,
                summary_model: None,
                summary_provider: None,
            },
            skills: SkillsConfig {
                paths: default_skill_paths(),
                max_active_skills: 5,
                disambiguation_threshold: 0.05,
                cosine_weight: 0.7,
                hybrid_search: true,
                learning: LearningConfig::default(),
                trust: TrustConfig::default(),
                prompt_mode: SkillPromptMode::Auto,
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
                autosave_assistant: false,
                autosave_min_length: 20,
                tool_call_cutoff: 6,
                sqlite_pool_size: 5,
                sessions: SessionsConfig::default(),
                documents: DocumentConfig::default(),
                eviction: zeph_memory::EvictionConfig::default(),
                compression: CompressionConfig::default(),
                sidequest: SidequestConfig::default(),
                routing: RoutingConfig::default(),
                graph: GraphConfig::default(),
                compression_guidelines: zeph_memory::CompressionGuidelinesConfig::default(),
                shutdown_summary: true,
                shutdown_summary_min_messages: 4,
                shutdown_summary_max_messages: 20,
                shutdown_summary_timeout_secs: 10,
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
            observability: ObservabilityConfig::default(),
            gateway: GatewayConfig::default(),
            daemon: DaemonConfig::default(),
            scheduler: SchedulerConfig::default(),
            tui: TuiConfig::default(),
            acp: AcpConfig::default(),
            agents: SubAgentConfig::default(),
            orchestration: OrchestrationConfig::default(),
            experiments: ExperimentConfig::default(),
            debug: DebugConfig::default(),
            logging: LoggingConfig::default(),
            #[cfg(feature = "lsp-context")]
            lsp: LspConfig::default(),
            secrets: ResolvedSecrets::default(),
        }
    }
}
