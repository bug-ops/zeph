use std::path::PathBuf;

use dialoguer::{Confirm, Input, Password, Select};
use zeph_core::config::{
    CompatibleConfig, Config, DiscordConfig, LlmConfig, MemoryConfig, ProviderKind, SemanticConfig,
    SlackConfig, TelegramConfig, VaultConfig,
};

#[derive(Default)]
#[cfg_attr(test, derive(Clone))]
pub(crate) struct WizardState {
    pub(crate) provider: Option<ProviderKind>,
    pub(crate) base_url: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) embedding_model: Option<String>,
    pub(crate) vision_model: Option<String>,
    pub(crate) api_key: Option<String>,
    pub(crate) compatible_name: Option<String>,
    pub(crate) sqlite_path: Option<String>,
    pub(crate) qdrant_url: Option<String>,
    pub(crate) semantic_enabled: bool,
    pub(crate) channel: ChannelChoice,
    pub(crate) telegram_token: Option<String>,
    pub(crate) telegram_users: Vec<String>,
    pub(crate) discord_token: Option<String>,
    pub(crate) discord_app_id: Option<String>,
    pub(crate) slack_bot_token: Option<String>,
    pub(crate) slack_signing_secret: Option<String>,
    pub(crate) vault_backend: String,
}

#[derive(Default, Clone, Copy)]
pub(crate) enum ChannelChoice {
    #[default]
    Cli,
    Telegram,
    Discord,
    Slack,
}

pub fn run(output: Option<PathBuf>) -> anyhow::Result<()> {
    println!("zeph init - configuration wizard\n");

    let mut state = WizardState {
        vault_backend: "env".into(),
        semantic_enabled: true,
        ..WizardState::default()
    };

    step_llm(&mut state)?;
    step_memory(&mut state)?;
    step_channel(&mut state)?;
    step_vault(&mut state)?;
    step_review_and_write(&state, output)?;

    Ok(())
}

fn step_llm(state: &mut WizardState) -> anyhow::Result<()> {
    println!("== Step 1/5: LLM Provider ==\n");

    let providers = [
        "Ollama (local)",
        "Claude (API)",
        "OpenAI (API)",
        "Compatible (custom)",
    ];
    let selection = Select::new()
        .with_prompt("Select LLM provider")
        .items(&providers)
        .default(0)
        .interact()?;

    match selection {
        0 => {
            state.provider = Some(ProviderKind::Ollama);
            state.base_url = Some(
                Input::new()
                    .with_prompt("Ollama base URL")
                    .default("http://localhost:11434".into())
                    .interact_text()?,
            );
            state.model = Some(
                Input::new()
                    .with_prompt("Model name")
                    .default("mistral:7b".into())
                    .interact_text()?,
            );
        }
        1 => {
            state.provider = Some(ProviderKind::Claude);
            state.api_key = Some(Password::new().with_prompt("Claude API key").interact()?);
            state.model = Some(
                Input::new()
                    .with_prompt("Model name")
                    .default("claude-sonnet-4-5-20250929".into())
                    .interact_text()?,
            );
        }
        2 => {
            state.provider = Some(ProviderKind::OpenAi);
            state.api_key = Some(Password::new().with_prompt("OpenAI API key").interact()?);
            state.base_url = Some(
                Input::new()
                    .with_prompt("Base URL")
                    .default("https://api.openai.com/v1".into())
                    .interact_text()?,
            );
            state.model = Some(
                Input::new()
                    .with_prompt("Model name")
                    .default("gpt-4o".into())
                    .interact_text()?,
            );
        }
        3 => {
            state.provider = Some(ProviderKind::Compatible);
            state.compatible_name =
                Some(Input::new().with_prompt("Provider name").interact_text()?);
            state.base_url = Some(Input::new().with_prompt("Base URL").interact_text()?);
            state.model = Some(Input::new().with_prompt("Model name").interact_text()?);
            state.api_key = Some(
                Password::new()
                    .with_prompt("API key (leave empty if none)")
                    .allow_empty_password(true)
                    .interact()?,
            );
        }
        _ => unreachable!(),
    }

    state.embedding_model = Some(
        Input::new()
            .with_prompt("Embedding model")
            .default("qwen3-embedding".into())
            .interact_text()?,
    );

    if state.provider == Some(ProviderKind::Ollama) {
        let use_vision = Confirm::new()
            .with_prompt("Use a separate model for vision (image input)?")
            .default(false)
            .interact()?;
        if use_vision {
            state.vision_model = Some(
                Input::new()
                    .with_prompt("Vision model name (e.g. llava:13b)")
                    .interact_text()?,
            );
        }
    }

    println!();
    Ok(())
}

