// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Configuration types and loaders for Zeph.
//!
//! This crate contains configuration struct and enum definitions, the TOML loader,
//! environment variable overrides, validation, and migration helpers.
//! Vault secret resolution is handled in `zeph-core` through the `SecretResolver` trait.

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
pub mod migrate;
pub mod providers;
pub mod rate_limit;
pub mod root;
pub mod sanitizer;
pub mod security;
pub mod subagent;
pub mod ui;

pub use agent::{
    AgentConfig, FocusConfig, SubAgentConfig, SubAgentLifecycleHooks, ToolFilterConfig,
};
pub use channels::{
    A2aServerConfig, DiscordConfig, IbctKeyConfig, McpConfig, McpOAuthConfig, McpServerConfig,
    McpTrustLevel, OAuthTokenStorage, SlackConfig, TelegramConfig, ToolDiscoveryConfig,
    ToolDiscoveryStrategyConfig, ToolPruningConfig, TrustCalibrationConfig,
};
pub use defaults::{
    DEFAULT_DEBUG_DIR, DEFAULT_LOG_FILE, DEFAULT_SKILLS_DIR, DEFAULT_SQLITE_PATH,
    default_debug_dir, default_log_file_path, default_skills_dir, default_sqlite_path,
    is_legacy_default_debug_dir, is_legacy_default_log_file, is_legacy_default_skills_path,
    is_legacy_default_sqlite_path,
};
pub use dump_format::DumpFormat;
pub use experiment::{ExperimentConfig, ExperimentSchedule, OrchestrationConfig, PlanCacheConfig};
pub use features::{
    CostConfig, DaemonConfig, DebugConfig, GatewayConfig, IndexConfig, ObservabilityConfig,
    ScheduledTaskConfig, ScheduledTaskKind, SchedulerConfig, SkillPromptMode, SkillsConfig,
    TraceConfig, VaultConfig,
};
pub use hooks::{FileChangedConfig, HooksConfig};
pub use learning::{DetectorMode, LearningConfig};
pub use logging::{LogRotation, LoggingConfig};
pub use memory::{
    AdmissionConfig, AdmissionStrategy, AdmissionWeights, BeliefRevisionConfig, CompressionConfig,
    CompressionStrategy, ContextStrategy, DigestConfig, DocumentConfig, GraphConfig, MemoryConfig,
    NoteLinkingConfig, PruningStrategy, RpeConfig, SemanticConfig, SessionsConfig, SidequestConfig,
    StoreRoutingConfig, StoreRoutingStrategy, TierConfig, VectorBackend,
};
pub use providers::{
    BanditConfig, CandleConfig, CandleInlineConfig, CascadeClassifierMode, CascadeConfig,
    ComplexityRoutingConfig, GenerationParams, LlmConfig, LlmRoutingStrategy, MAX_TOKENS_CAP,
    ProviderEntry, ProviderKind, ProviderName, RouterConfig, RouterStrategyConfig, SttConfig,
    TierMapping, validate_pool,
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
pub use subagent::{
    HookDef, HookMatcher, HookType, MemoryScope, PermissionMode, SkillFilter, SubagentHooks,
    ToolPolicy,
};
pub use ui::{AcpConfig, AcpLspConfig, AcpTransport, TuiConfig};
pub use ui::{DiagnosticSeverity, DiagnosticsConfig, HoverConfig, LspConfig};

// Top-level config struct, error type, and resolved secrets — moved from zeph-core.
pub use classifiers::{ClassifiersConfig, InjectionEnforcementMode};
pub use error::ConfigError;
pub use root::{Config, ResolvedSecrets};
