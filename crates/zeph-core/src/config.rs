// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Extension trait for resolving vault secrets into a Config.
//!
//! This trait is defined in zeph-core (not in zeph-config) due to Rust's orphan rule:
//! implementing a foreign trait on a foreign type requires the trait to be defined locally.

// Re-export Config types from zeph-config for internal use.
pub use zeph_config::{
    AcpAuthMethod, AcpConfig, AcpLspConfig, AcpTransport, AdditionalDir, AdditionalDirError,
    AgentConfig, CandleConfig, CandleDevice, CandleInlineConfig, CandleSource,
    CascadeClassifierMode, CascadeConfig, ClassifiersConfig, CompressionConfig,
    CompressionStrategy, Config, ConfigError, ContextFormat, CostConfig, DaemonConfig, DebugConfig,
    DetectorMode, DiscordConfig, DocumentConfig, DumpFormat, ExperimentConfig, ExperimentSchedule,
    FocusConfig, GatewayConfig, GenerationParams, GonkaNode, GraphConfig, HookAction, HookDef,
    HookMatcher, IndexConfig, LearningConfig, LlmConfig, LlmRoutingStrategy, LogRotation,
    LoggingConfig, MAX_TOKENS_CAP, McpConfig, McpOAuthConfig, McpServerConfig, McpTrustLevel,
    MemoryConfig, MemoryScope, NoteLinkingConfig, OAuthTokenStorage, OrchestrationConfig,
    PermissionMode, ProviderEntry, ProviderKind, ProviderName, PruningStrategy, RateLimitConfig,
    RegistryBackendKind, RegistryConfig, ResolvedSecrets, RetrievalConfig, RouterConfig,
    RouterStrategyConfig, ScheduledTaskConfig, ScheduledTaskKind, SchedulerConfig,
    SchedulerDaemonConfig, SchedulerSecurityConfig, SecurityConfig, SemanticConfig, SessionsConfig,
    SidequestConfig, SkillFilter, SkillPromptMode, SkillsConfig, SlackConfig, StoreRoutingConfig,
    StoreRoutingStrategy, SttConfig, SubAgentConfig, SubAgentLifecycleHooks, SubagentHooks,
    TaskSupervisorConfig, TelegramConfig, TimeoutConfig, ToolDiscoveryConfig,
    ToolDiscoveryStrategyConfig, ToolFilterConfig, ToolPolicy, TraceConfig, TrustConfig, TuiConfig,
    VaultConfig, VectorBackend,
};

pub use zeph_config::{
    AutoDreamConfig, CategoryConfig, ContextStrategy, DigestConfig, MagicDocsConfig,
    MicrocompactConfig, PersonaConfig, TrajectoryConfig, TreeConfig,
};
pub use zeph_config::{DiagnosticSeverity, DiagnosticsConfig, HoverConfig, LspConfig};
pub use zeph_config::{DurableBackend, DurableConfig, RetentionPolicy};
pub use zeph_config::{QualityConfig, TriggerPolicy};
pub use zeph_config::{TelemetryBackend, TelemetryConfig};

pub use zeph_config::{
    ContentIsolationConfig, CustomPiiPattern, ExfiltrationGuardConfig, MemoryWriteValidationConfig,
    PiiFilterConfig, QuarantineConfig,
};
pub use zeph_config::{GuardrailAction, GuardrailConfig, GuardrailFailStrategy};

pub use zeph_config::A2aClientConfig;
pub use zeph_config::A2aServerConfig;
pub use zeph_config::ChannelSkillsConfig;
pub use zeph_config::{CardTrustPolicy, TrustedAgentKey};
pub use zeph_config::{FileChangedConfig, HooksConfig};

pub use zeph_config::{
    DEFAULT_DEBUG_DIR, DEFAULT_LOG_FILE, DEFAULT_SKILLS_DIR, DEFAULT_SQLITE_PATH,
    default_debug_dir, default_log_file_path, default_skills_dir, default_sqlite_path,
    is_legacy_default_debug_dir, is_legacy_default_log_file, is_legacy_default_skills_path,
    is_legacy_default_sqlite_path,
};

pub use zeph_config::providers::{default_stt_language, validate_pool};

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

    /// Same as [`resolve_secrets`](Self::resolve_secrets), but additionally registers every
    /// successfully resolved secret value with `registry` for outbound LLM masking (#5437).
    ///
    /// When `registry` is `None` (secret masking disabled), behaves identically to
    /// `resolve_secrets`.
    ///
    /// # Errors
    ///
    /// Returns an error if the vault backend fails.
    fn resolve_secrets_masked(
        &mut self,
        vault: &dyn VaultProvider,
        registry: Option<&std::sync::Arc<zeph_sanitizer::secret_mask::SecretMaskRegistry>>,
    ) -> impl std::future::Future<Output = Result<(), ConfigError>> + Send;
}