fn step_memory(state: &mut WizardState) -> anyhow::Result<()> {
    println!("== Step 2/5: Memory ==\n");

    state.sqlite_path = Some(
        Input::new()
            .with_prompt("SQLite database path")
            .default("./data/zeph.db".into())
            .interact_text()?,
    );

    state.semantic_enabled = Confirm::new()
        .with_prompt("Enable semantic memory (requires Qdrant)?")
        .default(true)
        .interact()?;

    if state.semantic_enabled {
        state.qdrant_url = Some(
            Input::new()
                .with_prompt("Qdrant URL")
                .default("http://localhost:6334".into())
                .interact_text()?,
        );
    }

    println!();
    Ok(())
}

fn step_channel(state: &mut WizardState) -> anyhow::Result<()> {
    println!("== Step 3/5: Channel ==\n");

    let channels = ["CLI only (default)", "Telegram", "Discord", "Slack"];
    let selection = Select::new()
        .with_prompt("Select communication channel")
        .items(&channels)
        .default(0)
        .interact()?;

    match selection {
        0 => state.channel = ChannelChoice::Cli,
        1 => {
            state.channel = ChannelChoice::Telegram;
            state.telegram_token = Some(
                Password::new()
                    .with_prompt("Telegram bot token")
                    .interact()?,
            );
            let users: String = Input::new()
                .with_prompt("Allowed usernames (comma-separated)")
                .default(String::new())
                .interact_text()?;
            state.telegram_users = users
                .split(',')
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .collect();
        }
        2 => {
            state.channel = ChannelChoice::Discord;
            state.discord_token = Some(
                Password::new()
                    .with_prompt("Discord bot token")
                    .interact()?,
            );
            state.discord_app_id = Some(
                Input::new()
                    .with_prompt("Discord application ID")
                    .interact_text()?,
            );
        }
        3 => {
            state.channel = ChannelChoice::Slack;
            state.slack_bot_token =
                Some(Password::new().with_prompt("Slack bot token").interact()?);
            state.slack_signing_secret = Some(
                Password::new()
                    .with_prompt("Slack signing secret")
                    .interact()?,
            );
        }
        _ => unreachable!(),
    }

    println!();
    Ok(())
}

fn step_vault(state: &mut WizardState) -> anyhow::Result<()> {
    println!("== Step 4/5: Secrets Backend ==\n");

    let backends = ["env (environment variables)", "age (encrypted file)"];
    let selection = Select::new()
        .with_prompt("Select secrets backend")
        .items(&backends)
        .default(0)
        .interact()?;

    state.vault_backend = match selection {
        0 => "env".into(),
        1 => "age".into(),
        _ => unreachable!(),
    };

    println!();
    Ok(())
}

pub(crate) fn build_config(state: &WizardState) -> Config {
    let mut config = Config::default();
    let provider = state.provider.unwrap_or(ProviderKind::Ollama);

    config.llm = LlmConfig {
        provider,
        base_url: state
            .base_url
            .clone()
            .unwrap_or_else(|| "http://localhost:11434".into()),
        model: state.model.clone().unwrap_or_else(|| "mistral:7b".into()),
        embedding_model: state
            .embedding_model
            .clone()
            .unwrap_or_else(|| "qwen3-embedding".into()),
        cloud: None,
        openai: None,
        candle: None,
        orchestrator: None,
        compatible: if provider == ProviderKind::Compatible {
            Some(vec![CompatibleConfig {
                name: state
                    .compatible_name
                    .clone()
                    .unwrap_or_else(|| "custom".into()),
                base_url: state.base_url.clone().unwrap_or_default(),
                model: state.model.clone().unwrap_or_default(),
                max_tokens: 4096,
                embedding_model: None,
            }])
        } else {
            None
        },
        router: None,
        stt: None,
        vision_model: state.vision_model.clone().filter(|s| !s.is_empty()),
    };

    config.memory = MemoryConfig {
        sqlite_path: state
            .sqlite_path
            .clone()
            .unwrap_or_else(|| "./data/zeph.db".into()),
        qdrant_url: state
            .qdrant_url
            .clone()
            .unwrap_or_else(|| "http://localhost:6334".into()),
        semantic: SemanticConfig {
            enabled: state.semantic_enabled,
            ..SemanticConfig::default()
        },
        ..config.memory
    };

    match state.channel {
        ChannelChoice::Cli => {}
        ChannelChoice::Telegram => {
            config.telegram = Some(TelegramConfig {
                token: None,
                allowed_users: state.telegram_users.clone(),
            });
        }
        ChannelChoice::Discord => {
            config.discord = Some(DiscordConfig {
                token: None,
                application_id: state.discord_app_id.clone(),
                allowed_user_ids: vec![],
                allowed_role_ids: vec![],
                allowed_channel_ids: vec![],
            });
        }
        ChannelChoice::Slack => {
            config.slack = Some(SlackConfig {
                bot_token: None,
                signing_secret: None,
                webhook_host: "127.0.0.1".into(),
                port: 3000,
                allowed_user_ids: vec![],
                allowed_channel_ids: vec![],
            });
        }
    }

    config.vault = VaultConfig {
        backend: state.vault_backend.clone(),
    };

    config
}

