use std::path::PathBuf;
#[cfg(feature = "tui")]
use std::time::Duration;

#[cfg(any(feature = "a2a", feature = "tui"))]
use tokio::sync::watch;
use zeph_channels::AnyChannel;
use zeph_channels::CliChannel;
#[cfg(feature = "discord")]
use zeph_channels::discord::DiscordChannel;
#[cfg(feature = "slack")]
use zeph_channels::slack::SlackChannel;
use zeph_channels::telegram::TelegramChannel;
use zeph_core::agent::Agent;
#[cfg(not(feature = "tui"))]
use zeph_core::bootstrap::resolve_config_path;
use zeph_core::bootstrap::{AppBuilder, create_mcp_registry, warmup_provider};
#[cfg(feature = "tui")]
use zeph_core::channel::{Channel, ChannelError, ChannelMessage};
use zeph_core::config::Config;
use zeph_core::cost::CostTracker;
#[cfg(feature = "index")]
use zeph_index::{
    indexer::{CodeIndexer, IndexerConfig},
    retriever::{CodeRetriever, RetrievalConfig},
    store::CodeStore,
    watcher::IndexWatcher,
};
#[cfg(feature = "a2a")]
use zeph_llm::any::AnyProvider;
#[cfg(feature = "index")]
use zeph_llm::provider::LlmProvider;
#[cfg(feature = "tui")]
use zeph_tui::{App, EventReader, TuiChannel};

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
    async fn send_diff(&mut self, diff: zeph_core::DiffData) -> Result<(), ChannelError> {
        dispatch_app_channel!(self, send_diff, diff)
    }
    async fn send_tool_output(
        &mut self,
        tool_name: &str,
        display: &str,
        diff: Option<zeph_core::DiffData>,
        filter_stats: Option<String>,
    ) -> Result<(), ChannelError> {
        dispatch_app_channel!(
            self,
            send_tool_output,
            tool_name,
            display,
            diff,
            filter_stats
        )
    }
}

