mod env;
mod types;

#[cfg(test)]
mod tests;

pub use types::*;
pub use zeph_tools::AutonomyLevel;

use std::path::Path;

use anyhow::Context;

use crate::vault::VaultProvider;

impl Config {
    /// Load configuration from a TOML file with env var overrides.
    ///
    /// Falls back to sensible defaults when the file does not exist.
    ///
    /// # Errors
    ///
    /// Returns an error if the file exists but cannot be read or parsed.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let mut config = if path.exists() {
            let content = std::fs::read_to_string(path).context("failed to read config file")?;
            toml::from_str::<Self>(&content).context("failed to parse config file")?
        } else {
            Self::default()
        };

        config.apply_env_overrides();
        Ok(config)
    }

    /// Validate configuration values are within sane bounds.
    ///
    /// # Errors
    ///
    /// Returns an error if any value is out of range.
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.memory.history_limit <= 10_000,
            "history_limit must be <= 10000, got {}",
            self.memory.history_limit
        );
        if self.memory.context_budget_tokens > 0 {
            anyhow::ensure!(
                self.memory.context_budget_tokens <= 1_000_000,
                "context_budget_tokens must be <= 1000000, got {}",
                self.memory.context_budget_tokens
            );
        }
        anyhow::ensure!(
            self.agent.max_tool_iterations <= 100,
            "max_tool_iterations must be <= 100, got {}",
            self.agent.max_tool_iterations
        );
        anyhow::ensure!(self.a2a.rate_limit > 0, "a2a.rate_limit must be > 0");
        anyhow::ensure!(
            self.gateway.rate_limit > 0,
            "gateway.rate_limit must be > 0"
        );
        Ok(())
    }

    /// Resolve sensitive configuration values through the vault.
    ///
    /// # Errors
    ///
    /// Returns an error if the vault backend fails.
    pub async fn resolve_secrets(&mut self, vault: &dyn VaultProvider) -> anyhow::Result<()> {
        use crate::vault::Secret;

        if let Some(val) = vault.get_secret("ZEPH_CLAUDE_API_KEY").await? {
            self.secrets.claude_api_key = Some(Secret::new(val));
        }
        if let Some(val) = vault.get_secret("ZEPH_OPENAI_API_KEY").await? {
            self.secrets.openai_api_key = Some(Secret::new(val));
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
        Ok(())
    }
}
