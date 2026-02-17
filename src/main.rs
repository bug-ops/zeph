use std::path::{Path, PathBuf};
#[cfg(feature = "tui")]
use std::time::Duration;

use anyhow::{Context, bail};
use tokio::sync::watch;
use zeph_channels::CliChannel;
#[cfg(feature = "discord")]
use zeph_channels::discord::DiscordChannel;
#[cfg(feature = "slack")]
use zeph_channels::slack::SlackChannel;
use zeph_channels::telegram::TelegramChannel;
use zeph_core::agent::Agent;
#[cfg(feature = "tui")]
use zeph_core::channel::{Channel, ChannelError, ChannelMessage};
use zeph_core::config::{Config, ProviderKind};
use zeph_core::config_watcher::ConfigWatcher;
use zeph_core::cost::CostTracker;
#[cfg(feature = "vault-age")]
use zeph_core::vault::AgeVaultProvider;
use zeph_core::vault::{EnvVaultProvider, VaultProvider};
#[cfg(feature = "index")]
use zeph_index::{
    indexer::{CodeIndexer, IndexerConfig},
    retriever::{CodeRetriever, RetrievalConfig},
    store::CodeStore,
    watcher::IndexWatcher,
};
use zeph_llm::any::AnyProvider;
use zeph_llm::claude::ClaudeProvider;
#[cfg(feature = "compatible")]
use zeph_llm::compatible::CompatibleProvider;
use zeph_llm::ollama::OllamaProvider;
#[cfg(feature = "openai")]
use zeph_llm::openai::OpenAiProvider;
use zeph_llm::provider::LlmProvider;
#[cfg(feature = "router")]
use zeph_llm::router::RouterProvider;
use zeph_memory::semantic::SemanticMemory;
use zeph_skills::loader::SkillMeta;
use zeph_skills::matcher::{SkillMatcher, SkillMatcherBackend};
#[cfg(feature = "qdrant")]
use zeph_skills::qdrant_matcher::QdrantSkillMatcher;
use zeph_skills::registry::SkillRegistry;
use zeph_skills::watcher::SkillWatcher;
use zeph_tools::{CompositeExecutor, FileExecutor, ShellExecutor, WebScrapeExecutor};
#[cfg(feature = "tui")]
use zeph_tui::{App, EventReader, TuiChannel};

use zeph_channels::AnyChannel;

#[cfg(feature = "tui")]
#[derive(Debug)]
enum AppChannel {
    Standard(AnyChannel),
    Tui(TuiChannel),
}

#[cfg(feature = "tui")]
macro_rules! dispatch_app_channel {
    ($self:expr, $method:ident $(, $arg:expr)*) => {
        match $self {
            AppChannel::Standard(c) => c.$method($($arg),*).await,
            AppChannel::Tui(c) => c.$method($($arg),*).await,
        }
    };
}

#[cfg(feature = "tui")]
impl Channel for AppChannel {
    async fn recv(&mut self) -> Result<Option<ChannelMessage>, ChannelError> {
        dispatch_app_channel!(self, recv)
    }
    async fn send(&mut self, text: &str) -> Result<(), ChannelError> {
        dispatch_app_channel!(self, send, text)
    }
    async fn send_chunk(&mut self, chunk: &str) -> Result<(), ChannelError> {
        dispatch_app_channel!(self, send_chunk, chunk)
    }
    async fn flush_chunks(&mut self) -> Result<(), ChannelError> {
        dispatch_app_channel!(self, flush_chunks)
    }
    async fn send_typing(&mut self) -> Result<(), ChannelError> {
        dispatch_app_channel!(self, send_typing)
    }
    async fn confirm(&mut self, prompt: &str) -> Result<bool, ChannelError> {
        dispatch_app_channel!(self, confirm, prompt)
    }
    fn try_recv(&mut self) -> Option<ChannelMessage> {
        match self {
            Self::Standard(c) => c.try_recv(),
            Self::Tui(c) => c.try_recv(),
        }
    }
    async fn send_status(&mut self, text: &str) -> Result<(), ChannelError> {
        dispatch_app_channel!(self, send_status, text)
    }
    async fn send_queue_count(&mut self, count: usize) -> Result<(), ChannelError> {
        dispatch_app_channel!(self, send_queue_count, count)
    }
}