#[tokio::main]
#[allow(clippy::too_many_lines)]
async fn main() -> anyhow::Result<()> {
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
                .with_line_number(true)
                .init();
        } else {
            tracing_subscriber::fmt().with_env_filter(filter).init();
        }
    } else {
        tracing_subscriber::fmt::init();
    }
    #[cfg(not(feature = "tui"))]
    init_subscriber(&resolve_config_path());

    let app = AppBuilder::from_env().await?;
    let (provider, status_rx) = app.build_provider().await?;
    let embed_model = app.embedding_model();
    let budget_tokens = app.auto_budget_tokens(&provider);

    let registry = app.build_registry();
    let memory = app.build_memory(&provider).await?;

    let all_meta = registry.all_meta();
    let matcher = app.build_skill_matcher(&provider, &all_meta, &memory).await;
    let skill_count = all_meta.len();
    if matcher.is_some() {
        tracing::info!("skill matcher initialized for {skill_count} skill(s)");
    } else {
        tracing::info!("skill matcher unavailable, using all {skill_count} skill(s)");
    }

    #[cfg(feature = "tui")]
    let (channel, tui_handle) = create_channel_with_tui(app.config()).await?;
    #[cfg(not(feature = "tui"))]
    let channel = create_channel(app.config()).await?;

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

    let (shutdown_tx, shutdown_rx) = AppBuilder::build_shutdown();
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

    let config = app.config();
    let permission_policy = config
        .tools
        .permission_policy(config.security.autonomy_level);
    let skill_paths = app.skill_paths();

    #[allow(unused_variables)]
    let (tool_executor, mcp_tools, mcp_manager, shell_executor_for_tui) = {
        let filter_registry = if config.tools.filters.enabled {
            zeph_tools::OutputFilterRegistry::default_filters(&config.tools.filters)
        } else {
            zeph_tools::OutputFilterRegistry::new(false)
        };
        let mut shell_executor = zeph_tools::ShellExecutor::new(&config.tools.shell)
            .with_permissions(permission_policy.clone())
            .with_output_filters(filter_registry);
        if config.tools.audit.enabled
            && let Ok(logger) = zeph_tools::AuditLogger::from_config(&config.tools.audit).await
        {
            shell_executor = shell_executor.with_audit(logger);
        }

        #[cfg(feature = "tui")]
        let tool_event_rx = if tui_handle.is_some() {
            let (tool_tx, tool_rx) =
                tokio::sync::mpsc::unbounded_channel::<zeph_tools::ToolEvent>();
            shell_executor = shell_executor.with_tool_event_tx(tool_tx);
            Some(tool_rx)
        } else {
            None
        };

        let scrape_executor = zeph_tools::WebScrapeExecutor::new(&config.tools.scrape);
        let file_executor = zeph_tools::FileExecutor::new(
            config
                .tools
                .shell
                .allowed_paths
                .iter()
                .map(PathBuf::from)
                .collect(),
        );

        let mcp_manager = std::sync::Arc::new(zeph_core::bootstrap::create_mcp_manager(config));
        let mcp_tools = mcp_manager.connect_all().await;
        tracing::info!("discovered {} MCP tool(s)", mcp_tools.len());

        let mcp_executor = zeph_mcp::McpToolExecutor::new(mcp_manager.clone());
        let base_executor = zeph_tools::CompositeExecutor::new(
            file_executor,
            zeph_tools::CompositeExecutor::new(shell_executor, scrape_executor),
        );
        let executor = zeph_tools::CompositeExecutor::new(base_executor, mcp_executor);

        #[cfg(feature = "tui")]
        let shell_for_tui = tool_event_rx;
        #[cfg(not(feature = "tui"))]
        let shell_for_tui = ();

        (executor, mcp_tools, mcp_manager, shell_for_tui)
    };

    let watchers = app.build_watchers();
    let _skill_watcher = watchers.skill_watcher;
    let reload_rx = watchers.skill_reload_rx;
    let _config_watcher = watchers.config_watcher;
    let config_reload_rx = watchers.config_reload_rx;

    #[cfg(feature = "a2a")]
    if config.a2a.enabled {
        let a2a_provider = std::sync::Arc::new(provider.clone());
        let skill_names: Vec<&str> = all_meta.iter().map(|m| m.name.as_str()).collect();
        let a2a_system_prompt = format!(
            "You are {}. Available skills: {}",
            config.agent.name,
            skill_names.join(", ")
        );
        spawn_a2a_server(config, shutdown_rx.clone(), a2a_provider, a2a_system_prompt);
    }

    let mcp_registry = create_mcp_registry(config, &provider, &mcp_tools, &embed_model).await;

    #[cfg(feature = "index")]
    let index_pool = memory.sqlite().pool().clone();
    #[cfg(feature = "index")]
    let index_provider = provider.clone();
    #[cfg(feature = "index")]
    let provider_has_tools = provider.supports_tool_use();
    let warmup_provider_clone = provider.clone();

    let summary_provider = app.build_summary_provider();
    let config = app.config();
    let config_path = app.config_path().to_owned();

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
    .with_skill_reload(skill_paths.clone(), reload_rx)
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
    .with_config_reload(config_path, config_reload_rx);

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

    let agent = agent.with_mcp(mcp_tools, mcp_registry, Some(mcp_manager), &config.mcp);
    let agent = agent.with_learning(config.skills.learning.clone());

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

        let mut tui_app = App::new(tui_handle.user_tx, tui_handle.agent_rx);
        tui_app.set_show_source_labels(config.tui.show_source_labels);

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
        tui_app.load_history(&history);

        if let Some(rx) = tui_metrics_rx {
            tui_app = tui_app.with_metrics_rx(rx);
        }

        let agent_tx = tui_handle.agent_tx;
        tokio::spawn(forward_status_to_tui(status_rx, agent_tx.clone()));

        if let Some(tool_rx) = shell_executor_for_tui {
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
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            let _ = warmup_agent_tx
                .send(zeph_tui::AgentEvent::Status(String::new()))
                .await;
        });

        let mut agent = agent.with_warmup_ready(warmup_rx);

        let tui_task = tokio::spawn(zeph_tui::run_tui(tui_app, event_rx));
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
                chunk: zeph_tools::strip_ansi(&chunk),
            },
            zeph_tools::ToolEvent::Completed {
                tool_name,
                command,
                output,
                success,
                diff,
                filter_stats,
            } => {
                let stats_line = filter_stats.as_ref().and_then(|fs| {
                    (fs.filtered_chars < fs.raw_chars)
                        .then(|| format!("{:.1}% filtered", fs.savings_pct()))
                });
                zeph_tui::AgentEvent::ToolOutput {
                    tool_name,
                    command,
                    output,
                    success,
                    diff,
                    filter_stats: stats_line,
                }
            }
        };
        if tx.send(agent_event).await.is_err() {
            break;
        }
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

#[cfg(not(feature = "tui"))]
fn init_subscriber(config_path: &std::path::Path) {
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
    use std::path::Path;
    use zeph_core::channel::Channel;
    use zeph_core::config::ProviderKind;
    #[cfg(feature = "a2a")]
    use zeph_llm::ollama::OllamaProvider;

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
}