/// Registers a resolved secret with the PAAC mask registry, when one is attached.
///
/// No-op when `registry` is `None` (secret masking disabled or not yet bootstrapped).
fn register_masked_secret(
    registry: Option<&std::sync::Arc<zeph_sanitizer::secret_mask::SecretMaskRegistry>>,
    key_name: &str,
    value: &str,
) {
    if let Some(registry) = registry {
        registry.register(
            key_name,
            value,
            zeph_sanitizer::secret_mask::SecretCategory::from_key_name(key_name),
        );
    }
}

fn log_gonka_credential_status(has_key: bool, has_address: bool) {
    match (has_key, has_address) {
        (true, true) => tracing::info!("gonka wallet credentials resolved from vault"),
        (true, false) => tracing::warn!(
            "ZEPH_GONKA_PRIVATE_KEY is set but ZEPH_GONKA_ADDRESS is missing from vault"
        ),
        (false, true) => tracing::warn!(
            "ZEPH_GONKA_ADDRESS is set but ZEPH_GONKA_PRIVATE_KEY is missing from vault"
        ),
        (false, false) => {}
    }
}

// TODO(critic): vault values are inserted verbatim into typed config fields.
// Consider centralizing whitespace trimming and empty-string normalization
// here so every consumer (qdrant client, claude, openai, etc.) sees a clean value.
impl SecretResolver for Config {
    async fn resolve_secrets(&mut self, vault: &dyn VaultProvider) -> Result<(), ConfigError> {
        self.resolve_secrets_masked(vault, None).await
    }