#[tokio::main]
#[allow(clippy::too_many_lines)]
async fn main() -> anyhow::Result<()> {
    // When TUI is active, redirect tracing to a file to avoid corrupting the terminal
    #[cfg(feature = "tui")]
    let tui_active = is_tui_requested();
    #[cfg(feature = "tui")]
    if tui_active {
        let filter = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
        let file = std::fs::File::create("zeph.log").ok();
        if let Some(file) = file {
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_writer(file)
                .init();
        } else {
            tracing_subscriber::fmt().with_env_filter(filter).init();
        }
    } else {
        tracing_subscriber::fmt::init();
    }
    #[cfg(not(feature = "tui"))]
    init_subscriber(&resolve_config_path());

    let config_path = resolve_config_path();
    let mut config = Config::load(&config_path)?;
    config.validate()?;

    let vault_args = parse_vault_args(&config);
    let vault: Box<dyn VaultProvider> = match vault_args.backend.as_str() {
        "env" => Box::new(EnvVaultProvider),
        #[cfg(feature = "vault-age")]
        "age" => {
            let key = vault_args
                .key_path
                .context("--vault-key required for age backend")?;
            let path = vault_args
                .vault_path
                .context("--vault-path required for age backend")?;
            Box::new(AgeVaultProvider::new(Path::new(&key), Path::new(&path))?)
        }
        other => bail!("unknown vault backend: {other}"),
    };

    config.resolve_secrets(vault.as_ref()).await?;

    let mut provider = create_provider(&config)?;
    let embed_model = effective_embedding_model(&config);

    let (status_tx, status_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    provider.set_status_tx(status_tx);

    health_check(&provider).await;

    // Auto-detect context window for Ollama models
    if let AnyProvider::Ollama(ref mut ollama) = provider
        && let Ok(info) = ollama.fetch_model_info().await
        && let Some(ctx) = info.context_length
    {
        ollama.set_context_window(ctx);
        tracing::info!(context_window = ctx, "detected Ollama model context window");
    }

    let budget_tokens = if config.memory.auto_budget && config.memory.context_budget_tokens == 0 {
        if let Some(ctx_size) = provider.context_window() {
            tracing::info!(model_context = ctx_size, "auto-configured context budget");
            ctx_size
        } else {
            0
        }
    } else {
        config.memory.context_budget_tokens
    };

    let skill_paths: Vec<PathBuf> = config.skills.paths.iter().map(PathBuf::from).collect();
    let registry = SkillRegistry::load(&skill_paths);

    let memory = SemanticMemory::with_weights(
        &config.memory.sqlite_path,
        &config.memory.qdrant_url,
        provider.clone(),
        &embed_model,
        config.memory.semantic.vector_weight,
        config.memory.semantic.keyword_weight,
    )
    .await?;

    if config.memory.semantic.enabled && memory.has_qdrant() {
        tracing::info!("semantic memory enabled, Qdrant connected");
        match memory.embed_missing().await {
            Ok(n) if n > 0 => tracing::info!("backfilled {n} missing embedding(s)"),
            Ok(_) => {}
            Err(e) => tracing::warn!("embed_missing failed: {e:#}"),
        }
    }

    let all_meta = registry.all_meta();
    let matcher = create_skill_matcher(&config, &provider, &all_meta, &memory, &embed_model).await;
    let skill_count = all_meta.len();
    if matcher.is_some() {
        tracing::info!("skill matcher initialized for {skill_count} skill(s)");
    } else {
        tracing::info!("skill matcher unavailable, using all {skill_count} skill(s)");
    }

    #[cfg(feature = "tui")]
    let (channel, tui_handle) = create_channel_with_tui(&config).await?;
    #[cfg(not(feature = "tui"))]
    let channel = create_channel(&config).await?;

    #[cfg(feature = "tui")]
    let is_cli = matches!(channel, AppChannel::Standard(AnyChannel::Cli(_)));
    #[cfg(not(feature = "tui"))]
    let is_cli = matches!(channel, AnyChannel::Cli(_));
    if is_cli {
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

    tokio::task::spawn_blocking(|| {
        zeph_tools::cleanup_overflow_files(std::time::Duration::from_secs(86_400));
    });

    let permission_policy = config
        .tools
        .permission_policy(config.security.autonomy_level);
    let mut shell_executor =
        ShellExecutor::new(&config.tools.shell).with_permissions(permission_policy.clone());
    if config.tools.audit.enabled
        && let Ok(logger) = zeph_tools::AuditLogger::from_config(&config.tools.audit).await
    {
        shell_executor = shell_executor.with_audit(logger);
    }

    #[cfg(feature = "tui")]
    let tool_event_rx = if tui_handle.is_some() {
        let (tool_tx, tool_rx) = tokio::sync::mpsc::unbounded_channel::<zeph_tools::ToolEvent>();
        shell_executor = shell_executor.with_tool_event_tx(tool_tx);
        Some(tool_rx)
    } else {
        None
    };
    let scrape_executor = WebScrapeExecutor::new(&config.tools.scrape);
    let file_executor = FileExecutor::new(
        config
            .tools
            .shell
            .allowed_paths
            .iter()
            .map(PathBuf::from)
            .collect(),
    );

    #[cfg(feature = "mcp")]
    let (tool_executor, mcp_tools, mcp_manager) = {
        let mcp_manager = std::sync::Arc::new(create_mcp_manager(&config));
        let mcp_tools = mcp_manager.connect_all().await;
        tracing::info!("discovered {} MCP tool(s)", mcp_tools.len());

        let mcp_executor = zeph_mcp::McpToolExecutor::new(mcp_manager.clone());
        let base_executor = CompositeExecutor::new(
            file_executor,
            CompositeExecutor::new(shell_executor, scrape_executor),
        );
        let executor = CompositeExecutor::new(base_executor, mcp_executor);

        (executor, mcp_tools, mcp_manager)
    };

    #[cfg(not(feature = "mcp"))]
    let tool_executor = CompositeExecutor::new(
        file_executor,
        CompositeExecutor::new(shell_executor, scrape_executor),
    );

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

    let (config_reload_tx, config_reload_rx) = tokio::sync::mpsc::channel(4);
    let _config_watcher = match ConfigWatcher::start(&config_path, config_reload_tx) {
        Ok(w) => {
            tracing::info!("config watcher started");
            Some(w)
        }
        Err(e) => {
            tracing::warn!("config watcher unavailable: {e:#}");
            None
        }
    };

    #[cfg(feature = "a2a")]
    if config.a2a.enabled {
        let a2a_provider = std::sync::Arc::new(provider.clone());
        let skill_names: Vec<&str> = all_meta.iter().map(|m| m.name.as_str()).collect();
        let a2a_system_prompt = format!(
            "You are {}. Available skills: {}",
            config.agent.name,
            skill_names.join(", ")
        );
        spawn_a2a_server(
            &config,
            shutdown_rx.clone(),
            a2a_provider,
            a2a_system_prompt,
        );
    }

    #[cfg(feature = "mcp")]
    let mcp_registry = create_mcp_registry(&config, &provider, &mcp_tools, &embed_model).await;

    #[cfg(feature = "index")]
    let index_pool = memory.sqlite().pool().clone();
    #[cfg(feature = "index")]
    let index_provider = provider.clone();
    #[cfg(feature = "index")]
    let provider_has_tools = provider.supports_tool_use();
    let warmup_provider_clone = provider.clone();

    let summary_provider = config.agent.summary_model.as_ref().and_then(|model_spec| {
        match create_summary_provider(model_spec, &config) {
            Ok(sp) => {
                tracing::info!(model = %model_spec, "summary provider configured");
                Some(sp)
            }
            Err(e) => {
                tracing::warn!("failed to create summary provider: {e:#}, using primary");
                None
            }
        }
    });

    let agent = Agent::new(
        provider,
        channel,
        registry,
        matcher,
        config.skills.max_active_skills,
        tool_executor,
    )
    .with_max_tool_iterations(config.agent.max_tool_iterations)
    .with_model_name(config.llm.model.clone())
    .with_embedding_model(embed_model.clone())
    .with_skill_reload(skill_paths, reload_rx)
    .with_memory(
        memory,
        conversation_id,
        config.memory.history_limit,
        config.memory.semantic.recall_limit,
        config.memory.summarization_threshold,
    )
    .with_context_budget(
        budget_tokens,
        0.20,
        config.memory.compaction_threshold,
        config.memory.compaction_preserve_tail,
        config.memory.prune_protect_tokens,
    )
    .with_shutdown(shutdown_rx)
    .with_security(config.security, config.timeouts)
    .with_tool_summarization(config.tools.summarize_output)
    .with_permission_policy(permission_policy.clone())
    .with_config_reload(config_path.clone(), config_reload_rx);

    let agent = if config.cost.enabled {
        let tracker = CostTracker::new(true, f64::from(config.cost.max_daily_cents));
        agent.with_cost_tracker(tracker)
    } else {
        agent
    };

    let agent = if let Some(sp) = summary_provider {
        agent.with_summary_provider(sp)
    } else {
        agent
    };

    #[cfg(feature = "index")]
    let mut _index_watcher: Option<IndexWatcher> = None;
    #[cfg(feature = "index")]
    let agent = if config.index.enabled && !provider_has_tools {
        let init = async {
            let store = CodeStore::new(&config.memory.qdrant_url, index_pool)?;
            let provider_arc = std::sync::Arc::new(index_provider);
            let retrieval_config = RetrievalConfig {
                max_chunks: config.index.max_chunks,
                score_threshold: config.index.score_threshold,
                budget_ratio: config.index.budget_ratio,
            };
            let retriever =
                CodeRetriever::new(store.clone(), provider_arc.clone(), retrieval_config);
            let indexer = std::sync::Arc::new(CodeIndexer::new(
                store,
                provider_arc,
                IndexerConfig::default(),
            ));
            anyhow::Ok((retriever, indexer))
        };
        match init.await {
            Ok((retriever, indexer)) => {
                let indexer_clone = indexer.clone();
                tokio::spawn(async move {
                    let root = std::env::current_dir().unwrap_or_default();
                    match indexer_clone.index_project(&root).await {
                        Ok(report) => tracing::info!(
                            files = report.files_indexed,
                            chunks = report.chunks_created,
                            ms = report.duration_ms,
                            "project indexed"
                        ),
                        Err(e) => tracing::warn!("background indexing failed: {e:#}"),
                    }
                });
                _index_watcher = if config.index.watch {
                    let root = std::env::current_dir().unwrap_or_default();
                    match IndexWatcher::start(&root, indexer) {
                        Ok(w) => {
                            tracing::info!("index watcher started");
                            Some(w)
                        }
                        Err(e) => {
                            tracing::warn!("index watcher failed to start: {e:#}");
                            None
                        }
                    }
                } else {
                    None
                };
                agent.with_code_retriever(
                    std::sync::Arc::new(retriever),
                    config.index.repo_map_tokens,
                    config.index.repo_map_ttl_secs,
                )
            }
            Err(e) => {
                tracing::warn!("code index initialization failed: {e:#}");
                agent
            }
        }
    } else {
        if config.index.enabled && provider_has_tools {
            tracing::info!("code index skipped: provider supports native tool_use");
        }
        agent
    };

    #[cfg(feature = "mcp")]
    let agent = agent.with_mcp(mcp_tools, mcp_registry, Some(mcp_manager), &config.mcp);

    #[cfg(feature = "self-learning")]
    let agent = agent.with_learning(config.skills.learning);

    #[cfg(feature = "tui")]
    let tui_metrics_rx;
    #[cfg(feature = "tui")]
    let agent = if tui_active {
        let (tx, rx) = tokio::sync::watch::channel(zeph_core::metrics::MetricsSnapshot::default());
        tx.send_modify(|m| {
            m.model_name.clone_from(&config.llm.model);
        });
        tui_metrics_rx = Some(rx);
        agent.with_metrics(tx)
    } else {
        tui_metrics_rx = None;
        agent
    };

    let mut agent = agent;

    agent.load_history().await?;

    #[cfg(feature = "tui")]
    if let Some(tui_handle) = tui_handle {
        let (event_tx, event_rx) = tokio::sync::mpsc::channel(256);

        let reader = EventReader::new(event_tx, Duration::from_millis(100));
        std::thread::spawn(move || reader.run());

        let mut app = App::new(tui_handle.user_tx, tui_handle.agent_rx);

        let history: Vec<(&str, &str)> = agent
            .context_messages()
            .iter()
            .map(|m| {
                let role = match m.role {
                    zeph_llm::provider::Role::User => "user",
                    zeph_llm::provider::Role::Assistant => "assistant",
                    zeph_llm::provider::Role::System => "system",
                };
                (role, m.content.as_str())
            })
            .collect();
        app.load_history(&history);

        if let Some(rx) = tui_metrics_rx {
            app = app.with_metrics_rx(rx);
        }

        let agent_tx = tui_handle.agent_tx;
        tokio::spawn(forward_status_to_tui(status_rx, agent_tx.clone()));

        if let Some(tool_rx) = tool_event_rx {
            tokio::spawn(forward_tool_events_to_tui(tool_rx, agent_tx.clone()));
        }

        let (warmup_tx, warmup_rx) = watch::channel(false);
        let warmup_agent_tx = agent_tx.clone();
        tokio::spawn(async move {
            let _ = warmup_agent_tx
                .send(zeph_tui::AgentEvent::Status("warming up model...".into()))
                .await;
            warmup_provider(&warmup_provider_clone).await;
            let _ = warmup_agent_tx
                .send(zeph_tui::AgentEvent::Status("model ready".into()))
                .await;
            let _ = warmup_tx.send(true);
        });

        let mut agent = agent.with_warmup_ready(warmup_rx);

        let tui_task = tokio::spawn(zeph_tui::run_tui(app, event_rx));
        // No Box::pin here: TUI branch already spawns tasks, no large_futures lint
        let agent_future = agent.run();

        tokio::select! {
            result = tui_task => {
                agent.shutdown().await;
                return result?;
            }
            result = agent_future => {
                agent.shutdown().await;
                return result;
            }
        }
    }

    warmup_provider(&warmup_provider_clone).await;
    tokio::spawn(forward_status_to_stderr(status_rx));
    // Box::pin avoids clippy::large_futures on non-TUI path
    let result = Box::pin(agent.run()).await;
    agent.shutdown().await;
    result
}

async fn forward_status_to_stderr(mut rx: tokio::sync::mpsc::UnboundedReceiver<String>) {
    while let Some(msg) = rx.recv().await {
        eprintln!("[status] {msg}");
    }
}

#[cfg(feature = "tui")]
async fn forward_status_to_tui(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<String>,
    tx: tokio::sync::mpsc::Sender<zeph_tui::AgentEvent>,
) {
    while let Some(msg) = rx.recv().await {
        if tx.send(zeph_tui::AgentEvent::Status(msg)).await.is_err() {
            break;
        }
    }
}

#[cfg(feature = "tui")]
async fn forward_tool_events_to_tui(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<zeph_tools::ToolEvent>,
    tx: tokio::sync::mpsc::Sender<zeph_tui::AgentEvent>,
) {
    while let Some(event) = rx.recv().await {
        let agent_event = match event {
            zeph_tools::ToolEvent::Started { tool_name, command } => {
                zeph_tui::AgentEvent::ToolStart { tool_name, command }
            }
            zeph_tools::ToolEvent::OutputChunk {
                tool_name,
                command,
                chunk,
            } => zeph_tui::AgentEvent::ToolOutputChunk {
                tool_name,
                command,
                chunk,
            },
            zeph_tools::ToolEvent::Completed {
                tool_name,
                command,
                output,
                success,
            } => zeph_tui::AgentEvent::ToolOutput {
                tool_name,
                command,
                output,
                success,
            },
        };
        if tx.send(agent_event).await.is_err() {
            break;
        }
    }
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

async fn warmup_provider(provider: &AnyProvider) {
    match provider {
        AnyProvider::Ollama(ollama) => {
            let start = std::time::Instant::now();
            match ollama.warmup().await {
                Ok(()) => {
                    tracing::info!("ollama model ready ({:.1}s)", start.elapsed().as_secs_f64());
                }
                Err(e) => tracing::warn!("ollama warmup failed: {e:#}"),
            }
        }
        #[cfg(feature = "orchestrator")]
        AnyProvider::Orchestrator(orch) => {
            for (name, p) in orch.providers() {
                if let zeph_llm::orchestrator::SubProvider::Ollama(ollama) = p {
                    let start = std::time::Instant::now();
                    match ollama.warmup().await {
                        Ok(()) => tracing::info!(
                            "ollama '{name}' ready ({:.1}s)",
                            start.elapsed().as_secs_f64()
                        ),
                        Err(e) => tracing::warn!("ollama '{name}' warmup failed: {e:#}"),
                    }
                }
            }
        }
        _ => {}
    }
}

#[allow(unused_variables)]
async fn create_skill_matcher(
    config: &Config,
    provider: &AnyProvider,
    meta: &[&SkillMeta],
    memory: &SemanticMemory,
    embedding_model: &str,
) -> Option<SkillMatcherBackend> {
    let embed_fn = provider.embed_fn();

    #[cfg(feature = "qdrant")]
    if config.memory.semantic.enabled && memory.has_qdrant() {
        match QdrantSkillMatcher::new(&config.memory.qdrant_url) {
            Ok(mut qm) => match qm.sync(meta, embedding_model, &embed_fn).await {
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

    SkillMatcher::new(meta, &embed_fn)
        .await
        .map(SkillMatcherBackend::InMemory)
}

fn effective_embedding_model(config: &Config) -> String {
    match config.llm.provider {
        #[cfg(feature = "openai")]
        ProviderKind::OpenAi => {
            if let Some(m) = config
                .llm
                .openai
                .as_ref()
                .and_then(|o| o.embedding_model.clone())
            {
                return m;
            }
        }
        #[cfg(feature = "orchestrator")]
        ProviderKind::Orchestrator => {
            if let Some(orch) = &config.llm.orchestrator
                && let Some(pcfg) = orch.providers.get(&orch.embed)
            {
                #[cfg(feature = "openai")]
                if pcfg.provider_type == "openai"
                    && let Some(m) = config
                        .llm
                        .openai
                        .as_ref()
                        .and_then(|o| o.embedding_model.clone())
                {
                    return m;
                }
            }
        }
        ProviderKind::Compatible => {
            if let Some(entries) = &config.llm.compatible
                && let Some(entry) = entries.first()
                && let Some(ref m) = entry.embedding_model
            {
                return m.clone();
            }
        }
        _ => {}
    }
    config.llm.embedding_model.clone()
}

#[allow(clippy::too_many_lines)]
fn create_provider(config: &Config) -> anyhow::Result<AnyProvider> {
    match config.llm.provider {
        ProviderKind::Ollama | ProviderKind::Claude => {
            create_named_provider(config.llm.provider.as_str(), config)
        }
        #[cfg(feature = "openai")]
        ProviderKind::OpenAi => create_named_provider("openai", config),
        #[cfg(feature = "compatible")]
        ProviderKind::Compatible => create_named_provider("compatible", config),
        #[cfg(feature = "candle")]
        ProviderKind::Candle => {
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
        ProviderKind::Orchestrator => {
            let orch = build_orchestrator(config)?;
            Ok(AnyProvider::Orchestrator(Box::new(orch)))
        }
        #[cfg(feature = "router")]
        ProviderKind::Router => {
            let router_cfg = config
                .llm
                .router
                .as_ref()
                .context("llm.router config section required for router provider")?;

            let mut providers = Vec::new();
            for name in &router_cfg.chain {
                let p = create_named_provider(name, config)?;
                providers.push(p);
            }
            if providers.is_empty() {
                bail!("router chain is empty");
            }
            Ok(AnyProvider::Router(Box::new(RouterProvider::new(
                providers,
            ))))
        }
        #[allow(unreachable_patterns)]
        other => bail!("LLM provider {other} not available (feature not enabled)"),
    }
}

fn create_named_provider(name: &str, config: &Config) -> anyhow::Result<AnyProvider> {
    match name {
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
            let api_key = config
                .secrets
                .claude_api_key
                .as_ref()
                .context("ZEPH_CLAUDE_API_KEY not found in vault")?
                .expose()
                .to_owned();
            Ok(AnyProvider::Claude(ClaudeProvider::new(
                api_key,
                cloud.model.clone(),
                cloud.max_tokens,
            )))
        }
        #[cfg(feature = "openai")]
        "openai" => {
            let openai_cfg = config
                .llm
                .openai
                .as_ref()
                .context("llm.openai config section required for OpenAI provider")?;
            let api_key = config
                .secrets
                .openai_api_key
                .as_ref()
                .context("ZEPH_OPENAI_API_KEY not found in vault")?
                .expose()
                .to_owned();
            Ok(AnyProvider::OpenAi(OpenAiProvider::new(
                api_key,
                openai_cfg.base_url.clone(),
                openai_cfg.model.clone(),
                openai_cfg.max_tokens,
                openai_cfg.embedding_model.clone(),
                openai_cfg.reasoning_effort.clone(),
            )))
        }
        other => {
            #[cfg(feature = "compatible")]
            if let Some(entries) = &config.llm.compatible {
                let entry = if other == "compatible" {
                    entries.first()
                } else {
                    entries.iter().find(|e| e.name == other)
                };
                if let Some(entry) = entry {
                    let api_key = config
                        .secrets
                        .compatible_api_keys
                        .get(&entry.name)
                        .with_context(|| {
                            format!(
                                "ZEPH_COMPATIBLE_{}_API_KEY required for {}",
                                entry.name.to_uppercase(),
                                entry.name
                            )
                        })?
                        .expose()
                        .to_owned();
                    return Ok(AnyProvider::Compatible(CompatibleProvider::new(
                        entry.name.clone(),
                        api_key,
                        entry.base_url.clone(),
                        entry.model.clone(),
                        entry.max_tokens,
                        entry.embedding_model.clone(),
                    )));
                }
            }
            bail!("unknown provider: {other}")
        }
    }
}

fn create_summary_provider(model_spec: &str, config: &Config) -> anyhow::Result<AnyProvider> {
    if let Some(model) = model_spec.strip_prefix("ollama/") {
        let base_url = &config.llm.base_url;
        let provider = OllamaProvider::new(base_url, model.to_owned(), String::new());
        Ok(AnyProvider::Ollama(provider))
    } else {
        bail!("unsupported summary_model format: {model_spec} (expected 'ollama/<model>')")
    }
}

#[cfg(feature = "a2a")]
fn spawn_a2a_server(
    config: &Config,
    shutdown_rx: watch::Receiver<bool>,
    provider: std::sync::Arc<AnyProvider>,
    system_prompt: String,
) {
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
        std::sync::Arc::new(AgentTaskProcessor {
            provider,
            system_prompt,
        });
    let a2a_server = zeph_a2a::A2aServer::new(
        card,
        processor,
        &config.a2a.host,
        config.a2a.port,
        shutdown_rx,
    )
    .with_auth(config.a2a.auth_token.clone())
    .with_rate_limit(config.a2a.rate_limit)
    .with_max_body_size(config.a2a.max_body_size);

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
struct AgentTaskProcessor {
    provider: std::sync::Arc<AnyProvider>,
    system_prompt: String,
}

#[cfg(feature = "a2a")]
impl zeph_a2a::TaskProcessor for AgentTaskProcessor {
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
        let provider = self.provider.clone();
        let system_prompt = self.system_prompt.clone();
        let user_text = message.text_content().unwrap_or("").to_owned();

        Box::pin(async move {
            let messages = vec![
                zeph_llm::provider::Message::from_legacy(
                    zeph_llm::provider::Role::System,
                    &system_prompt,
                ),
                zeph_llm::provider::Message::from_legacy(
                    zeph_llm::provider::Role::User,
                    &user_text,
                ),
            ];

            let response_text = provider.chat(&messages).await.map_err(|e| {
                tracing::error!("A2A inference failed: {e:#}");
                zeph_a2a::A2aError::Server("inference failed".to_owned())
            })?;

            Ok(zeph_a2a::ProcessResult {
                response: zeph_a2a::Message {
                    role: zeph_a2a::Role::Agent,
                    parts: vec![zeph_a2a::Part::text(response_text)],
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
        .map(|s| {
            let transport = if let Some(ref url) = s.url {
                zeph_mcp::McpTransport::Http { url: url.clone() }
            } else {
                zeph_mcp::McpTransport::Stdio {
                    command: s.command.clone().unwrap_or_default(),
                    args: s.args.clone(),
                    env: s.env.clone(),
                }
            };
            zeph_mcp::ServerEntry {
                id: s.id.clone(),
                transport,
                timeout: std::time::Duration::from_secs(s.timeout),
            }
        })
        .collect();
    zeph_mcp::McpManager::new(entries)
}

#[cfg(feature = "mcp")]
async fn create_mcp_registry(
    config: &Config,
    provider: &AnyProvider,
    mcp_tools: &[zeph_mcp::McpTool],
    embedding_model: &str,
) -> Option<zeph_mcp::McpToolRegistry> {
    if !config.memory.semantic.enabled {
        return None;
    }
    match zeph_mcp::McpToolRegistry::new(&config.memory.qdrant_url) {
        Ok(mut reg) => {
            let embed_fn = provider.embed_fn();
            if let Err(e) = reg.sync(mcp_tools, embedding_model, &embed_fn).await {
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
#[allow(clippy::too_many_lines)]
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
                let api_key = config
                    .secrets
                    .claude_api_key
                    .as_ref()
                    .context("ZEPH_CLAUDE_API_KEY required for claude sub-provider")?
                    .expose()
                    .to_owned();
                let model = pcfg.model.as_deref().unwrap_or(&cloud.model);
                SubProvider::Claude(ClaudeProvider::new(
                    api_key,
                    model.to_owned(),
                    cloud.max_tokens,
                ))
            }
            #[cfg(feature = "openai")]
            "openai" => {
                let openai_cfg = config
                    .llm
                    .openai
                    .as_ref()
                    .context("llm.openai config required for openai sub-provider")?;
                let api_key = config
                    .secrets
                    .openai_api_key
                    .as_ref()
                    .context("ZEPH_OPENAI_API_KEY required for openai sub-provider")?
                    .expose()
                    .to_owned();
                let model = pcfg.model.as_deref().unwrap_or(&openai_cfg.model);
                SubProvider::OpenAi(OpenAiProvider::new(
                    api_key,
                    openai_cfg.base_url.clone(),
                    model.to_owned(),
                    openai_cfg.max_tokens,
                    openai_cfg.embedding_model.clone(),
                    openai_cfg.reasoning_effort.clone(),
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

    Ok(ModelOrchestrator::new(
        routes,
        providers,
        orch_cfg.default.clone(),
        orch_cfg.embed.clone(),
    )?)
}

#[cfg_attr(not(feature = "vault-age"), allow(dead_code))]
struct VaultArgs {
    backend: String,
    key_path: Option<String>,
    vault_path: Option<String>,
}

/// Priority: CLI --vault > `ZEPH_VAULT_BACKEND` env > config.vault.backend > "env"
fn parse_vault_args(config: &Config) -> VaultArgs {
    let args: Vec<String> = std::env::args().collect();
    let cli_backend = args
        .windows(2)
        .find(|w| w[0] == "--vault")
        .map(|w| w[1].clone());
    let env_backend = std::env::var("ZEPH_VAULT_BACKEND").ok();
    let backend = cli_backend
        .or(env_backend)
        .unwrap_or_else(|| config.vault.backend.clone());
    let key_path = args
        .windows(2)
        .find(|w| w[0] == "--vault-key")
        .map(|w| w[1].clone());
    let vault_path = args
        .windows(2)
        .find(|w| w[0] == "--vault-path")
        .map(|w| w[1].clone());
    VaultArgs {
        backend,
        key_path,
        vault_path,
    }
}

fn resolve_config_path() -> PathBuf {
    let args: Vec<String> = std::env::args().collect();
    if let Some(path) = args.windows(2).find(|w| w[0] == "--config").map(|w| &w[1]) {
        return PathBuf::from(path);
    }
    if let Ok(path) = std::env::var("ZEPH_CONFIG") {
        return PathBuf::from(path);
    }
    PathBuf::from("config/default.toml")
}

#[cfg(feature = "tui")]
struct TuiHandle {
    user_tx: tokio::sync::mpsc::Sender<String>,
    agent_tx: tokio::sync::mpsc::Sender<zeph_tui::AgentEvent>,
    agent_rx: tokio::sync::mpsc::Receiver<zeph_tui::AgentEvent>,
}

#[cfg(feature = "tui")]
fn is_tui_requested() -> bool {
    std::env::args().any(|a| a == "--tui")
        || std::env::var("ZEPH_TUI")
            .map(|v| v == "true" || v == "1")
            .unwrap_or(false)
}

#[cfg(feature = "tui")]
async fn create_channel_with_tui(
    config: &Config,
) -> anyhow::Result<(AppChannel, Option<TuiHandle>)> {
    if is_tui_requested() {
        let (user_tx, user_rx) = tokio::sync::mpsc::channel(32);
        let (agent_tx, agent_rx) = tokio::sync::mpsc::channel(256);
        let agent_tx_clone = agent_tx.clone();
        let channel = TuiChannel::new(user_rx, agent_tx);
        let handle = TuiHandle {
            user_tx,
            agent_tx: agent_tx_clone,
            agent_rx,
        };
        return Ok((AppChannel::Tui(channel), Some(handle)));
    }
    let channel = create_channel_inner(config).await?;
    Ok((AppChannel::Standard(channel), None))
}

#[cfg_attr(feature = "tui", allow(dead_code))]
async fn create_channel(config: &Config) -> anyhow::Result<AnyChannel> {
    create_channel_inner(config).await
}

#[allow(clippy::unused_async)]
async fn create_channel_inner(config: &Config) -> anyhow::Result<AnyChannel> {
    #[cfg(feature = "discord")]
    if let Some(dc) = &config.discord
        && let Some(token) = &dc.token
    {
        let channel = DiscordChannel::new(
            token.clone(),
            dc.allowed_user_ids.clone(),
            dc.allowed_role_ids.clone(),
            dc.allowed_channel_ids.clone(),
        );
        tracing::info!("running in Discord mode");
        return Ok(AnyChannel::Discord(channel));
    }

    #[cfg(feature = "slack")]
    if let Some(sl) = &config.slack
        && let Some(bot_token) = &sl.bot_token
    {
        let signing_secret = sl.signing_secret.clone().unwrap_or_default();
        let channel = SlackChannel::new(
            bot_token.clone(),
            signing_secret,
            sl.webhook_host.clone(),
            sl.port,
            sl.allowed_user_ids.clone(),
            sl.allowed_channel_ids.clone(),
        )
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
        tracing::info!(
            "running in Slack mode (events on {}:{})",
            sl.webhook_host,
            sl.port
        );
        return Ok(AnyChannel::Slack(channel));
    }

    if let Some(token) = config.telegram.as_ref().and_then(|t| t.token.clone()) {
        let allowed = config
            .telegram
            .as_ref()
            .map_or_else(Vec::new, |t| t.allowed_users.clone());
        let tg = TelegramChannel::new(token, allowed).start()?;
        tracing::info!("running in Telegram mode");
        return Ok(AnyChannel::Telegram(tg));
    }

    Ok(AnyChannel::Cli(CliChannel::new()))
}

#[cfg(not(feature = "tui"))]
fn init_subscriber(config_path: &Path) {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let fmt_layer = tracing_subscriber::fmt::layer();

    #[cfg(feature = "otel")]
    {
        let config = Config::load(config_path).ok();
        let use_otlp = config
            .as_ref()
            .is_some_and(|c| c.observability.exporter == "otlp");

        if use_otlp {
            let endpoint = config
                .as_ref()
                .map_or("http://localhost:4317", |c| &c.observability.endpoint);

            match setup_otel_tracer(endpoint) {
                Ok(tracer) => {
                    let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);
                    tracing_subscriber::registry()
                        .with(filter)
                        .with(fmt_layer)
                        .with(otel_layer)
                        .init();
                    return;
                }
                Err(e) => {
                    eprintln!("OTel initialization failed, falling back to fmt: {e}");
                }
            }
        }
    }

    #[cfg(not(feature = "otel"))]
    let _ = config_path;

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt_layer)
        .init();
}

#[cfg(feature = "otel")]
fn setup_otel_tracer(endpoint: &str) -> anyhow::Result<opentelemetry_sdk::trace::SdkTracer> {
    use opentelemetry::trace::TracerProvider;
    use opentelemetry_otlp::WithExportConfig;

    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .build()?;

    let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .build();

    let tracer = provider.tracer("zeph");
    opentelemetry::global::set_tracer_provider(provider);

    Ok(tracer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeph_core::channel::Channel;

    #[test]
    fn vault_args_defaults_in_test_context() {
        let config = Config::load(Path::new("/nonexistent")).unwrap();
        let args = parse_vault_args(&config);
        assert_eq!(args.backend, "env");
        assert!(args.key_path.is_none());
        assert!(args.vault_path.is_none());
    }

    #[test]
    fn vault_args_uses_config_backend_as_fallback() {
        let mut config = Config::load(Path::new("/nonexistent")).unwrap();
        config.vault.backend = "age".into();
        let args = parse_vault_args(&config);
        assert_eq!(args.backend, "age");
    }

    #[test]
    fn vault_args_env_overrides_config() {
        let mut config = Config::load(Path::new("/nonexistent")).unwrap();
        config.vault.backend = "age".into();
        unsafe { std::env::set_var("ZEPH_VAULT_BACKEND", "env") };
        let args = parse_vault_args(&config);
        unsafe { std::env::remove_var("ZEPH_VAULT_BACKEND") };
        assert_eq!(args.backend, "env");
    }

    #[test]
    fn vault_args_struct_construction() {
        let args = VaultArgs {
            backend: "age".into(),
            key_path: Some("/tmp/key".into()),
            vault_path: Some("/tmp/vault".into()),
        };
        assert_eq!(args.backend, "age");
        assert_eq!(args.key_path.as_deref(), Some("/tmp/key"));
        assert_eq!(args.vault_path.as_deref(), Some("/tmp/vault"));
    }

    #[test]
    fn vault_args_struct_env_backend() {
        let args = VaultArgs {
            backend: "env".into(),
            key_path: None,
            vault_path: None,
        };
        assert_eq!(args.backend, "env");
        assert!(args.key_path.is_none());
        assert!(args.vault_path.is_none());
    }

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
        let config = Config::load(Path::new("config/default.toml"));
        assert!(config.is_ok());
    }

    #[test]
    fn config_loading_nonexistent_uses_defaults() {
        let config = Config::load(Path::new("/does/not/exist.toml")).unwrap();
        assert_eq!(config.llm.provider, ProviderKind::Ollama);
        assert_eq!(config.agent.name, "Zeph");
    }

    #[test]
    fn create_provider_ollama() {
        let config = Config::load(Path::new("/nonexistent")).unwrap();
        let provider = create_provider(&config).unwrap();
        assert!(matches!(provider, AnyProvider::Ollama(_)));
        assert_eq!(provider.name(), "ollama");
    }

    #[test]
    fn create_provider_claude_without_cloud_config_errors() {
        let mut config = Config::load(Path::new("/nonexistent")).unwrap();
        config.llm.provider = ProviderKind::Claude;
        config.llm.cloud = None;
        let result = create_provider(&config);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("llm.cloud config section required")
        );
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

    #[tokio::test]
    async fn health_check_ollama_unreachable() {
        let provider = AnyProvider::Ollama(OllamaProvider::new(
            "http://127.0.0.1:1",
            "test".into(),
            "embed".into(),
        ));
        health_check(&provider).await;
    }

    #[tokio::test]
    async fn health_check_claude_noop() {
        let provider = AnyProvider::Claude(ClaudeProvider::new("key".into(), "model".into(), 1024));
        health_check(&provider).await;
    }

    #[cfg(feature = "candle")]
    #[test]
    fn select_device_cpu_default() {
        let device = select_device("cpu").unwrap();
        assert!(matches!(device, zeph_llm::candle_provider::Device::Cpu));
    }

    #[cfg(feature = "candle")]
    #[test]
    fn select_device_unknown_defaults_to_cpu() {
        let device = select_device("unknown").unwrap();
        assert!(matches!(device, zeph_llm::candle_provider::Device::Cpu));
    }

    #[cfg(all(feature = "candle", not(feature = "metal")))]
    #[test]
    fn select_device_metal_without_feature_errors() {
        let result = select_device("metal");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("metal feature"));
    }

    #[cfg(all(feature = "candle", not(feature = "cuda")))]
    #[test]
    fn select_device_cuda_without_feature_errors() {
        let result = select_device("cuda");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("cuda feature"));
    }

    #[cfg(feature = "candle")]
    #[test]
    fn select_device_auto_fallback() {
        let device = select_device("auto").unwrap();
        assert!(matches!(
            device,
            zeph_llm::candle_provider::Device::Cpu
                | zeph_llm::candle_provider::Device::Cuda(_)
                | zeph_llm::candle_provider::Device::Metal(_)
        ));
    }

    #[cfg(feature = "candle")]
    #[test]
    fn create_provider_candle_without_config_errors() {
        let mut config = Config::load(Path::new("/nonexistent")).unwrap();
        config.llm.provider = ProviderKind::Candle;
        config.llm.candle = None;
        let result = create_provider(&config);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("llm.candle config section required")
        );
    }

    #[cfg(feature = "orchestrator")]
    #[test]
    fn create_provider_orchestrator_without_config_errors() {
        let mut config = Config::load(Path::new("/nonexistent")).unwrap();
        config.llm.provider = ProviderKind::Orchestrator;
        config.llm.orchestrator = None;
        let result = create_provider(&config);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("llm.orchestrator config section required")
        );
    }

    #[cfg(feature = "orchestrator")]
    #[test]
    fn build_orchestrator_with_unknown_provider_errors() {
        use std::collections::HashMap;
        use zeph_core::config::OrchestratorProviderConfig;

        let mut config = Config::load(Path::new("/nonexistent")).unwrap();
        config.llm.provider = ProviderKind::Orchestrator;

        let mut providers = HashMap::new();
        providers.insert(
            "test".to_string(),
            OrchestratorProviderConfig {
                provider_type: "unknown_type".to_string(),
                model: None,
                filename: None,
                device: None,
            },
        );

        config.llm.orchestrator = Some(zeph_core::config::OrchestratorConfig {
            providers,
            routes: HashMap::new(),
            default: "test".to_string(),
            embed: "test".to_string(),
        });

        let result = build_orchestrator(&config);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("unknown orchestrator sub-provider type")
        );
    }

    #[cfg(feature = "orchestrator")]
    #[test]
    fn build_orchestrator_claude_without_cloud_config_errors() {
        use std::collections::HashMap;
        use zeph_core::config::OrchestratorProviderConfig;

        let mut config = Config::load(Path::new("/nonexistent")).unwrap();
        config.llm.provider = ProviderKind::Orchestrator;
        config.llm.cloud = None;

        let mut providers = HashMap::new();
        providers.insert(
            "claude_sub".to_string(),
            OrchestratorProviderConfig {
                provider_type: "claude".to_string(),
                model: None,
                filename: None,
                device: None,
            },
        );

        config.llm.orchestrator = Some(zeph_core::config::OrchestratorConfig {
            providers,
            routes: HashMap::new(),
            default: "claude_sub".to_string(),
            embed: "claude_sub".to_string(),
        });

        let result = build_orchestrator(&config);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("llm.cloud config required")
        );
    }

    #[cfg(all(feature = "orchestrator", feature = "candle"))]
    #[test]
    fn build_orchestrator_candle_without_config_errors() {
        use std::collections::HashMap;
        use zeph_core::config::OrchestratorProviderConfig;

        let mut config = Config::load(Path::new("/nonexistent")).unwrap();
        config.llm.provider = ProviderKind::Orchestrator;
        config.llm.candle = None;

        let mut providers = HashMap::new();
        providers.insert(
            "candle_sub".to_string(),
            OrchestratorProviderConfig {
                provider_type: "candle".to_string(),
                model: None,
                filename: None,
                device: None,
            },
        );

        config.llm.orchestrator = Some(zeph_core::config::OrchestratorConfig {
            providers,
            routes: HashMap::new(),
            default: "candle_sub".to_string(),
            embed: "candle_sub".to_string(),
        });

        let result = build_orchestrator(&config);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("llm.candle config required")
        );
    }

    #[cfg(feature = "mcp")]
    #[test]
    fn create_mcp_manager_with_http_transport() {
        use std::collections::HashMap;

        let mut config = Config::load(Path::new("/nonexistent")).unwrap();
        config.mcp.servers = vec![zeph_core::config::McpServerConfig {
            id: "test".into(),
            url: Some("http://localhost:3000".into()),
            command: None,
            args: vec![],
            env: HashMap::new(),
            timeout: 30,
        }];

        let manager = create_mcp_manager(&config);
        let debug = format!("{manager:?}");
        assert!(debug.contains("server_count: 1"));
    }

    #[cfg(feature = "mcp")]
    #[test]
    fn create_mcp_manager_with_stdio_transport() {
        use std::collections::HashMap;

        let mut config = Config::load(Path::new("/nonexistent")).unwrap();
        config.mcp.servers = vec![zeph_core::config::McpServerConfig {
            id: "test".into(),
            url: None,
            command: Some("node".into()),
            args: vec!["server.js".into()],
            env: HashMap::new(),
            timeout: 30,
        }];

        let manager = create_mcp_manager(&config);
        let debug = format!("{manager:?}");
        assert!(debug.contains("server_count: 1"));
    }

    #[cfg(feature = "mcp")]
    #[test]
    fn create_mcp_manager_empty_servers() {
        let mut config = Config::load(Path::new("/nonexistent")).unwrap();
        config.mcp.servers = vec![];

        let manager = create_mcp_manager(&config);
        let debug = format!("{manager:?}");
        assert!(debug.contains("server_count: 0"));
    }

    #[cfg(feature = "mcp")]
    #[tokio::test]
    async fn create_mcp_registry_when_semantic_disabled() {
        let config_path = Path::new("/nonexistent");
        let mut config = Config::load(config_path).unwrap();
        config.memory.semantic.enabled = false;

        let provider = AnyProvider::Ollama(OllamaProvider::new(
            "http://localhost:11434",
            "test".into(),
            "embed".into(),
        ));

        let mcp_tools = vec![];
        let registry = create_mcp_registry(&config, &provider, &mcp_tools, "test-model").await;
        assert!(registry.is_none());
    }

    #[cfg(feature = "a2a")]
    #[test]
    fn agent_task_processor_construction() {
        let provider = std::sync::Arc::new(AnyProvider::Ollama(OllamaProvider::new(
            "http://localhost:11434",
            "test".into(),
            "embed".into(),
        )));
        let processor = AgentTaskProcessor {
            provider,
            system_prompt: "test prompt".into(),
        };
        assert!(!processor.system_prompt.is_empty());
    }

    #[test]
    fn create_provider_claude_without_api_key_errors() {
        let mut config = Config::load(Path::new("/nonexistent")).unwrap();
        config.llm.provider = ProviderKind::Claude;
        config.llm.cloud = Some(zeph_core::config::CloudLlmConfig {
            model: "claude-3-opus".into(),
            max_tokens: 4096,
        });
        config.secrets.claude_api_key = None;

        let result = create_provider(&config);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("ZEPH_CLAUDE_API_KEY not found")
        );
    }

    #[cfg(feature = "orchestrator")]
    #[test]
    fn build_orchestrator_claude_sub_without_api_key_errors() {
        use std::collections::HashMap;
        use zeph_core::config::OrchestratorProviderConfig;

        let mut config = Config::load(Path::new("/nonexistent")).unwrap();
        config.llm.provider = ProviderKind::Orchestrator;
        config.llm.cloud = Some(zeph_core::config::CloudLlmConfig {
            model: "claude-3".into(),
            max_tokens: 4096,
        });
        config.secrets.claude_api_key = None;

        let mut providers = HashMap::new();
        providers.insert(
            "claude_sub".to_string(),
            OrchestratorProviderConfig {
                provider_type: "claude".to_string(),
                model: None,
                filename: None,
                device: None,
            },
        );

        config.llm.orchestrator = Some(zeph_core::config::OrchestratorConfig {
            providers,
            routes: HashMap::new(),
            default: "claude_sub".to_string(),
            embed: "claude_sub".to_string(),
        });

        let result = build_orchestrator(&config);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("ZEPH_CLAUDE_API_KEY required")
        );
    }

    #[tokio::test]
    async fn create_skill_matcher_when_semantic_disabled() {
        let tmp = std::env::temp_dir().join("zeph_test_skill_matcher.db");
        let tmp_path = tmp.to_string_lossy().to_string();

        let mut config = Config::load(Path::new("/nonexistent")).unwrap();
        config.memory.semantic.enabled = false;
        config.memory.sqlite_path = tmp_path.clone();

        let provider = AnyProvider::Ollama(OllamaProvider::new(
            "http://localhost:11434",
            "test".into(),
            "embed".into(),
        ));

        let memory = SemanticMemory::new(
            &tmp_path,
            &config.memory.qdrant_url,
            provider.clone(),
            &config.llm.embedding_model,
        )
        .await
        .unwrap();

        let meta: Vec<&SkillMeta> = vec![];
        let result = create_skill_matcher(&config, &provider, &meta, &memory, "test-model").await;
        assert!(result.is_none());

        let _ = std::fs::remove_file(&tmp);
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

    #[cfg(feature = "candle")]
    #[tokio::test]
    async fn health_check_candle_logs_device() {
        use zeph_llm::candle_provider::CandleProvider;

        let source = zeph_llm::candle_provider::loader::ModelSource::HuggingFace {
            repo_id: "test/model".to_string(),
            filename: Some("model.gguf".to_string()),
        };
        let template = zeph_llm::candle_provider::template::ChatTemplate::parse_str(
            "{{ bos_token }}{{ messages[0].content }}",
        );
        let gen_config = zeph_llm::candle_provider::generate::GenerationConfig {
            temperature: 0.7,
            top_p: Some(0.9),
            top_k: Some(50),
            max_tokens: 512,
            seed: 42,
            repeat_penalty: 1.1,
            repeat_last_n: 64,
        };
        let device = zeph_llm::candle_provider::Device::Cpu;

        let candle_result =
            CandleProvider::new(&source, template, gen_config, Some("embed/model"), device);

        if let Ok(candle) = candle_result {
            let provider = AnyProvider::Candle(candle);
            health_check(&provider).await;
        }
    }

    #[cfg(feature = "orchestrator")]
    #[tokio::test]
    async fn health_check_orchestrator_logs_providers() {
        use std::collections::HashMap;
        use zeph_llm::orchestrator::{ModelOrchestrator, SubProvider};

        let mut providers = HashMap::new();
        providers.insert(
            "ollama_local".to_string(),
            SubProvider::Ollama(OllamaProvider::new(
                "http://localhost:11434",
                "test".into(),
                "embed".into(),
            )),
        );

        let routes = HashMap::new();
        let orch = ModelOrchestrator::new(
            routes,
            providers,
            "ollama_local".to_string(),
            "ollama_local".to_string(),
        )
        .unwrap();

        let provider = AnyProvider::Orchestrator(Box::new(orch));
        health_check(&provider).await;
    }

    #[cfg(all(feature = "orchestrator", feature = "candle"))]
    #[test]
    fn build_orchestrator_with_candle_local_source() {
        use std::collections::HashMap;
        use zeph_core::config::OrchestratorProviderConfig;

        let mut config = Config::load(Path::new("/nonexistent")).unwrap();
        config.llm.provider = ProviderKind::Orchestrator;
        config.llm.candle = Some(zeph_core::config::CandleConfig {
            source: "local".into(),
            local_path: "/tmp/model.gguf".into(),
            filename: Some("model.gguf".to_string()),
            chat_template: "{{ messages[0].content }}".into(),
            device: "cpu".into(),
            embedding_repo: Some("embed/model".into()),
            generation: zeph_core::config::GenerationParams {
                temperature: 0.7,
                top_p: Some(0.9),
                top_k: Some(50),
                max_tokens: 512,
                seed: 42,
                repeat_penalty: 1.1,
                repeat_last_n: 64,
            },
        });

        let mut providers = HashMap::new();
        providers.insert(
            "candle_local".to_string(),
            OrchestratorProviderConfig {
                provider_type: "candle".to_string(),
                model: Some("local-model".to_string()),
                filename: None,
                device: Some("cpu".to_string()),
            },
        );

        config.llm.orchestrator = Some(zeph_core::config::OrchestratorConfig {
            providers,
            routes: HashMap::new(),
            default: "candle_local".to_string(),
            embed: "candle_local".to_string(),
        });

        let result = build_orchestrator(&config);
        assert!(result.is_err(), "expected error loading nonexistent model");
    }

    #[cfg(feature = "orchestrator")]
    #[test]
    fn build_orchestrator_with_ollama_sub_provider() {
        use std::collections::HashMap;
        use zeph_core::config::OrchestratorProviderConfig;

        let mut config = Config::load(Path::new("/nonexistent")).unwrap();
        config.llm.provider = ProviderKind::Orchestrator;

        let mut providers = HashMap::new();
        providers.insert(
            "ollama_sub".to_string(),
            OrchestratorProviderConfig {
                provider_type: "ollama".to_string(),
                model: Some("llama2".to_string()),
                filename: None,
                device: None,
            },
        );

        config.llm.orchestrator = Some(zeph_core::config::OrchestratorConfig {
            providers,
            routes: HashMap::new(),
            default: "ollama_sub".to_string(),
            embed: "ollama_sub".to_string(),
        });

        let result = build_orchestrator(&config);
        assert!(result.is_ok());
    }

    #[cfg(feature = "orchestrator")]
    #[test]
    fn build_orchestrator_routes_parsing() {
        use std::collections::HashMap;
        use zeph_core::config::OrchestratorProviderConfig;

        let mut config = Config::load(Path::new("/nonexistent")).unwrap();
        config.llm.provider = ProviderKind::Orchestrator;

        let mut providers = HashMap::new();
        providers.insert(
            "ollama_sub".to_string(),
            OrchestratorProviderConfig {
                provider_type: "ollama".to_string(),
                model: None,
                filename: None,
                device: None,
            },
        );

        let mut routes = HashMap::new();
        routes.insert("chat".to_string(), vec!["ollama_sub".to_string()]);
        routes.insert("embed".to_string(), vec!["ollama_sub".to_string()]);

        config.llm.orchestrator = Some(zeph_core::config::OrchestratorConfig {
            providers,
            routes,
            default: "ollama_sub".to_string(),
            embed: "ollama_sub".to_string(),
        });

        let result = build_orchestrator(&config);
        assert!(result.is_ok());
    }

    #[cfg(feature = "openai")]
    #[test]
    fn create_provider_openai_missing_config_errors() {
        let mut config = Config::load(Path::new("/nonexistent")).unwrap();
        config.llm.provider = ProviderKind::OpenAi;
        config.llm.openai = None;
        let result = create_provider(&config);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("llm.openai config section required")
        );
    }

    #[test]
    fn effective_embedding_model_defaults_to_llm() {
        let config = Config::load(Path::new("/nonexistent")).unwrap();
        assert_eq!(effective_embedding_model(&config), "qwen3-embedding");
    }

    #[cfg(feature = "openai")]
    #[test]
    fn effective_embedding_model_uses_openai_when_set() {
        let mut config = Config::load(Path::new("/nonexistent")).unwrap();
        config.llm.provider = ProviderKind::OpenAi;
        config.llm.openai = Some(zeph_core::config::OpenAiConfig {
            base_url: "https://api.openai.com/v1".into(),
            model: "gpt-5.2".into(),
            max_tokens: 4096,
            embedding_model: Some("text-embedding-3-small".into()),
            reasoning_effort: None,
        });
        assert_eq!(effective_embedding_model(&config), "text-embedding-3-small");
    }

    #[cfg(feature = "openai")]
    #[test]
    fn effective_embedding_model_falls_back_when_openai_embed_missing() {
        let mut config = Config::load(Path::new("/nonexistent")).unwrap();
        config.llm.provider = ProviderKind::OpenAi;
        config.llm.openai = Some(zeph_core::config::OpenAiConfig {
            base_url: "https://api.openai.com/v1".into(),
            model: "gpt-5.2".into(),
            max_tokens: 4096,
            embedding_model: None,
            reasoning_effort: None,
        });
        assert_eq!(effective_embedding_model(&config), "qwen3-embedding");
    }

    #[cfg(feature = "openai")]
    #[test]
    fn create_provider_openai_missing_api_key_errors() {
        let mut config = Config::load(Path::new("/nonexistent")).unwrap();
        config.llm.provider = ProviderKind::OpenAi;
        config.llm.openai = Some(zeph_core::config::OpenAiConfig {
            base_url: "https://api.openai.com/v1".into(),
            model: "gpt-4o".into(),
            max_tokens: 4096,
            embedding_model: None,
            reasoning_effort: None,
        });
        config.secrets.openai_api_key = None;
        let result = create_provider(&config);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("ZEPH_OPENAI_API_KEY not found")
        );
    }
}
