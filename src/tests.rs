// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::channel::create_channel;
use crate::cli::{Cli, Command, VaultCommand};
use clap::Parser;
use std::path::{Path, PathBuf};
use zeph_channels::{AnyChannel, CliChannel};
use zeph_core::channel::Channel;
use zeph_core::config::{Config, ProviderKind};

#[tokio::test]
async fn create_channel_returns_cli_when_no_telegram() {
    let config = Config::load(Path::new("/nonexistent/config.toml")).unwrap();
    let channel = create_channel(&config).await.unwrap();
    assert!(matches!(channel, AnyChannel::Cli(_)));
}

#[test]
fn any_channel_debug_cli() {
    let ch = AnyChannel::Cli(CliChannel::new());
    let debug = format!("{ch:?}");
    assert!(debug.contains("Cli"));
}

#[tokio::test]
async fn any_channel_cli_send() {
    let mut ch = AnyChannel::Cli(CliChannel::new());
    let result = ch.send("test message").await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn any_channel_cli_send_chunk() {
    let mut ch = AnyChannel::Cli(CliChannel::new());
    let result = ch.send_chunk("chunk").await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn any_channel_cli_flush_chunks() {
    let mut ch = AnyChannel::Cli(CliChannel::new());
    ch.send_chunk("data").await.unwrap();
    let result = ch.flush_chunks().await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn any_channel_cli_send_typing() {
    let mut ch = AnyChannel::Cli(CliChannel::new());
    let result = ch.send_typing().await;
    assert!(result.is_ok());
}

#[test]
fn config_loading_from_default_toml() {
    let config = Config::load(Path::new("config/default.toml")).unwrap();
    assert_eq!(
        config.skills.paths,
        vec![zeph_core::config::default_skills_dir()]
    );
    assert_eq!(
        config.memory.sqlite_path,
        zeph_core::config::default_sqlite_path()
    );
    assert_eq!(
        config.debug.output_dir,
        zeph_core::config::default_debug_dir()
    );
    assert_eq!(
        config.logging.file,
        zeph_core::config::default_log_file_path()
    );
}

#[test]
fn config_loading_nonexistent_uses_defaults() {
    let config = Config::load(Path::new("/does/not/exist.toml")).unwrap();
    assert_eq!(config.llm.effective_provider(), ProviderKind::Ollama);
    assert_eq!(config.agent.name, "Zeph");
}

#[tokio::test]
async fn create_channel_no_telegram_config() {
    let mut config = Config::load(Path::new("/nonexistent")).unwrap();
    config.telegram = None;
    let channel = create_channel(&config).await.unwrap();
    assert!(matches!(channel, AnyChannel::Cli(_)));
}

#[tokio::test]
async fn create_channel_telegram_without_token() {
    let mut config = Config::load(Path::new("/nonexistent")).unwrap();
    config.telegram = Some(zeph_core::config::TelegramConfig {
        token: None,
        allowed_users: vec![],
    });
    let channel = create_channel(&config).await.unwrap();
    assert!(matches!(channel, AnyChannel::Cli(_)));
}

#[test]
fn any_channel_debug_telegram() {
    use zeph_channels::telegram::TelegramChannel;
    let tg = TelegramChannel::new("test_token".to_string(), vec![]);
    let ch = AnyChannel::Telegram(tg);
    let debug = format!("{ch:?}");
    assert!(debug.contains("Telegram"));
}

#[tokio::test]
async fn any_channel_telegram_send_typing() {
    use zeph_channels::telegram::TelegramChannel;
    let tg = TelegramChannel::new("invalid_token_for_test".to_string(), vec![]);
    let mut ch = AnyChannel::Telegram(tg);
    let _result = ch.send_typing().await;
}

#[tokio::test]
async fn create_channel_telegram_with_token() {
    let mut config = Config::load(Path::new("/nonexistent")).unwrap();
    config.telegram = Some(zeph_core::config::TelegramConfig {
        token: Some("test_token".to_string()),
        allowed_users: vec!["testuser".to_string()],
    });
    let channel = create_channel(&config).await.unwrap();
    assert!(matches!(channel, AnyChannel::Telegram(_)));
}

#[cfg(feature = "discord")]
#[tokio::test]
async fn create_channel_discord_without_token_falls_through() {
    let mut config = Config::load(Path::new("/nonexistent")).unwrap();
    config.discord = Some(zeph_core::config::DiscordConfig {
        token: None,
        application_id: None,
        allowed_user_ids: vec![],
        allowed_role_ids: vec![],
        allowed_channel_ids: vec![],
    });
    config.telegram = None;
    let channel = create_channel(&config).await.unwrap();
    assert!(matches!(channel, AnyChannel::Cli(_)));
}

#[cfg(feature = "slack")]
#[tokio::test]
async fn create_channel_slack_without_token_falls_through() {
    let mut config = Config::load(Path::new("/nonexistent")).unwrap();
    config.slack = Some(zeph_core::config::SlackConfig {
        bot_token: None,
        signing_secret: None,
        webhook_host: "127.0.0.1".into(),
        port: 3000,
        allowed_user_ids: vec![],
        allowed_channel_ids: vec![],
    });
    config.telegram = None;
    let channel = create_channel(&config).await.unwrap();
    assert!(matches!(channel, AnyChannel::Cli(_)));
}

#[tokio::test]
async fn create_channel_telegram_with_empty_allowed_users_errors() {
    let mut config = Config::load(Path::new("/nonexistent")).unwrap();
    config.telegram = Some(zeph_core::config::TelegramConfig {
        token: Some("test_token2".to_string()),
        allowed_users: vec![],
    });
    let result = create_channel(&config).await;
    assert!(result.is_err());
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("allowed_users must not be empty")
    );
}

#[test]
fn cli_parse_no_args_runs_default() {
    let cli = Cli::try_parse_from(["zeph"]).unwrap();
    assert!(cli.command.is_none());
    assert!(!cli.tui);
    assert!(cli.config.is_none());
}

#[test]
fn cli_parse_init_subcommand() {
    let cli = Cli::try_parse_from(["zeph", "init"]).unwrap();
    assert!(matches!(cli.command, Some(Command::Init { output: None })));
}

#[test]
fn cli_parse_init_with_output() {
    let cli = Cli::try_parse_from(["zeph", "init", "-o", "/tmp/cfg.toml"]).unwrap();
    match cli.command {
        Some(Command::Init { output }) => {
            assert_eq!(output.unwrap(), PathBuf::from("/tmp/cfg.toml"));
        }
        _ => panic!("expected Init subcommand"),
    }
}

#[test]
fn cli_parse_tui_flag() {
    let cli = Cli::try_parse_from(["zeph", "--tui"]).unwrap();
    assert!(cli.tui);
}

#[test]
fn cli_parse_config_flag() {
    let cli = Cli::try_parse_from(["zeph", "--config", "my.toml"]).unwrap();
    assert_eq!(cli.config.unwrap(), PathBuf::from("my.toml"));
}

#[test]
fn cli_parse_vault_flags() {
    let cli = Cli::try_parse_from([
        "zeph",
        "--vault",
        "age",
        "--vault-key",
        "/k",
        "--vault-path",
        "/v",
    ])
    .unwrap();
    assert_eq!(cli.vault.as_deref(), Some("age"));
    assert_eq!(cli.vault_key.unwrap(), PathBuf::from("/k"));
    assert_eq!(cli.vault_path.unwrap(), PathBuf::from("/v"));
}

#[test]
fn cli_parse_vault_defaults_to_none() {
    let cli = Cli::try_parse_from(["zeph"]).unwrap();
    assert!(cli.vault.is_none());
    assert!(cli.vault_key.is_none());
    assert!(cli.vault_path.is_none());
}

#[test]
fn cli_parse_vault_partial_flags() {
    let cli = Cli::try_parse_from(["zeph", "--vault", "age"]).unwrap();
    assert_eq!(cli.vault.as_deref(), Some("age"));
    assert!(cli.vault_key.is_none());
    assert!(cli.vault_path.is_none());
}

#[test]
fn build_config_ollama_defaults() {
    use crate::init::{WizardState, build_config};

    let state = WizardState {
        provider: Some(ProviderKind::Ollama),
        base_url: Some("http://localhost:11434".into()),
        model: Some("llama3".into()),
        ..WizardState::default()
    };
    let config = build_config(&state);
    assert_eq!(config.llm.effective_provider(), ProviderKind::Ollama);
    assert_eq!(config.llm.effective_model(), "llama3");
    assert!(config.telegram.is_none());
}

#[test]
fn build_config_claude_provider() {
    use crate::init::{WizardState, build_config};

    let state = WizardState {
        provider: Some(ProviderKind::Claude),
        model: Some("claude-sonnet-4-5-20250929".into()),
        api_key: Some("sk-test".into()),
        ..WizardState::default()
    };
    let config = build_config(&state);
    assert_eq!(config.llm.effective_provider(), ProviderKind::Claude);
}

#[test]
fn build_config_compatible_provider() {
    use crate::init::{WizardState, build_config};

    let state = WizardState {
        provider: Some(ProviderKind::Compatible),
        compatible_name: Some("groq".into()),
        base_url: Some("https://api.groq.com/v1".into()),
        model: Some("mixtral".into()),
        ..WizardState::default()
    };
    let config = build_config(&state);
    assert_eq!(config.llm.effective_provider(), ProviderKind::Compatible);
    assert_eq!(config.llm.providers[0].name.as_deref(), Some("groq"));
}

#[test]
fn build_config_telegram_channel() {
    use crate::init::{ChannelChoice, WizardState, build_config};

    let state = WizardState {
        channel: ChannelChoice::Telegram,
        telegram_token: Some("tok".into()),
        telegram_users: vec!["alice".into()],
        ..WizardState::default()
    };
    let config = build_config(&state);
    assert!(config.telegram.is_some());
    assert_eq!(config.telegram.unwrap().allowed_users, vec!["alice"]);
}

#[test]
fn build_config_discord_channel() {
    use crate::init::{ChannelChoice, WizardState, build_config};

    let state = WizardState {
        channel: ChannelChoice::Discord,
        discord_token: Some("tok".into()),
        discord_app_id: Some("123".into()),
        ..WizardState::default()
    };
    let config = build_config(&state);
    assert!(config.discord.is_some());
}

#[test]
fn build_config_slack_channel() {
    use crate::init::{ChannelChoice, WizardState, build_config};

    let state = WizardState {
        channel: ChannelChoice::Slack,
        slack_bot_token: Some("xoxb".into()),
        slack_signing_secret: Some("secret".into()),
        ..WizardState::default()
    };
    let config = build_config(&state);
    assert!(config.slack.is_some());
}

#[test]
fn build_config_vault_age() {
    use crate::init::{WizardState, build_config};

    let state = WizardState {
        vault_backend: "age".into(),
        ..WizardState::default()
    };
    let config = build_config(&state);
    assert_eq!(config.vault.backend, "age");
}

#[test]
fn build_config_semantic_disabled() {
    use crate::init::{WizardState, build_config};

    let state = WizardState {
        semantic_enabled: false,
        ..WizardState::default()
    };
    let config = build_config(&state);
    assert!(!config.memory.semantic.enabled);
}

#[cfg(feature = "a2a")]
#[test]
fn agent_task_processor_construction() {
    use crate::daemon::AgentTaskProcessor;

    let (_, handle) = zeph_core::LoopbackChannel::pair(8);
    let sanitizer = zeph_core::ContentSanitizer::new(&zeph_core::ContentIsolationConfig::default());
    let processor = AgentTaskProcessor {
        loopback_handle: std::sync::Arc::new(tokio::sync::Mutex::new(handle)),
        sanitizer,
    };
    assert!(std::sync::Arc::strong_count(&processor.loopback_handle) == 1);
}

// R-03: VaultCommand CLI parsing
#[test]
fn cli_parse_vault_init() {
    let cli = Cli::try_parse_from(["zeph", "vault", "init"]).unwrap();
    assert!(matches!(
        cli.command,
        Some(Command::Vault {
            command: VaultCommand::Init
        })
    ));
}

#[test]
fn cli_parse_vault_set() {
    let cli = Cli::try_parse_from(["zeph", "vault", "set", "MY_KEY", "MY_VAL"]).unwrap();
    match cli.command {
        Some(Command::Vault {
            command: VaultCommand::Set { key, value },
        }) => {
            assert_eq!(key, "MY_KEY");
            assert_eq!(value, "MY_VAL");
        }
        _ => panic!("expected VaultCommand::Set"),
    }
}

#[test]
fn cli_parse_vault_get() {
    let cli = Cli::try_parse_from(["zeph", "vault", "get", "MY_KEY"]).unwrap();
    match cli.command {
        Some(Command::Vault {
            command: VaultCommand::Get { key },
        }) => assert_eq!(key, "MY_KEY"),
        _ => panic!("expected VaultCommand::Get"),
    }
}

#[test]
fn cli_parse_vault_list() {
    let cli = Cli::try_parse_from(["zeph", "vault", "list"]).unwrap();
    assert!(matches!(
        cli.command,
        Some(Command::Vault {
            command: VaultCommand::List
        })
    ));
}

#[test]
fn cli_parse_vault_rm() {
    let cli = Cli::try_parse_from(["zeph", "vault", "rm", "MY_KEY"]).unwrap();
    match cli.command {
        Some(Command::Vault {
            command: VaultCommand::Rm { key },
        }) => assert_eq!(key, "MY_KEY"),
        _ => panic!("expected VaultCommand::Rm"),
    }
}
