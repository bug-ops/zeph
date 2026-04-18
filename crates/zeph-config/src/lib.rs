// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Configuration types and loaders for Zeph.
//!
//! This crate contains configuration struct and enum definitions, the TOML loader,
//! environment variable overrides, validation, and migration helpers.
//! Vault secret resolution is handled in `zeph-core` through the `SecretResolver` trait.
//!
//! # Loading configuration
//!
//! ```no_run
//! use std::path::Path;
//! use zeph_config::Config;
//!
//! // Load from file (falls back to defaults when the file does not exist)
//! let config = Config::load(Path::new("/etc/zeph/config.toml"))
//!     .expect("failed to load config");
//!
//! // Validate numeric bounds and cross-references
//! config.validate().expect("config validation failed");
//!
//! println!("Agent name: {}", config.agent.name);
//! println!("History limit: {}", config.memory.history_limit);
//! ```
//!
//! # Environment variable overrides
//!
//! After loading from TOML, `Config::load` automatically applies env-var overrides.
//! Key variables:
//!
//! | Variable | Field overridden |
//! |---|---|
//! | `ZEPH_LLM_PROVIDER` | `llm.providers[0].provider_type` |
//! | `ZEPH_LLM_MODEL` | `llm.providers[0].model` |
//! | `ZEPH_SQLITE_PATH` | `memory.sqlite_path` |
//! | `ZEPH_QDRANT_URL` | `memory.qdrant_url` |
//!
//! # Config migration
//!
//! Use [`migrate::ConfigMigrator`] to upgrade existing TOML configs with newly-added
//! parameters added as commented-out entries:
//!
//! ```no_run
//! use zeph_config::migrate::ConfigMigrator;
//!
//! let user_toml = std::fs::read_to_string("config.toml").unwrap();
//! let migrator = ConfigMigrator::new();
//! let result = migrator.migrate(&user_toml).expect("migration failed");
//! println!("Added {} new parameters", result.added_count);
//! std::fs::write("config.toml", &result.output).unwrap();
//! ```

pub mod agent;
pub mod channels;
pub mod classifiers;
pub mod defaults;
pub mod dump_format;
mod env;
pub mod error;
pub mod experiment;
pub mod features;
pub mod hooks;
pub mod learning;
mod loader;
pub mod logging;
pub mod memory;
pub mod metrics;
pub mod migrate;
pub mod providers;
pub mod rate_limit;
pub mod root;
pub mod sanitizer;
pub mod security;
pub mod session;
pub mod subagent;
pub mod telemetry;
pub mod ui;
pub mod vigil;

pub use agent::{
    AgentConfig, ContextInjectionMode, FocusConfig, ModelSpec, SubAgentConfig,
    SubAgentLifecycleHooks, TaskSupervisorConfig, ToolFilterConfig,
};
pub use channels::{
    A2aServerConfig, ChannelSkillsConfig, DiscordConfig, IbctKeyConfig, McpConfig, McpOAuthConfig,
    McpServerConfig, McpTrustLevel, OAuthTokenStorage, SlackConfig, TelegramConfig,
    ToolDiscoveryConfig, ToolDiscoveryStrategyConfig, ToolPruningConfig, TrustCalibrationConfig,
    is_skill_allowed,
};
pub use defaults::{
    DEFAULT_DEBUG_DIR, DEFAULT_LOG_FILE, DEFAULT_SKILLS_DIR, DEFAULT_SQLITE_PATH,
    default_debug_dir, default_integrity_registry_path, default_log_file_path, default_skills_dir,
    default_sqlite_path, is_legacy_default_debug_dir, is_legacy_default_log_file,
    is_legacy_default_skills_path, is_legacy_default_sqlite_path,
};
pub use dump_format::DumpFormat;
pub use experiment::{
    AdaptOrchConfig, ExperimentConfig, ExperimentSchedule, OrchestrationConfig, PlanCacheConfig,
};
pub use features::{
    CostConfig, DaemonConfig, DebugConfig, GatewayConfig, IndexConfig, ScheduledTaskConfig,
    ScheduledTaskKind, SchedulerConfig, SkillMiningConfig, SkillPromptMode, SkillsConfig,
    TraceConfig, VaultConfig,
};
pub use hooks::{FileChangedConfig, HooksConfig};
pub use learning::{DetectorMode, LearningConfig};
pub use logging::{LogRotation, LoggingConfig};
pub use memory::{
    AdmissionConfig, AdmissionStrategy, AdmissionWeights, AutoDreamConfig, BeliefRevisionConfig,
    CategoryConfig, CompressionConfig, CompressionPredictorConfig, CompressionStrategy,
    ContextStrategy, DigestConfig, DocumentConfig, ForgettingConfig, GraphConfig, MagicDocsConfig,
    MemoryConfig, MicrocompactConfig, NoteLinkingConfig, PersonaConfig, PruningStrategy, RpeConfig,
    SemanticConfig, SessionsConfig, SidequestConfig, StoreRoutingConfig, StoreRoutingStrategy,
    TierConfig, TrajectoryConfig, TreeConfig, VectorBackend,
};
pub use metrics::MetricsConfig;
pub use providers::{
    BanditConfig, CandleConfig, CandleInlineConfig, CascadeClassifierMode, CascadeConfig,
    CoeConfig, ComplexityRoutingConfig, GenerationParams, LlmConfig, LlmRoutingStrategy,
    MAX_TOKENS_CAP, ProviderEntry, ProviderKind, ProviderName, RouterConfig, RouterStrategyConfig,
    SttConfig, TierMapping, validate_pool,
};
pub use providers::{default_stt_language, default_stt_provider};
pub use rate_limit::RateLimitConfig;
pub use sanitizer::{
    CausalIpiConfig, ContentIsolationConfig, CustomPiiPattern, EmbeddingGuardConfig,
    ExfiltrationGuardConfig, MemoryWriteValidationConfig, PiiFilterConfig, QuarantineConfig,
    ResponseVerificationConfig,
};
pub use sanitizer::{GuardrailAction, GuardrailConfig, GuardrailFailStrategy};
pub use security::{ScannerConfig, SecurityConfig, TimeoutConfig, TrustConfig};
pub use session::{RecapConfig, SessionConfig};
pub use subagent::{
    HookDef, HookMatcher, HookType, MemoryScope, PermissionMode, SkillFilter, SubagentHooks,
    ToolPolicy,
};
pub use telemetry::{TelemetryBackend, TelemetryConfig};
pub use ui::{AcpConfig, AcpLspConfig, AcpTransport, TuiConfig};
pub use ui::{DiagnosticSeverity, DiagnosticsConfig, HoverConfig, LspConfig};
pub use vigil::VigilConfig;

// Top-level config struct, error type, and resolved secrets — moved from zeph-core.
pub use classifiers::{ClassifiersConfig, InjectionEnforcementMode};
pub use error::ConfigError;
pub use root::{Config, ResolvedSecrets};
