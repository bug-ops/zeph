use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use tokio::sync::watch;
use zeph_channels::telegram::TelegramChannel;
use zeph_core::agent::Agent;
use zeph_core::channel::{Channel, ChannelMessage, CliChannel};
use zeph_core::config::Config;
use zeph_llm::any::AnyProvider;
use zeph_llm::claude::ClaudeProvider;
use zeph_llm::ollama::OllamaProvider;
use zeph_llm::provider::LlmProvider;
use zeph_memory::semantic::SemanticMemory;
use zeph_skills::matcher::{SkillMatcher, SkillMatcherBackend};
use zeph_skills::qdrant_matcher::QdrantSkillMatcher;
use zeph_skills::registry::SkillRegistry;
use zeph_skills::watcher::SkillWatcher;
use zeph_tools::{CompositeExecutor, ShellExecutor, WebScrapeExecutor};

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

    async fn send_chunk(&mut self, chunk: &str) -> anyhow::Result<()> {
        match self {
            Self::Cli(c) => c.send_chunk(chunk).await,
            Self::Telegram(c) => c.send_chunk(chunk).await,
        }
    }

    async fn flush_chunks(&mut self) -> anyhow::Result<()> {
        match self {
            Self::Cli(c) => c.flush_chunks().await,
            Self::Telegram(c) => c.flush_chunks().await,
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

    health_check(&provider).await;

    let skill_paths: Vec<PathBuf> = config.skills.paths.iter().map(PathBuf::from).collect();
    let registry = SkillRegistry::load(&skill_paths);
    let skills = registry.into_skills();

    let memory = SemanticMemory::new(
        &config.memory.sqlite_path,
        &config.memory.qdrant_url,
        provider.clone(),
        &config.llm.embedding_model,
    )
    .await?;

    if config.memory.semantic.enabled && memory.has_qdrant() {
        tracing::info!("semantic memory enabled, Qdrant connected");
    }

    let matcher = create_skill_matcher(&config, &provider, &skills, &memory).await;
    let skill_count = skills.len();
    if matcher.is_some() {
        tracing::info!("skill matcher initialized for {skill_count} skill(s)");
    } else {
        tracing::info!("skill matcher unavailable, using all {skill_count} skill(s)");
    }

    let channel = create_channel(&config)?;

    if matches!(channel, AnyChannel::Cli(_)) {
        println!("zeph v{}", env!("CARGO_PKG_VERSION"));
    }

    let conversation_id = match memory.sqlite().latest_conversation_id().await? {
        Some(id) => id,
        None => memory.sqlite().create_conversation().await?,
    };

    tracing::info!("conversation id: {conversation_id}");

    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    tokio::spawn(async move {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::error!("failed to listen for ctrl-c: {e:#}");
            return;
        }
        tracing::info!("received shutdown signal");
        let _ = shutdown_tx.send(true);
    });

    let shell_executor = ShellExecutor::new(&config.tools.shell);
    let scrape_executor = WebScrapeExecutor::new(&config.tools.scrape);

    #[cfg(feature = "mcp")]
    let (tool_executor, mcp_tools) = {
        let mcp_manager = std::sync::Arc::new(create_mcp_manager(&config));
        let mcp_tools = mcp_manager.connect_all().await;
        tracing::info!("discovered {} MCP tool(s)", mcp_tools.len());

        let mcp_executor = zeph_mcp::McpToolExecutor::new(mcp_manager.clone());
        let base_executor = CompositeExecutor::new(shell_executor, scrape_executor);
        let executor = CompositeExecutor::new(base_executor, mcp_executor);

        (executor, mcp_tools)
    };

    #[cfg(not(feature = "mcp"))]
    let tool_executor = CompositeExecutor::new(shell_executor, scrape_executor);

    let (reload_tx, reload_rx) = tokio::sync::mpsc::channel(4);
    let _watcher = match SkillWatcher::start(&skill_paths, reload_tx) {
        Ok(w) => {
            tracing::info!("skill watcher started");
            Some(w)
        }
        Err(e) => {
            tracing::warn!("skill watcher unavailable: {e:#}");
            None
        }
    };

    #[cfg(feature = "a2a")]
    if config.a2a.enabled {
        spawn_a2a_server(&config, shutdown_rx.clone());
    }

    #[cfg(feature = "mcp")]
    let mcp_registry = create_mcp_registry(&config, &provider, &mcp_tools).await;

    let agent = Agent::new(
        provider,
        channel,
        skills,
        matcher,
        config.skills.max_active_skills,
        tool_executor,
    )
    .with_embedding_model(config.llm.embedding_model.clone())
    .with_skill_reload(skill_paths, reload_rx)
    .with_memory(
        memory,
        conversation_id,
        config.memory.history_limit,
        config.memory.semantic.recall_limit,
        config.memory.summarization_threshold,
    )
    .with_shutdown(shutdown_rx);

    #[cfg(feature = "mcp")]
    let agent = agent.with_mcp(mcp_tools, mcp_registry);

    #[cfg(feature = "self-learning")]
    let agent = agent.with_learning(config.skills.learning);

    let mut agent = agent;

    agent.load_history().await?;
    agent.run().await
}