fn step_review_and_write(state: &WizardState, output: Option<PathBuf>) -> anyhow::Result<()> {
    println!("== Step 5/5: Review & Write ==\n");

    let config = build_config(state);
    let toml_str = toml::to_string_pretty(&config)?;

    println!("--- Generated config ---");
    println!("{toml_str}");
    println!("------------------------\n");

    let default_path = PathBuf::from("config.toml");
    let path = output.unwrap_or_else(|| {
        Input::new()
            .with_prompt("Write config to")
            .default(default_path.display().to_string())
            .interact_text()
            .map(PathBuf::from)
            .unwrap_or(default_path)
    });

    if path.exists() {
        let overwrite = Confirm::new()
            .with_prompt(format!("{} already exists. Overwrite?", path.display()))
            .default(false)
            .interact()?;
        if !overwrite {
            println!("Aborted.");
            return Ok(());
        }
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, &toml_str)?;
    println!("Config written to {}", path.display());

    print_secrets_instructions(state);
    print_next_steps(&path);

    Ok(())
}

fn print_secrets_instructions(state: &WizardState) {
    let mut secrets = Vec::new();

    if let Some(ref key) = state.api_key
        && !key.is_empty()
    {
        let var = match state.provider {
            Some(ProviderKind::Claude) => "ZEPH_CLAUDE_API_KEY",
            Some(ProviderKind::OpenAi) => "ZEPH_OPENAI_API_KEY",
            Some(ProviderKind::Compatible) => {
                let name = state
                    .compatible_name
                    .as_deref()
                    .unwrap_or("custom")
                    .to_uppercase();
                // Leak is fine here: runs once at CLI exit
                let var = format!("ZEPH_COMPATIBLE_{name}_API_KEY");
                secrets.push(var);
                secrets.last().map(String::as_str).unwrap_or_default()
            }
            _ => "",
        };
        if !var.is_empty() && !secrets.iter().any(|s| s == var) {
            secrets.push(var.to_owned());
        }
    }

    if state.telegram_token.is_some() {
        secrets.push("ZEPH_TELEGRAM_TOKEN".into());
    }
    if state.discord_token.is_some() {
        secrets.push("ZEPH_DISCORD_TOKEN".into());
    }
    if state.slack_bot_token.is_some() {
        secrets.push("ZEPH_SLACK_BOT_TOKEN".into());
    }
    if state.slack_signing_secret.is_some() {
        secrets.push("ZEPH_SLACK_SIGNING_SECRET".into());
    }

    if secrets.is_empty() {
        return;
    }

    if state.vault_backend == "env" {
        println!("\nAdd the following to your shell profile:");
        for var in &secrets {
            println!("  export {var}=\"<your-secret>\"");
        }
    } else {
        println!("\nStore secrets via: zeph vault set <KEY> <VALUE>");
        println!("Required keys: {}", secrets.join(", "));
    }
}

fn print_next_steps(path: &std::path::Path) {
    println!("\nNext steps:");
    println!("  1. Set required environment variables (see above)");
    println!("  2. Run: zeph --config {}", path.display());
    println!("  3. Or with TUI: zeph --tui --config {}", path.display());
}
