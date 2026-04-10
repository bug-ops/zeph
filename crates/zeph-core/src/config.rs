// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Extension trait for resolving vault secrets into a Config.
//!
//! This trait is defined in zeph-core (not in zeph-config) due to Rust's orphan rule:
//! implementing a foreign trait on a foreign type requires the trait to be defined locally.

// Re-export Config types from zeph-config for internal use.
pub use zeph_config::{
    AcpConfig, AcpLspConfig, AcpTransport, AgentConfig, CandleConfig, CandleInlineConfig,
    CascadeClassifierMode, CascadeConfig, ClassifiersConfig, CompressionConfig,
    CompressionStrategy, Config, ConfigError, CostConfig, DaemonConfig, DebugConfig, DetectorMode,
    DiscordConfig, DocumentConfig, DumpFormat, ExperimentConfig, ExperimentSchedule, FocusConfig,
    GatewayConfig, GenerationParams, GraphConfig, HookDef, HookMatcher, HookType, IndexConfig,
    LearningConfig, LlmConfig, LlmRoutingStrategy, LogRotation, LoggingConfig, MAX_TOKENS_CAP,
    McpConfig, McpOAuthConfig, McpServerConfig, McpTrustLevel, MemoryConfig, MemoryScope,
    NoteLinkingConfig, OAuthTokenStorage, ObservabilityConfig, OrchestrationConfig, PermissionMode,
    ProviderEntry, ProviderKind, ProviderName, PruningStrategy, RateLimitConfig, ResolvedSecrets,
    RouterConfig, RouterStrategyConfig, ScheduledTaskConfig, ScheduledTaskKind, SchedulerConfig,
    SecurityConfig, SemanticConfig, SessionsConfig, SidequestConfig, SkillFilter, SkillPromptMode,
    SkillsConfig, SlackConfig, StoreRoutingConfig, StoreRoutingStrategy, SttConfig, SubAgentConfig,
    SubAgentLifecycleHooks, SubagentHooks, TelegramConfig, TimeoutConfig, ToolDiscoveryConfig,
    ToolDiscoveryStrategyConfig, ToolFilterConfig, ToolPolicy, TraceConfig, TrustConfig, TuiConfig,
    VaultConfig, VectorBackend,
};

pub use zeph_config::{
    AutoDreamConfig, CategoryConfig, ContextStrategy, DigestConfig, MagicDocsConfig,
    MicrocompactConfig, PersonaConfig, TrajectoryConfig, TreeConfig,
};
pub use zeph_config::{DiagnosticSeverity, DiagnosticsConfig, HoverConfig, LspConfig};
pub use zeph_config::{TelemetryBackend, TelemetryConfig};

pub use zeph_config::{
    ContentIsolationConfig, CustomPiiPattern, ExfiltrationGuardConfig, MemoryWriteValidationConfig,
    PiiFilterConfig, QuarantineConfig,
};
pub use zeph_config::{GuardrailAction, GuardrailConfig, GuardrailFailStrategy};

pub use zeph_config::A2aServerConfig;
pub use zeph_config::ChannelSkillsConfig;
pub use zeph_config::{FileChangedConfig, HooksConfig};

pub use zeph_config::{
    DEFAULT_DEBUG_DIR, DEFAULT_LOG_FILE, DEFAULT_SKILLS_DIR, DEFAULT_SQLITE_PATH,
    default_debug_dir, default_log_file_path, default_skills_dir, default_sqlite_path,
    is_legacy_default_debug_dir, is_legacy_default_log_file, is_legacy_default_skills_path,
    is_legacy_default_sqlite_path,
};

pub use zeph_config::providers::{default_stt_language, default_stt_provider, validate_pool};

pub mod migrate {
    pub use zeph_config::migrate::*;
}

use crate::vault::{Secret, VaultProvider};

/// Extension trait for resolving vault secrets into a [`Config`].
///
/// Implemented for [`Config`] in `zeph-core` because `VaultProvider` lives here.
/// Call with `use zeph_core::config::SecretResolver` in scope.
pub trait SecretResolver {
    /// Populate `secrets` fields from the vault.
    ///
    /// # Errors
    ///
    /// Returns an error if the vault backend fails.
    fn resolve_secrets(
        &mut self,
        vault: &dyn VaultProvider,
    ) -> impl std::future::Future<Output = Result<(), ConfigError>> + Send;
}

