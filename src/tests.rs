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
        skills: zeph_core::config::ChannelSkillsConfig::default(),
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
        skills: zeph_core::config::ChannelSkillsConfig::default(),
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
        skills: zeph_core::config::ChannelSkillsConfig::default(),
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
        skills: zeph_core::config::ChannelSkillsConfig::default(),
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
        skills: zeph_core::config::ChannelSkillsConfig::default(),
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
        drain_timeout: std::time::Duration::from_millis(30_000),
    };
    assert!(std::sync::Arc::strong_count(&processor.loopback_handle) == 1);
}

// Fix #2302: stale events left in output_rx after a completed request must be drained
// so they do not bleed into the next request as a false leading Flush/FullMessage.
#[cfg(feature = "a2a")]
#[tokio::test]
async fn loopback_stale_flush_drained_after_full_message() {
    use zeph_core::LoopbackChannel;
    use zeph_core::LoopbackEvent;

    let (_channel, mut handle) = LoopbackChannel::pair(8);

    // Simulate: agent sends FullMessage (terminates the recv loop), then emits a
    // stale Flush (e.g. emitted by a different code path after the turn ends).
    handle.output_rx.try_recv().unwrap_err(); // channel is empty initially

    // Pre-load the channel as the agent loop would: FullMessage followed by stale Flush.
    // We drive the output_tx side via a separate sender cloned from the pair internals.
    // Because LoopbackHandle owns output_rx (not output_tx), we simulate by sending
    // through a fresh mpsc pair that mirrors the drain logic directly.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<LoopbackEvent>(8);
    tx.send(LoopbackEvent::FullMessage("hello".to_owned()))
        .await
        .unwrap();
    tx.send(LoopbackEvent::Flush).await.unwrap();

    // Consume up to and including FullMessage (mirrors the recv loop in process()).
    let mut got_terminal = false;
    while let Some(event) = rx.recv().await {
        match event {
            LoopbackEvent::FullMessage(_) | LoopbackEvent::Flush => {
                got_terminal = true;
                break;
            }
            _ => {}
        }
    }
    assert!(got_terminal);

    // Drain stale events (mirrors the fix in src/daemon.rs).
    let mut drained = 0usize;
    while rx.try_recv().is_ok() {
        drained += 1;
    }

    // The stale Flush must have been drained.
    assert_eq!(drained, 1, "expected exactly one stale event to be drained");

    // Channel is now empty — a subsequent request would not see the stale Flush.
    assert!(rx.try_recv().is_err());
}

// Fix #2326: drain-until-Flush guarantees no tail event leaks into the next request.
// Simulates the race: agent emits FullMessage (exits recv loop), then asynchronously
// emits Usage + Flush. The drain loop must consume both before process() returns.
#[cfg(feature = "a2a")]
#[tokio::test]
async fn a2a_response_shift_drain_until_flush_prevents_leak() {
    use zeph_core::LoopbackEvent;

    // Test the drain logic directly using a channel pair.
    // Sequence: FullMessage → Usage (tail) → Flush (tail, arrives with delay).
    let (tx, mut rx) = tokio::sync::mpsc::channel::<LoopbackEvent>(8);

    tx.send(LoopbackEvent::FullMessage("req1-response".into()))
        .await
        .unwrap();

    // Tail events arrive asynchronously after the primary response — models the race.
    let tx2 = tx.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let _ = tx2
            .send(LoopbackEvent::Usage {
                input_tokens: 10,
                output_tokens: 5,
                context_window: 8192,
            })
            .await;
        let _ = tx2.send(LoopbackEvent::Flush).await;
    });
    drop(tx);

    // Recv loop: exit on FullMessage (mirrors src/daemon.rs process()).
    let mut exited_on_flush = false;
    let mut response = String::new();
    while let Some(event) = rx.recv().await {
        match event {
            LoopbackEvent::Flush => {
                exited_on_flush = true;
                break;
            }
            LoopbackEvent::FullMessage(text) => {
                response = text;
                break;
            }
            _ => {}
        }
    }
    assert_eq!(response, "req1-response");

    // Drain until Flush — the fix for #2326.
    if !exited_on_flush {
        loop {
            match rx.recv().await {
                Some(LoopbackEvent::Flush) | None => break,
                Some(_) => {}
            }
        }
    }

    // Channel must be empty — stale events must not leak into the next request.
    assert!(
        rx.try_recv().is_err(),
        "channel must be empty after drain; stale events would shift next response"
    );
}

// Fix #2329: drain loop must complete normally when Flush arrives within the timeout.
#[cfg(feature = "a2a")]
#[tokio::test]
async fn a2a_drain_completes_on_flush_within_timeout() {
    use zeph_core::LoopbackEvent;

    let (tx, mut rx) = tokio::sync::mpsc::channel::<LoopbackEvent>(8);

    // Simulate tail events arriving after FullMessage — typical agent turn sequence.
    let tx2 = tx.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let _ = tx2
            .send(LoopbackEvent::Usage {
                input_tokens: 10,
                output_tokens: 5,
                context_window: 8192,
            })
            .await;
        let _ = tx2.send(LoopbackEvent::Flush).await;
    });
    drop(tx);

    // Drain loop with a generous timeout — must complete before it expires.
    let drain = async {
        loop {
            match rx.recv().await {
                Some(LoopbackEvent::Flush) | None => break,
                Some(_) => {}
            }
        }
    };
    let result = tokio::time::timeout(std::time::Duration::from_millis(500), drain).await;
    assert!(
        result.is_ok(),
        "drain must complete before timeout on normal Flush"
    );
}

// Fix #2329: drain loop must break on timeout when Flush is never sent (e.g. agent panic).
#[cfg(feature = "a2a")]
#[tokio::test]
async fn a2a_drain_times_out_when_flush_never_arrives() {
    use zeph_core::LoopbackEvent;

    let (tx, mut rx) = tokio::sync::mpsc::channel::<LoopbackEvent>(8);

    // Keep sender alive but never send Flush — models a panicked agent holding the Arc.
    let _keep_tx = tx;

    // Drain loop with a very short timeout — must expire.
    let drain = async {
        loop {
            match rx.recv().await {
                Some(LoopbackEvent::Flush) | None => break,
                Some(_) => {}
            }
        }
    };
    let result = tokio::time::timeout(std::time::Duration::from_millis(50), drain).await;
    assert!(
        result.is_err(),
        "drain must time out when Flush is never sent"
    );
}

// Fix #2295: stale PID detection — read_pid_file + is_process_alive roundtrip.
#[test]
fn stale_pid_detection_dead_process() {
    use zeph_core::daemon::{is_process_alive, read_pid_file, remove_pid_file};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("stale.pid");
    let path_str = path.to_string_lossy().to_string();

    // Write a PID that is guaranteed to be dead (u32::MAX).
    std::fs::write(&path, u32::MAX.to_string()).unwrap();

    let pid = read_pid_file(&path_str).expect("should read PID");
    assert!(!is_process_alive(pid), "u32::MAX must not be alive");

    remove_pid_file(&path_str).unwrap();
    assert!(!path.exists());
}

#[test]
fn stale_pid_detection_live_process() {
    use zeph_core::daemon::{is_process_alive, read_pid_file, remove_pid_file, write_pid_file};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("live.pid");
    let path_str = path.to_string_lossy().to_string();

    write_pid_file(&path_str).unwrap();

    let pid = read_pid_file(&path_str).expect("should read PID");
    assert_eq!(pid, std::process::id());
    assert!(is_process_alive(pid), "current process must be alive");

    remove_pid_file(&path_str).unwrap();
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