    #[allow(clippy::too_many_lines)] // one branch per vault key, each 2-4 lines — flat by design
    async fn resolve_secrets_masked(
        &mut self,
        vault: &dyn VaultProvider,
        registry: Option<&std::sync::Arc<zeph_sanitizer::secret_mask::SecretMaskRegistry>>,
    ) -> Result<(), ConfigError> {
        if let Some(val) = vault.get_secret("ZEPH_CLAUDE_API_KEY").await? {
            register_masked_secret(registry, "ZEPH_CLAUDE_API_KEY", &val);
            self.secrets.claude_api_key = Some(Secret::new(val));
        }
        if let Some(val) = vault.get_secret("ZEPH_OPENAI_API_KEY").await? {
            register_masked_secret(registry, "ZEPH_OPENAI_API_KEY", &val);
            self.secrets.openai_api_key = Some(Secret::new(val));
        }
        if let Some(val) = vault.get_secret("ZEPH_GEMINI_API_KEY").await? {
            register_masked_secret(registry, "ZEPH_GEMINI_API_KEY", &val);
            self.secrets.gemini_api_key = Some(Secret::new(val));
        }
        if let Some(val) = vault.get_secret("ZEPH_GONKA_PRIVATE_KEY").await? {
            register_masked_secret(registry, "ZEPH_GONKA_PRIVATE_KEY", &val);
            self.secrets.gonka_private_key = Some(Secret::new(val));
        }
        if let Some(val) = vault.get_secret("ZEPH_GONKA_ADDRESS").await? {
            // M1 (#5437 critique): a wallet address is a public identifier, not a secret —
            // masking it would only suppress legitimate model reasoning about it, with no
            // confidentiality benefit.
            self.secrets.gonka_address = Some(Secret::new(val));
        }
        if let Some(val) = vault.get_secret("ZEPH_COCOON_ACCESS_HASH").await? {
            register_masked_secret(registry, "ZEPH_COCOON_ACCESS_HASH", &val);
            self.secrets.cocoon_access_hash = Some(Secret::new(val));
        }
        // Registry lookups are strictly opt-in (NFR-001): only touch the vault when the
        // registry is enabled AND a key name is configured, never unconditionally.
        if self.skills.registry.enabled
            && let Some(key_name) = self.skills.registry.auth_vault_key.clone()
            && let Some(val) = vault.get_secret(&key_name).await?
        {
            register_masked_secret(registry, &key_name, &val);
            self.secrets.skill_registry_token = Some(Secret::new(val));
        }
        log_gonka_credential_status(
            self.secrets.gonka_private_key.is_some(),
            self.secrets.gonka_address.is_some(),
        );
        if let Some(val) = vault.get_secret("ZEPH_TELEGRAM_TOKEN").await?
            && let Some(tg) = self.telegram.as_mut()
        {
            register_masked_secret(registry, "ZEPH_TELEGRAM_TOKEN", &val);
            tg.token = Some(val);
        }
        if let Some(val) = vault.get_secret("ZEPH_A2A_AUTH_TOKEN").await? {
            // #6268: an empty-string secret must be treated as "not configured", not as a
            // valid "" bearer token that `AuthConfig`/`auth_middleware` would otherwise
            // (pre-fix) accept from any request with no Authorization header at all.
            if val.is_empty() {
                tracing::warn!(
                    "ZEPH_A2A_AUTH_TOKEN resolved to an empty string; treating as not configured"
                );
            } else {
                register_masked_secret(registry, "ZEPH_A2A_AUTH_TOKEN", &val);
                self.a2a.auth_token = Some(val);
            }
        }
        for entry in &self.llm.providers {
            if entry.provider_type == crate::config::ProviderKind::Compatible
                && let Some(ref name) = entry.name
            {
                let normalized: String = name
                    .to_uppercase()
                    .chars()
                    .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                    .collect();
                let env_key = format!("ZEPH_COMPATIBLE_{normalized}_API_KEY");
                if let Some(val) = vault.get_secret(&env_key).await? {
                    register_masked_secret(registry, &env_key, &val);
                    self.secrets
                        .compatible_api_keys
                        .insert(name.clone(), Secret::new(val));
                }
            }
        }
        if let Some(val) = vault.get_secret("ZEPH_HF_TOKEN").await? {
            register_masked_secret(registry, "ZEPH_HF_TOKEN", &val);
            self.classifiers.hf_token = Some(val.clone());
            if let Some(candle) = self.llm.candle.as_mut() {
                candle.hf_token = Some(val);
            }
        }
        if let Some(val) = vault.get_secret("ZEPH_GATEWAY_TOKEN").await? {
            // #6268: same rationale as ZEPH_A2A_AUTH_TOKEN above — an empty secret is
            // "not configured", not a valid "" bearer token.
            if val.is_empty() {
                tracing::warn!(
                    "ZEPH_GATEWAY_TOKEN resolved to an empty string; treating as not configured"
                );
            } else {
                register_masked_secret(registry, "ZEPH_GATEWAY_TOKEN", &val);
                self.gateway.auth_token = Some(val);
            }
        }
        if let Some(val) = vault.get_secret("ZEPH_DATABASE_URL").await? {
            register_masked_secret(registry, "ZEPH_DATABASE_URL", &val);
            self.memory.database_url = Some(Secret::new(val));
        }
        if let Some(val) = vault.get_secret("ZEPH_QDRANT_API_KEY").await? {
            register_masked_secret(registry, "ZEPH_QDRANT_API_KEY", &val);
            self.memory.qdrant_api_key = Some(Secret::new(val));
        }
        if let Some(val) = vault.get_secret("ZEPH_DISCORD_TOKEN").await?
            && let Some(dc) = self.discord.as_mut()
        {
            register_masked_secret(registry, "ZEPH_DISCORD_TOKEN", &val);
            dc.token = Some(val);
        }
        if let Some(val) = vault.get_secret("ZEPH_DISCORD_APP_ID").await?
            && let Some(dc) = self.discord.as_mut()
        {
            // M1 (#5437 critique): the Discord application ID is a public snowflake, not a
            // secret — masking it would only suppress legitimate model reasoning about it.
            dc.application_id = Some(val);
        }
        if let Some(val) = vault.get_secret("ZEPH_SLACK_BOT_TOKEN").await?
            && let Some(sl) = self.slack.as_mut()
        {
            register_masked_secret(registry, "ZEPH_SLACK_BOT_TOKEN", &val);
            sl.bot_token = Some(val);
        }
        if let Some(val) = vault.get_secret("ZEPH_SLACK_SIGNING_SECRET").await?
            && let Some(sl) = self.slack.as_mut()
        {
            register_masked_secret(registry, "ZEPH_SLACK_SIGNING_SECRET", &val);
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
                register_masked_secret(registry, &key, &val);
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

    #[tokio::test]
    #[cfg(any(test, feature = "mock"))]
    async fn resolve_gonka_secrets_both_present() {
        use crate::vault::MockVaultProvider;

        let vault = MockVaultProvider::new()
            .with_secret("ZEPH_GONKA_PRIVATE_KEY", "gonka-priv-key-abc")
            .with_secret("ZEPH_GONKA_ADDRESS", "gonka1xyzaddress");

        let mut config = Config::load(std::path::Path::new("/nonexistent/config.toml")).unwrap();
        config.resolve_secrets(&vault).await.unwrap();

        assert_eq!(
            config.secrets.gonka_private_key.as_ref().unwrap().expose(),
            "gonka-priv-key-abc"
        );
        assert_eq!(
            config.secrets.gonka_address.as_ref().unwrap().expose(),
            "gonka1xyzaddress"
        );
    }

    #[tokio::test]
    #[cfg(any(test, feature = "mock"))]
    async fn resolve_gonka_partial_only_private_key() {
        use crate::vault::MockVaultProvider;

        let vault =
            MockVaultProvider::new().with_secret("ZEPH_GONKA_PRIVATE_KEY", "gonka-priv-key-only");

        let mut config = Config::load(std::path::Path::new("/nonexistent/config.toml")).unwrap();
        config.resolve_secrets(&vault).await.unwrap();

        assert!(config.secrets.gonka_private_key.is_some());
        assert!(config.secrets.gonka_address.is_none());
    }

    // --- resolve_secrets_masked / PAAC secret mask registry (#5437) ---

    #[tokio::test]
    #[cfg(any(test, feature = "mock"))]
    async fn resolve_secrets_masked_registers_values_with_registry() {
        use crate::vault::MockVaultProvider;
        use std::sync::Arc;
        use zeph_sanitizer::secret_mask::SecretMaskRegistry;

        let vault = MockVaultProvider::new()
            .with_secret("ZEPH_CLAUDE_API_KEY", "sk-claude-secret-value-1234")
            .with_secret("ZEPH_OPENAI_API_KEY", "sk-openai-secret-value-5678");

        let mut config = Config::load(std::path::Path::new("/nonexistent/config.toml")).unwrap();
        let registry = Arc::new(SecretMaskRegistry::new());
        config
            .resolve_secrets_masked(&vault, Some(&registry))
            .await
            .unwrap();

        assert_eq!(
            registry.len(),
            2,
            "both resolved secrets must be registered"
        );
        let masked = registry.mask("token: sk-claude-secret-value-1234");
        assert!(!masked.contains("sk-claude-secret-value-1234"));
        assert!(masked.contains("<SECRET:api_key:"));
    }

    #[tokio::test]
    #[cfg(any(test, feature = "mock"))]
    async fn resolve_secrets_masked_none_registry_behaves_like_resolve_secrets() {
        use crate::vault::MockVaultProvider;

        let vault = MockVaultProvider::new().with_secret("ZEPH_CLAUDE_API_KEY", "sk-test-123");
        let mut config = Config::load(std::path::Path::new("/nonexistent/config.toml")).unwrap();
        config.resolve_secrets_masked(&vault, None).await.unwrap();

        assert_eq!(
            config.secrets.claude_api_key.as_ref().unwrap().expose(),
            "sk-test-123",
            "None registry must not change resolution behavior"
        );
    }

    #[tokio::test]
    #[cfg(any(test, feature = "mock"))]
    async fn resolve_secrets_plain_delegates_without_registering() {
        use crate::vault::MockVaultProvider;

        // Plain resolve_secrets() (used by every call site except bootstrap) must still work
        // unchanged — it delegates to resolve_secrets_masked with registry = None.
        let vault = MockVaultProvider::new().with_secret("ZEPH_OPENAI_API_KEY", "sk-openai-abc");
        let mut config = Config::load(std::path::Path::new("/nonexistent/config.toml")).unwrap();
        config.resolve_secrets(&vault).await.unwrap();

        assert_eq!(
            config.secrets.openai_api_key.as_ref().unwrap().expose(),
            "sk-openai-abc"
        );
    }

    #[tokio::test]
    #[cfg(any(test, feature = "mock"))]
    async fn resolve_secrets_empty_gateway_token_treated_as_not_configured() {
        use crate::vault::MockVaultProvider;

        // #6268: an empty-string ZEPH_GATEWAY_TOKEN must not become `Some("")` — that would
        // let AuthConfig hash "" and accept any request with no Authorization header at all.
        let vault = MockVaultProvider::new().with_secret("ZEPH_GATEWAY_TOKEN", "");
        let mut config = Config::load(std::path::Path::new("/nonexistent/config.toml")).unwrap();
        config.resolve_secrets(&vault).await.unwrap();

        assert!(config.gateway.auth_token.is_none());
    }

    #[tokio::test]
    #[cfg(any(test, feature = "mock"))]
    async fn resolve_secrets_empty_a2a_token_treated_as_not_configured() {
        use crate::vault::MockVaultProvider;

        let vault = MockVaultProvider::new().with_secret("ZEPH_A2A_AUTH_TOKEN", "");
        let mut config = Config::load(std::path::Path::new("/nonexistent/config.toml")).unwrap();
        config.resolve_secrets(&vault).await.unwrap();

        assert!(config.a2a.auth_token.is_none());
    }
}