impl SecretResolver for Config {
    async fn resolve_secrets(&mut self, vault: &dyn VaultProvider) -> Result<(), ConfigError> {
        if let Some(val) = vault.get_secret("ZEPH_CLAUDE_API_KEY").await? {
            self.secrets.claude_api_key = Some(Secret::new(val));
        }
        if let Some(val) = vault.get_secret("ZEPH_OPENAI_API_KEY").await? {
            self.secrets.openai_api_key = Some(Secret::new(val));
        }
        if let Some(val) = vault.get_secret("ZEPH_GEMINI_API_KEY").await? {
            self.secrets.gemini_api_key = Some(Secret::new(val));
        }
        if let Some(val) = vault.get_secret("ZEPH_TELEGRAM_TOKEN").await?
            && let Some(tg) = self.telegram.as_mut()
        {
            tg.token = Some(val);
        }
        if let Some(val) = vault.get_secret("ZEPH_A2A_AUTH_TOKEN").await? {
            self.a2a.auth_token = Some(val);
        }
        for entry in &self.llm.providers {
            if entry.provider_type == crate::config::ProviderKind::Compatible
                && let Some(ref name) = entry.name
            {
                let env_key = format!("ZEPH_COMPATIBLE_{}_API_KEY", name.to_uppercase());
                if let Some(val) = vault.get_secret(&env_key).await? {
                    self.secrets
                        .compatible_api_keys
                        .insert(name.clone(), Secret::new(val));
                }
            }
        }
        if let Some(val) = vault.get_secret("ZEPH_HF_TOKEN").await? {
            self.classifiers.hf_token = Some(val.clone());
            if let Some(candle) = self.llm.candle.as_mut() {
                candle.hf_token = Some(val);
            }
        }
        if let Some(val) = vault.get_secret("ZEPH_GATEWAY_TOKEN").await? {
            self.gateway.auth_token = Some(val);
        }
        if let Some(val) = vault.get_secret("ZEPH_DATABASE_URL").await? {
            self.memory.database_url = Some(val);
        }
        if let Some(val) = vault.get_secret("ZEPH_DISCORD_TOKEN").await?
            && let Some(dc) = self.discord.as_mut()
        {
            dc.token = Some(val);
        }
        if let Some(val) = vault.get_secret("ZEPH_DISCORD_APP_ID").await?
            && let Some(dc) = self.discord.as_mut()
        {
            dc.application_id = Some(val);
        }
        if let Some(val) = vault.get_secret("ZEPH_SLACK_BOT_TOKEN").await?
            && let Some(sl) = self.slack.as_mut()
        {
            sl.bot_token = Some(val);
        }
        if let Some(val) = vault.get_secret("ZEPH_SLACK_SIGNING_SECRET").await?
            && let Some(sl) = self.slack.as_mut()
        {
            sl.signing_secret = Some(val);
        }
        for key in vault.list_keys() {
            if let Some(custom_name) = key.strip_prefix("ZEPH_SECRET_")
                && !custom_name.is_empty()
                && let Some(val) = vault.get_secret(&key).await?
            {
                // Canonical form uses underscores. Both `_` and `-` in vault key names
                // are normalized to `_` so that ZEPH_SECRET_MY-KEY and ZEPH_SECRET_MY_KEY
                // both map to "my_key", matching SKILL.md requires-secrets parsing.
                let normalized = custom_name.to_lowercase().replace('-', "_");
                self.secrets.custom.insert(normalized, Secret::new(val));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[cfg(any(test, feature = "mock"))]
    async fn resolve_secrets_with_mock_vault() {
        use crate::vault::MockVaultProvider;

        let vault = MockVaultProvider::new()
            .with_secret("ZEPH_CLAUDE_API_KEY", "sk-test-123")
            .with_secret("ZEPH_TELEGRAM_TOKEN", "tg-token-456");

        let mut config = Config::load(std::path::Path::new("/nonexistent/config.toml")).unwrap();
        config.resolve_secrets(&vault).await.unwrap();

        assert_eq!(
            config.secrets.claude_api_key.as_ref().unwrap().expose(),
            "sk-test-123"
        );
        if let Some(tg) = config.telegram {
            assert_eq!(tg.token.as_deref(), Some("tg-token-456"));
        }
    }
}
