// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

mod env;
pub mod migrate;
mod types;

#[cfg(test)]
mod tests;

pub use types::*;
pub use zeph_tools::AutonomyLevel;

use std::path::Path;

use crate::vault::VaultProvider;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config file: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to parse config file: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("config validation failed: {0}")]
    Validation(String),
    #[error("vault error: {0}")]
    Vault(#[from] crate::vault::VaultError),
}

impl Config {
    /// Load configuration from a TOML file with env var overrides.
    ///
    /// Falls back to sensible defaults when the file does not exist.
    ///
    /// # Errors
    ///
    /// Returns an error if the file exists but cannot be read or parsed.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let mut config = if path.exists() {
            let content = std::fs::read_to_string(path)?;
            toml::from_str::<Self>(&content)?
        } else {
            Self::default()
        };

        config.apply_env_overrides();
        config.normalize_legacy_runtime_defaults();
        Ok(config)
    }

    /// Validate configuration values are within sane bounds.
    ///
    /// # Errors
    ///
    /// Returns an error if any value is out of range.
    #[allow(clippy::too_many_lines)]
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.memory.history_limit > 10_000 {
            return Err(ConfigError::Validation(format!(
                "history_limit must be <= 10000, got {}",
                self.memory.history_limit
            )));
        }
        if self.memory.context_budget_tokens > 1_000_000 {
            return Err(ConfigError::Validation(format!(
                "context_budget_tokens must be <= 1000000, got {}",
                self.memory.context_budget_tokens
            )));
        }
        if self.agent.max_tool_iterations > 100 {
            return Err(ConfigError::Validation(format!(
                "max_tool_iterations must be <= 100, got {}",
                self.agent.max_tool_iterations
            )));
        }
        if self.a2a.rate_limit == 0 {
            return Err(ConfigError::Validation("a2a.rate_limit must be > 0".into()));
        }
        if self.gateway.rate_limit == 0 {
            return Err(ConfigError::Validation(
                "gateway.rate_limit must be > 0".into(),
            ));
        }
        if self.gateway.max_body_size > 10_485_760 {
            return Err(ConfigError::Validation(format!(
                "gateway.max_body_size must be <= 10485760 (10 MiB), got {}",
                self.gateway.max_body_size
            )));
        }
        if self.memory.token_safety_margin <= 0.0 {
            return Err(ConfigError::Validation(format!(
                "token_safety_margin must be > 0.0, got {}",
                self.memory.token_safety_margin
            )));
        }
        if self.memory.tool_call_cutoff == 0 {
            return Err(ConfigError::Validation(
                "tool_call_cutoff must be >= 1".into(),
            ));
        }
        if let crate::config::CompressionStrategy::Proactive {
            threshold_tokens,
            max_summary_tokens,
        } = &self.memory.compression.strategy
        {
            if *threshold_tokens < 1_000 {
                return Err(ConfigError::Validation(format!(
                    "compression.threshold_tokens must be >= 1000, got {threshold_tokens}"
                )));
            }
            if *max_summary_tokens < 128 {
                return Err(ConfigError::Validation(format!(
                    "compression.max_summary_tokens must be >= 128, got {max_summary_tokens}"
                )));
            }
        }
        if !self.memory.soft_compaction_threshold.is_finite()
            || self.memory.soft_compaction_threshold <= 0.0
            || self.memory.soft_compaction_threshold >= 1.0
        {
            return Err(ConfigError::Validation(format!(
                "soft_compaction_threshold must be in (0.0, 1.0) exclusive, got {}",
                self.memory.soft_compaction_threshold
            )));
        }
        if !self.memory.hard_compaction_threshold.is_finite()
            || self.memory.hard_compaction_threshold <= 0.0
            || self.memory.hard_compaction_threshold >= 1.0
        {
            return Err(ConfigError::Validation(format!(
                "hard_compaction_threshold must be in (0.0, 1.0) exclusive, got {}",
                self.memory.hard_compaction_threshold
            )));
        }
        if self.memory.soft_compaction_threshold >= self.memory.hard_compaction_threshold {
            return Err(ConfigError::Validation(format!(
                "soft_compaction_threshold ({}) must be less than hard_compaction_threshold ({})",
                self.memory.soft_compaction_threshold, self.memory.hard_compaction_threshold,
            )));
        }
        if self.memory.graph.temporal_decay_rate < 0.0
            || self.memory.graph.temporal_decay_rate > 10.0
        {
            return Err(ConfigError::Validation(format!(
                "memory.graph.temporal_decay_rate must be in [0.0, 10.0], got {}",
                self.memory.graph.temporal_decay_rate
            )));
        }
        // MCP server validation
        {
            use std::collections::HashSet;
            let mut seen_oauth_vault_keys: HashSet<String> = HashSet::new();
            for s in &self.mcp.servers {
                // headers and oauth are mutually exclusive
                if !s.headers.is_empty() && s.oauth.as_ref().is_some_and(|o| o.enabled) {
                    return Err(ConfigError::Validation(format!(
                        "MCP server '{}': cannot use both 'headers' and 'oauth' simultaneously",
                        s.id
                    )));
                }
                // vault key collision detection
                if s.oauth.as_ref().is_some_and(|o| o.enabled) {
                    let key = format!("ZEPH_MCP_OAUTH_{}", s.id.to_uppercase().replace('-', "_"));
                    if !seen_oauth_vault_keys.insert(key.clone()) {
                        return Err(ConfigError::Validation(format!(
                            "MCP server '{}' has vault key collision ('{key}'): another server \
                             with the same normalized ID already uses this key",
                            s.id
                        )));
                    }
                }
            }
        }

        self.experiments.validate()?;

        // Focus config validation
        if self.agent.focus.compression_interval == 0 {
            return Err(ConfigError::Validation(
                "agent.focus.compression_interval must be >= 1".into(),
            ));
        }
        if self.agent.focus.min_messages_per_focus == 0 {
            return Err(ConfigError::Validation(
                "agent.focus.min_messages_per_focus must be >= 1".into(),
            ));
        }

        // SideQuest config validation
        if self.memory.sidequest.interval_turns == 0 {
            return Err(ConfigError::Validation(
                "memory.sidequest.interval_turns must be >= 1".into(),
            ));
        }
        if !self.memory.sidequest.max_eviction_ratio.is_finite()
            || self.memory.sidequest.max_eviction_ratio <= 0.0
            || self.memory.sidequest.max_eviction_ratio > 1.0
        {
            return Err(ConfigError::Validation(format!(
                "memory.sidequest.max_eviction_ratio must be in (0.0, 1.0], got {}",
                self.memory.sidequest.max_eviction_ratio
            )));
        }

        Ok(())
    }

    /// Resolve sensitive configuration values through the vault.
    ///
    /// # Errors
    ///
    /// Returns an error if the vault backend fails.
    pub async fn resolve_secrets(&mut self, vault: &dyn VaultProvider) -> Result<(), ConfigError> {
        use crate::vault::Secret;

        if let Some(val) = vault.get_secret("ZEPH_CLAUDE_API_KEY").await? {
            self.secrets.claude_api_key = Some(Secret::new(val));
        }
        if let Some(val) = vault.get_secret("ZEPH_OPENAI_API_KEY").await? {
            self.secrets.openai_api_key = Some(Secret::new(val));
        }
        if let Some(val) = vault.get_secret("ZEPH_GEMINI_API_KEY").await? {
            self.secrets.gemini_api_key = Some(Secret::new(val));
        }
        if let Some(val) = vault.get_secret("ZEPH_TELEGRAM_TOKEN").await? {
            let tg = self.telegram.get_or_insert(TelegramConfig {
                token: None,
                allowed_users: Vec::new(),
            });
            tg.token = Some(val);
        }
        if let Some(val) = vault.get_secret("ZEPH_A2A_AUTH_TOKEN").await? {
            self.a2a.auth_token = Some(val);
        }
        if let Some(ref entries) = self.llm.compatible {
            for entry in entries {
                let env_key = format!("ZEPH_COMPATIBLE_{}_API_KEY", entry.name.to_uppercase());
                if let Some(val) = vault.get_secret(&env_key).await? {
                    self.secrets
                        .compatible_api_keys
                        .insert(entry.name.clone(), Secret::new(val));
                }
            }
        }
        if let Some(val) = vault.get_secret("ZEPH_GATEWAY_TOKEN").await? {
            self.gateway.auth_token = Some(val);
        }
        if let Some(val) = vault.get_secret("ZEPH_DISCORD_TOKEN").await? {
            let dc = self.discord.get_or_insert(DiscordConfig {
                token: None,
                application_id: None,
                allowed_user_ids: Vec::new(),
                allowed_role_ids: Vec::new(),
                allowed_channel_ids: Vec::new(),
            });
            dc.token = Some(val);
        }
        if let Some(val) = vault.get_secret("ZEPH_DISCORD_APP_ID").await?
            && let Some(dc) = self.discord.as_mut()
        {
            dc.application_id = Some(val);
        }
        if let Some(val) = vault.get_secret("ZEPH_SLACK_BOT_TOKEN").await? {
            let sl = self.slack.get_or_insert(SlackConfig {
                bot_token: None,
                signing_secret: None,
                webhook_host: "127.0.0.1".into(),
                port: 3000,
                allowed_user_ids: Vec::new(),
                allowed_channel_ids: Vec::new(),
            });
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

    fn normalize_legacy_runtime_defaults(&mut self) {
        if is_legacy_default_sqlite_path(&self.memory.sqlite_path) {
            self.memory.sqlite_path = default_sqlite_path();
        }

        for skill_path in &mut self.skills.paths {
            if is_legacy_default_skills_path(skill_path) {
                *skill_path = default_skills_dir();
            }
        }

        if is_legacy_default_debug_dir(&self.debug.output_dir) {
            self.debug.output_dir = default_debug_dir();
        }

        if is_legacy_default_log_file(&self.logging.file) {
            self.logging.file = default_log_file_path();
        }
    }
}