async fn health_check(provider: &AnyProvider) {
    match provider {
        AnyProvider::Ollama(ollama) => match ollama.health_check().await {
            Ok(()) => tracing::info!("ollama health check passed"),
            Err(e) => tracing::warn!("ollama health check failed: {e:#}"),
        },
        #[cfg(feature = "candle")]
        AnyProvider::Candle(candle) => {
            tracing::info!("candle provider loaded, device: {}", candle.device_name());
        }
        #[cfg(feature = "orchestrator")]
        AnyProvider::Orchestrator(orch) => {
            for (name, p) in orch.providers() {
                tracing::info!(
                    "orchestrator sub-provider '{name}': {}",
                    zeph_llm::provider::LlmProvider::name(p)
                );
            }
        }
        _ => {}
    }
}

async fn create_skill_matcher(
    config: &Config,
    provider: &AnyProvider,
    skills: &[zeph_skills::loader::Skill],
    memory: &SemanticMemory<AnyProvider>,
) -> Option<SkillMatcherBackend> {
    let p = provider.clone();
    let embed_fn = move |text: &str| -> zeph_skills::matcher::EmbedFuture {
        let owned = text.to_owned();
        let p = p.clone();
        Box::pin(async move { p.embed(&owned).await })
    };

    if config.memory.semantic.enabled && memory.has_qdrant() {
        match QdrantSkillMatcher::new(&config.memory.qdrant_url) {
            Ok(mut qm) => match qm
                .sync(skills, &config.llm.embedding_model, &embed_fn)
                .await
            {
                Ok(_) => return Some(SkillMatcherBackend::Qdrant(qm)),
                Err(e) => {
                    tracing::warn!("Qdrant skill sync failed, falling back to in-memory: {e:#}");
                }
            },
            Err(e) => {
                tracing::warn!("Qdrant client creation failed, falling back to in-memory: {e:#}");
            }
        }
    }

    SkillMatcher::new(skills, &embed_fn)
        .await
        .map(SkillMatcherBackend::InMemory)
}

