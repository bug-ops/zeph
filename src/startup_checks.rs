// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Startup mutual-exclusion checks for CLI mode flags.

use zeph_core::config::Config;

use crate::cli::Cli;

/// Validate that the selected mode flags are mutually compatible.
///
/// Called once at startup before any channel or subsystem is constructed.
///
/// # Errors
///
/// Returns an error string if two incompatible flags are active simultaneously.
pub(crate) fn validate_mode_compatibility(cli: &Cli, config: &Config) -> anyhow::Result<()> {
    #[cfg(feature = "tui")]
    if cli.json && cli.tui {
        anyhow::bail!("--json and --tui are mutually exclusive (both claim stdout)");
    }

    #[cfg(feature = "acp")]
    if cli.json && cli.acp {
        anyhow::bail!(
            "--json and --acp are mutually exclusive (both claim stdout; ACP owns JSON-RPC framing)"
        );
    }

    if cli.json
        && config
            .telegram
            .as_ref()
            .and_then(|t| t.token.as_ref())
            .is_some()
    {
        anyhow::bail!(
            "--json cannot be used while a Telegram token is configured; \
             JSON events are stdout-only and Telegram is a separate transport"
        );
    }

    #[cfg(feature = "discord")]
    if cli.json
        && config
            .discord
            .as_ref()
            .and_then(|d| d.token.as_ref())
            .is_some()
    {
        anyhow::bail!("--json cannot be used alongside a configured Discord channel");
    }

    #[cfg(feature = "slack")]
    if cli.json
        && config
            .slack
            .as_ref()
            .and_then(|s| s.bot_token.as_ref())
            .is_some()
    {
        anyhow::bail!("--json cannot be used alongside a configured Slack channel");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cli_json() -> Cli {
        Cli {
            json: true,
            ..Default::default()
        }
    }

    #[test]
    fn json_with_telegram_token_is_rejected() {
        use zeph_config::{ChannelSkillsConfig, TelegramConfig};
        let cli = cli_json();
        let cfg = Config {
            telegram: Some(TelegramConfig {
                token: Some("tok".into()),
                allowed_users: vec![],
                skills: ChannelSkillsConfig::default(),
            }),
            ..Default::default()
        };
        assert!(validate_mode_compatibility(&cli, &cfg).is_err());
    }

    #[test]
    fn json_without_conflicts_is_ok() {
        let cli = cli_json();
        let cfg = Config::default();
        assert!(validate_mode_compatibility(&cli, &cfg).is_ok());
    }

    #[test]
    #[cfg(feature = "tui")]
    fn json_with_tui_is_rejected() {
        let cli = Cli {
            json: true,
            tui: true,
            ..Default::default()
        };
        let cfg = Config::default();
        assert!(validate_mode_compatibility(&cli, &cfg).is_err());
    }

    #[test]
    #[cfg(feature = "discord")]
    fn json_with_discord_token_is_rejected() {
        use zeph_config::{ChannelSkillsConfig, DiscordConfig};
        let cli = cli_json();
        let cfg = Config {
            discord: Some(DiscordConfig {
                token: Some("discord-tok".into()),
                application_id: None,
                allowed_user_ids: vec![],
                allowed_role_ids: vec![],
                allowed_channel_ids: vec![],
                skills: ChannelSkillsConfig::default(),
            }),
            ..Default::default()
        };
        assert!(validate_mode_compatibility(&cli, &cfg).is_err());
    }

    #[test]
    #[cfg(feature = "slack")]
    fn json_with_slack_token_is_rejected() {
        use zeph_config::{ChannelSkillsConfig, SlackConfig};
        let cli = cli_json();
        let cfg = Config {
            slack: Some(SlackConfig {
                bot_token: Some("xoxb-slack".into()),
                signing_secret: None,
                webhook_host: "0.0.0.0".into(),
                port: 3000,
                allowed_user_ids: vec![],
                allowed_channel_ids: vec![],
                skills: ChannelSkillsConfig::default(),
            }),
            ..Default::default()
        };
        assert!(validate_mode_compatibility(&cli, &cfg).is_err());
    }
}
