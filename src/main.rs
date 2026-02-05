use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use zeph_channels::telegram::TelegramChannel;
use zeph_core::agent::Agent;
use zeph_core::channel::{Channel, ChannelMessage, CliChannel};
use zeph_core::config::Config;
use zeph_llm::any::AnyProvider;
use zeph_llm::claude::ClaudeProvider;
use zeph_llm::ollama::OllamaProvider;
use zeph_memory::sqlite::SqliteStore;
use zeph_skills::prompt::format_skills_prompt;
use zeph_skills::registry::SkillRegistry;

/// Enum dispatch for runtime channel selection, following the `AnyProvider` pattern.
#[derive(Debug)]
enum AnyChannel {
    Cli(CliChannel),
    Telegram(TelegramChannel),
}

impl Channel for AnyChannel {
    async fn recv(&mut self) -> anyhow::Result<Option<ChannelMessage>> {
        match self {
            Self::Cli(c) => c.recv().await,
            Self::Telegram(c) => c.recv().await,
        }
    }

    async fn send(&mut self, text: &str) -> anyhow::Result<()> {
        match self {
            Self::Cli(c) => c.send(text).await,
            Self::Telegram(c) => c.send(text).await,
        }
    }

    async fn send_typing(&mut self) -> anyhow::Result<()> {
        match self {
            Self::Cli(c) => c.send_typing().await,
            Self::Telegram(c) => c.send_typing().await,
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let config = Config::load(Path::new("config/default.toml"))?;

    let provider = create_provider(&config)?;

    let skill_paths: Vec<PathBuf> = config.skills.paths.iter().map(PathBuf::from).collect();
    let registry = SkillRegistry::load(&skill_paths);
    let skills_prompt = format_skills_prompt(registry.all());

    tracing::info!("loaded {} skill(s)", registry.all().len());

    let channel = create_channel(&config)?;

    if matches!(channel, AnyChannel::Cli(_)) {
        println!("zeph v{}", env!("CARGO_PKG_VERSION"));
    }

    let store = SqliteStore::new(&config.memory.sqlite_path).await?;
    let conversation_id = match store.latest_conversation_id().await? {
        Some(id) => id,
        None => store.create_conversation().await?,
    };

    tracing::info!("conversation id: {conversation_id}");

    let mut agent = Agent::new(provider, channel, &skills_prompt)
        .with_memory(store, conversation_id, config.memory.history_limit);
    agent.load_history().await?;
    agent.run().await
}

fn create_provider(config: &Config) -> anyhow::Result<AnyProvider> {
    match config.llm.provider.as_str() {
        "ollama" => {
            let provider =
                OllamaProvider::new(&config.llm.base_url, config.llm.model.clone());
            Ok(AnyProvider::Ollama(provider))
        }
        "claude" => {
            let cloud = config
                .llm
                .cloud
                .as_ref()
                .context("llm.cloud config section required for Claude provider")?;

            let api_key = std::env::var("ZEPH_CLAUDE_API_KEY")
                .context("ZEPH_CLAUDE_API_KEY env var required for Claude provider")?;

            let provider = ClaudeProvider::new(api_key, cloud.model.clone(), cloud.max_tokens);
            Ok(AnyProvider::Claude(provider))
        }
        other => bail!("unknown LLM provider: {other}"),
    }
}

fn create_channel(config: &Config) -> anyhow::Result<AnyChannel> {
    let token = config
        .telegram
        .as_ref()
        .and_then(|t| t.token.clone());

    if let Some(token) = token {
        let allowed = config
            .telegram
            .as_ref()
            .map_or_else(Vec::new, |t| t.allowed_users.clone());

        let tg = TelegramChannel::new(token, allowed).start()?;
        tracing::info!("running in Telegram mode");
        Ok(AnyChannel::Telegram(tg))
    } else {
        Ok(AnyChannel::Cli(CliChannel::new()))
    }
}