fn create_provider(config: &Config) -> anyhow::Result<AnyProvider> {
    match config.llm.provider.as_str() {
        "ollama" => {
            let provider = OllamaProvider::new(
                &config.llm.base_url,
                config.llm.model.clone(),
                config.llm.embedding_model.clone(),
            );
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
        #[cfg(feature = "candle")]
        "candle" => {
            let candle_cfg = config
                .llm
                .candle
                .as_ref()
                .context("llm.candle config section required for candle provider")?;

            let source = match candle_cfg.source.as_str() {
                "local" => zeph_llm::candle_provider::loader::ModelSource::Local {
                    path: std::path::PathBuf::from(&candle_cfg.local_path),
                },
                _ => zeph_llm::candle_provider::loader::ModelSource::HuggingFace {
                    repo_id: config.llm.model.clone(),
                    filename: candle_cfg.filename.clone(),
                },
            };

            let template = zeph_llm::candle_provider::template::ChatTemplate::parse_str(
                &candle_cfg.chat_template,
            );
            let gen_config = zeph_llm::candle_provider::generate::GenerationConfig {
                temperature: candle_cfg.generation.temperature,
                top_p: candle_cfg.generation.top_p,
                top_k: candle_cfg.generation.top_k,
                max_tokens: candle_cfg.generation.capped_max_tokens(),
                seed: candle_cfg.generation.seed,
                repeat_penalty: candle_cfg.generation.repeat_penalty,
                repeat_last_n: candle_cfg.generation.repeat_last_n,
            };

            let device = select_device(&candle_cfg.device)?;

            let provider = zeph_llm::candle_provider::CandleProvider::new(
                &source,
                template,
                gen_config,
                candle_cfg.embedding_repo.as_deref(),
                device,
            )?;
            Ok(AnyProvider::Candle(provider))
        }
        #[cfg(feature = "orchestrator")]
        "orchestrator" => {
            let orch = build_orchestrator(config)?;
            Ok(AnyProvider::Orchestrator(Box::new(orch)))
        }
        other => bail!("unknown LLM provider: {other}"),
    }
}

#[cfg(feature = "a2a")]
fn spawn_a2a_server(config: &Config, shutdown_rx: watch::Receiver<bool>) {
    let public_url = if config.a2a.public_url.is_empty() {
        format!("http://{}:{}", config.a2a.host, config.a2a.port)
    } else {
        config.a2a.public_url.clone()
    };

    let card =
        zeph_a2a::AgentCardBuilder::new(&config.agent.name, &public_url, env!("CARGO_PKG_VERSION"))
            .description("Zeph AI agent")
            .streaming(true)
            .build();

    let processor: std::sync::Arc<dyn zeph_a2a::TaskProcessor> =
        std::sync::Arc::new(EchoTaskProcessor);
    let a2a_server = zeph_a2a::A2aServer::new(
        card,
        processor,
        &config.a2a.host,
        config.a2a.port,
        shutdown_rx,
    )
    .with_auth(config.a2a.auth_token.clone())
    .with_rate_limit(config.a2a.rate_limit);

    tracing::info!(
        "A2A server spawned on {}:{}",
        config.a2a.host,
        config.a2a.port
    );

    tokio::spawn(async move {
        if let Err(e) = a2a_server.serve().await {
            tracing::error!("A2A server error: {e:#}");
        }
    });
}

#[cfg(feature = "a2a")]
struct EchoTaskProcessor;

#[cfg(feature = "a2a")]
impl zeph_a2a::TaskProcessor for EchoTaskProcessor {
    fn process(
        &self,
        _task_id: String,
        message: zeph_a2a::Message,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<zeph_a2a::ProcessResult, zeph_a2a::A2aError>>
                + Send,
        >,
    > {
        Box::pin(async move {
            let text = message.text_content().unwrap_or("").to_owned();
            Ok(zeph_a2a::ProcessResult {
                response: zeph_a2a::Message {
                    role: zeph_a2a::Role::Agent,
                    parts: vec![zeph_a2a::Part::text(format!("echo: {text}"))],
                    message_id: None,
                    task_id: None,
                    context_id: None,
                    metadata: None,
                },
                artifacts: vec![],
            })
        })
    }
}

#[cfg(feature = "mcp")]
fn create_mcp_manager(config: &Config) -> zeph_mcp::McpManager {
    let entries: Vec<zeph_mcp::ServerEntry> = config
        .mcp
        .servers
        .iter()
        .map(|s| zeph_mcp::ServerEntry {
            id: s.id.clone(),
            command: s.command.clone(),
            args: s.args.clone(),
            env: s.env.clone(),
            timeout: std::time::Duration::from_secs(s.timeout),
        })
        .collect();
    zeph_mcp::McpManager::new(entries)
}

#[cfg(feature = "mcp")]
async fn create_mcp_registry(
    config: &Config,
    provider: &AnyProvider,
    mcp_tools: &[zeph_mcp::McpTool],
) -> Option<zeph_mcp::McpToolRegistry> {
    if !config.memory.semantic.enabled {
        return None;
    }
    match zeph_mcp::McpToolRegistry::new(&config.memory.qdrant_url) {
        Ok(mut reg) => {
            let p = provider.clone();
            let embed_fn = move |text: &str| -> zeph_mcp::registry::EmbedFuture {
                let owned = text.to_owned();
                let p = p.clone();
                Box::pin(async move { p.embed(&owned).await })
            };
            if let Err(e) = reg
                .sync(mcp_tools, &config.llm.embedding_model, &embed_fn)
                .await
            {
                tracing::warn!("MCP tool embedding sync failed: {e:#}");
            }
            Some(reg)
        }
        Err(e) => {
            tracing::warn!("MCP tool registry unavailable: {e:#}");
            None
        }
    }
}

#[cfg(feature = "candle")]
fn select_device(preference: &str) -> anyhow::Result<zeph_llm::candle_provider::Device> {
    match preference {
        "metal" => {
            #[cfg(feature = "metal")]
            return Ok(zeph_llm::candle_provider::Device::new_metal(0)?);
            #[cfg(not(feature = "metal"))]
            bail!("candle compiled without metal feature");
        }
        "cuda" => {
            #[cfg(feature = "cuda")]
            return Ok(zeph_llm::candle_provider::Device::new_cuda(0)?);
            #[cfg(not(feature = "cuda"))]
            bail!("candle compiled without cuda feature");
        }
        "auto" => {
            #[cfg(feature = "metal")]
            if let Ok(device) = zeph_llm::candle_provider::Device::new_metal(0) {
                return Ok(device);
            }
            #[cfg(feature = "cuda")]
            if let Ok(device) = zeph_llm::candle_provider::Device::new_cuda(0) {
                return Ok(device);
            }
            Ok(zeph_llm::candle_provider::Device::Cpu)
        }
        _ => Ok(zeph_llm::candle_provider::Device::Cpu),
    }
}

#[cfg(feature = "orchestrator")]
fn build_orchestrator(
    config: &Config,
) -> anyhow::Result<zeph_llm::orchestrator::ModelOrchestrator> {
    use std::collections::HashMap;
    use zeph_llm::orchestrator::{ModelOrchestrator, SubProvider, TaskType};

    let orch_cfg = config
        .llm
        .orchestrator
        .as_ref()
        .context("llm.orchestrator config section required for orchestrator provider")?;

    let mut providers = HashMap::new();
    for (name, pcfg) in &orch_cfg.providers {
        let provider = match pcfg.provider_type.as_str() {
            "ollama" => {
                let model = pcfg.model.as_deref().unwrap_or(&config.llm.model);
                SubProvider::Ollama(OllamaProvider::new(
                    &config.llm.base_url,
                    model.to_owned(),
                    config.llm.embedding_model.clone(),
                ))
            }
            "claude" => {
                let cloud = config
                    .llm
                    .cloud
                    .as_ref()
                    .context("llm.cloud config required for claude sub-provider")?;
                let api_key = std::env::var("ZEPH_CLAUDE_API_KEY")
                    .context("ZEPH_CLAUDE_API_KEY required for claude sub-provider")?;
                let model = pcfg.model.as_deref().unwrap_or(&cloud.model);
                SubProvider::Claude(ClaudeProvider::new(
                    api_key,
                    model.to_owned(),
                    cloud.max_tokens,
                ))
            }
            #[cfg(feature = "candle")]
            "candle" => {
                let candle_cfg = config
                    .llm
                    .candle
                    .as_ref()
                    .context("llm.candle config required for candle sub-provider")?;
                let source = match candle_cfg.source.as_str() {
                    "local" => zeph_llm::candle_provider::loader::ModelSource::Local {
                        path: std::path::PathBuf::from(&candle_cfg.local_path),
                    },
                    _ => zeph_llm::candle_provider::loader::ModelSource::HuggingFace {
                        repo_id: pcfg
                            .model
                            .clone()
                            .unwrap_or_else(|| config.llm.model.clone()),
                        filename: candle_cfg.filename.clone(),
                    },
                };
                let template = zeph_llm::candle_provider::template::ChatTemplate::parse_str(
                    &candle_cfg.chat_template,
                );
                let device_pref = pcfg.device.as_deref().unwrap_or(&candle_cfg.device);
                let device = select_device(device_pref)?;
                let gen_config = zeph_llm::candle_provider::generate::GenerationConfig {
                    temperature: candle_cfg.generation.temperature,
                    top_p: candle_cfg.generation.top_p,
                    top_k: candle_cfg.generation.top_k,
                    max_tokens: candle_cfg.generation.capped_max_tokens(),
                    seed: candle_cfg.generation.seed,
                    repeat_penalty: candle_cfg.generation.repeat_penalty,
                    repeat_last_n: candle_cfg.generation.repeat_last_n,
                };
                let candle_provider = zeph_llm::candle_provider::CandleProvider::new(
                    &source,
                    template,
                    gen_config,
                    candle_cfg.embedding_repo.as_deref(),
                    device,
                )?;
                SubProvider::Candle(candle_provider)
            }
            other => bail!("unknown orchestrator sub-provider type: {other}"),
        };
        providers.insert(name.clone(), provider);
    }

    let mut routes = HashMap::new();
    for (task_str, chain) in &orch_cfg.routes {
        let task = TaskType::parse_str(task_str);
        routes.insert(task, chain.clone());
    }

    ModelOrchestrator::new(
        routes,
        providers,
        orch_cfg.default.clone(),
        orch_cfg.embed.clone(),
    )
}

fn create_channel(config: &Config) -> anyhow::Result<AnyChannel> {
    let token = config.telegram.as_ref().and_then(|t| t.token.clone());

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
