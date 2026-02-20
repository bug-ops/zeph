mod init;

use std::path::PathBuf;
#[cfg(feature = "tui")]
use std::time::Duration;

use clap::{Parser, Subcommand};
use zeph_core::vault::AgeVaultProvider;

#[cfg(any(feature = "a2a", feature = "tui", feature = "scheduler"))]
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
#[cfg(feature = "scheduler")]
use zeph_scheduler::{
    JobStore, ScheduledTask, Scheduler, TaskHandler, TaskKind, UpdateCheckHandler,
};
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

#[derive(Parser)]
#[command(
    name = "zeph",
    version,
    about = "Lightweight AI agent with hybrid inference"
)]
struct Cli {
    /// Run with TUI dashboard
    #[arg(long)]
    tui: bool,

    /// Path to config file
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Secrets backend: "env" or "age"
    #[arg(long, value_name = "BACKEND")]
    vault: Option<String>,

    /// Path to age identity (private key) file
    #[arg(long, value_name = "PATH")]
    vault_key: Option<PathBuf>,

    /// Path to age-encrypted secrets file
    #[arg(long, value_name = "PATH")]
    vault_path: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Interactive configuration wizard
    Init {
        /// Output path for generated config
        #[arg(long, short, value_name = "PATH")]
        output: Option<PathBuf>,
    },
    /// Manage the age-encrypted secrets vault
    Vault {
        #[command(subcommand)]
        command: VaultCommand,
    },
}

#[derive(Subcommand)]
enum VaultCommand {
    /// Generate age keypair and empty encrypted vault
    Init,
    /// Encrypt and store a secret.
    /// Note: VALUE is visible in process listing (ps/history). For sensitive values
    /// prefer setting the variable in the shell and passing via env instead.
    Set {
        #[arg()]
        key: String,
        #[arg()]
        value: String,
    },
    /// Decrypt and print a secret value
    Get {
        #[arg()]
        key: String,
    },
    /// List stored secret keys (no values)
    List,
    /// Remove a secret
    Rm {
        #[arg()]
        key: String,
    },
}

#[tokio::main]
#[allow(clippy::too_many_lines)]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Command::Init { output }) => return init::run(output),
        Some(Command::Vault { command: vault_cmd }) => {
            return handle_vault_command(
                vault_cmd,
                cli.vault_key.as_deref(),
                cli.vault_path.as_deref(),
            );
        }
        None => {}
    }

    #[cfg(feature = "tui")]
    let tui_active = cli.tui;
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
    init_subscriber(&resolve_config_path(cli.config.as_deref()));

    let app = AppBuilder::new(
        cli.config.as_deref(),
        cli.vault.as_deref(),
        cli.vault_key.as_deref(),
        cli.vault_path.as_deref(),
    )
    .await?;
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

    let cli_history = {
        let entries = memory
            .sqlite()
            .load_input_history(1000)
            .await
            .unwrap_or_default();
        let store = memory.sqlite().clone();
        let persist: Box<dyn Fn(&str) + Send> = Box::new(move |text: &str| {
            let store = store.clone();
            let text = text.to_owned();
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    if let Err(e) = store.save_input_entry(&text).await {
                        tracing::warn!("failed to persist input history entry: {e}");
                    }
                });
            }
        });
        Some((entries, persist))
    };

    #[cfg(feature = "tui")]
    let (channel, tui_handle) =
        create_channel_with_tui(app.config(), tui_active, cli_history).await?;
    #[cfg(not(feature = "tui"))]
    let channel = create_channel_inner(app.config(), cli_history).await?;

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
    .with_disambiguation_threshold(config.skills.disambiguation_threshold)
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
    .with_shutdown(shutdown_rx.clone())
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

    #[cfg(feature = "scheduler")]
    let agent = bootstrap_scheduler(agent, config, shutdown_rx.clone()).await;

    #[cfg(feature = "candle")]
    let agent = if config
        .llm
        .stt
        .as_ref()
        .is_some_and(|s| s.provider == "candle-whisper")
    {
        let stt_cfg = config.llm.stt.as_ref();
        let model = stt_cfg.map_or("openai/whisper-tiny", |s| s.model.as_str());
        let language = stt_cfg.map_or("auto", |s| s.language.as_str());
        match zeph_llm::candle_whisper::CandleWhisperProvider::load(model, None, language) {
            Ok(provider) => {
                tracing::info!("STT enabled via candle-whisper (model: {model})");
                agent.with_stt(Box::new(provider))
            }
            Err(e) => {
                tracing::error!("failed to load candle-whisper: {e}");
                agent
            }
        }
    } else {
        agent
    };

    #[cfg(feature = "stt")]
    let agent = if let Some(ref stt_cfg) = config.llm.stt {
        if stt_cfg.provider == "candle-whisper" {
            agent
        } else {
            let base_url = stt_cfg.base_url.as_deref().unwrap_or_else(|| {
                config
                    .llm
                    .openai
                    .as_ref()
                    .map_or("https://api.openai.com/v1", |o| o.base_url.as_str())
            });
            let api_key = config
                .secrets
                .openai_api_key
                .as_ref()
                .map_or(String::new(), |k| k.expose().to_string());
            let whisper = zeph_llm::whisper::WhisperProvider::new(
                zeph_core::http::default_client(),
                api_key,
                base_url,
                &stt_cfg.model,
            )
            .with_language(&stt_cfg.language);
            tracing::info!(
                model = stt_cfg.model,
                base_url,
                "STT enabled via Whisper API"
            );
            agent.with_stt(Box::new(whisper))
        }
    } else {
        agent
    };

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

    // Double Ctrl+C: first cancels current operation, second within 2s shuts down
    let cancel_signal = agent.cancel_signal();
    tokio::spawn(async move {
        let mut last_ctrl_c: Option<tokio::time::Instant> = None;
        loop {
            if tokio::signal::ctrl_c().await.is_err() {
                break;
            }
            let now = tokio::time::Instant::now();
            if let Some(prev) = last_ctrl_c
                && now.duration_since(prev) < std::time::Duration::from_secs(2)
            {
                tracing::info!("received second ctrl-c, shutting down");
                let _ = shutdown_tx.send(true);
                break;
            }
            tracing::info!("received ctrl-c, cancelling current operation");
            cancel_signal.notify_waiters();
            last_ctrl_c = Some(now);
        }
    });

    agent.load_history().await?;

    #[cfg(feature = "tui")]
    if let Some(tui_handle) = tui_handle {
        let (event_tx, event_rx) = tokio::sync::mpsc::channel(256);

        let reader = EventReader::new(event_tx, Duration::from_millis(100));
        std::thread::spawn(move || reader.run());

        let mut tui_app = App::new(tui_handle.user_tx, tui_handle.agent_rx)
            .with_cancel_signal(agent.cancel_signal())
            .with_command_tx(tui_handle.command_tx);
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
        tokio::spawn(forward_tui_commands(
            tui_handle.command_rx,
            agent_tx.clone(),
            TuiCommandContext {
                provider: format!("{:?}", config.llm.provider),
                model: config.llm.model.clone(),
                agent_name: config.agent.name.clone(),
                semantic_enabled: config.memory.semantic.enabled,
                autonomy_level: format!("{:?}", config.security.autonomy_level),
                max_tool_iterations: config.agent.max_tool_iterations,
            },
        ));

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
                result??;
                return Ok(());
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

fn default_vault_dir() -> PathBuf {
    zeph_core::vault::default_vault_dir()
}

fn handle_vault_command(
    cmd: VaultCommand,
    key_path: Option<&std::path::Path>,
    vault_path: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    let dir = default_vault_dir();
    let key_path_owned = key_path.map_or_else(|| dir.join("vault-key.txt"), PathBuf::from);
    let vault_path_owned = vault_path.map_or_else(|| dir.join("secrets.age"), PathBuf::from);

    match cmd {
        VaultCommand::Init => {
            AgeVaultProvider::init_vault(&dir)
                .map_err(|e| anyhow::anyhow!("vault init failed: {e}"))?;
        }
        VaultCommand::Set { key, value } => {
            let mut provider = AgeVaultProvider::load(&key_path_owned, &vault_path_owned)
                .map_err(|e| anyhow::anyhow!("failed to load vault: {e}"))?;
            provider.set_secret_mut(key, value);
            provider
                .save()
                .map_err(|e| anyhow::anyhow!("failed to save vault: {e}"))?;
        }
        VaultCommand::Get { key } => {
            let provider = AgeVaultProvider::load(&key_path_owned, &vault_path_owned)
                .map_err(|e| anyhow::anyhow!("failed to load vault: {e}"))?;
            if let Some(val) = provider.get(&key) {
                println!("{val}"); // lgtm[rust/cleartext-logging]
            } else {
                anyhow::bail!("key not found: {key}");
            }
        }
        VaultCommand::List => {
            let provider = AgeVaultProvider::load(&key_path_owned, &vault_path_owned)
                .map_err(|e| anyhow::anyhow!("failed to load vault: {e}"))?;
            for key in provider.list_keys() {
                println!("{key}");
            }
        }
        VaultCommand::Rm { key } => {
            let mut provider = AgeVaultProvider::load(&key_path_owned, &vault_path_owned)
                .map_err(|e| anyhow::anyhow!("failed to load vault: {e}"))?;
            if !provider.remove_secret_mut(&key) {
                anyhow::bail!("key not found: {key}");
            }
            provider
                .save()
                .map_err(|e| anyhow::anyhow!("failed to save vault: {e}"))?;
        }
    }

    Ok(())
}

async fn forward_status_to_stderr(mut rx: tokio::sync::mpsc::UnboundedReceiver<String>) {
    while let Some(msg) = rx.recv().await {
        eprintln!("[status] {msg}");
    }
}

// SECURITY: non-secret fields only
#[cfg(feature = "tui")]
struct TuiCommandContext {
    provider: String,
    model: String,
    agent_name: String,
    semantic_enabled: bool,
    autonomy_level: String,
    max_tool_iterations: usize,
}

#[cfg(feature = "tui")]
async fn forward_tui_commands(
    mut rx: tokio::sync::mpsc::Receiver<zeph_tui::TuiCommand>,
    tx: tokio::sync::mpsc::Sender<zeph_tui::AgentEvent>,
    ctx: TuiCommandContext,
) {
    while let Some(cmd) = rx.recv().await {
        let (command_id, output) = match cmd {
            zeph_tui::TuiCommand::ViewConfig => {
                let text = format!(
                    "Active configuration:\n  Provider: {}\n  Model: {}\n  Agent name: {}\n  Semantic enabled: {}",
                    ctx.provider, ctx.model, ctx.agent_name, ctx.semantic_enabled,
                );
                ("view:config".to_owned(), text)
            }
            zeph_tui::TuiCommand::ViewAutonomy => {
                let text = format!(
                    "Autonomy level: {}\n  Max tool iterations: {}",
                    ctx.autonomy_level, ctx.max_tool_iterations,
                );
                ("view:autonomy".to_owned(), text)
            }
            _ => continue,
        };
        if tx
            .send(zeph_tui::AgentEvent::CommandResult { command_id, output })
            .await
            .is_err()
        {
            break;
        }
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

type CliHistory = (Vec<String>, Box<dyn Fn(&str) + Send>);

#[allow(clippy::unused_async)]
async fn create_channel_inner(
    config: &Config,
    history: Option<CliHistory>,
) -> anyhow::Result<AnyChannel> {
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

    if let Some((entries, persist_fn)) = history {
        let cli = CliChannel::with_history(entries, persist_fn);
        return Ok(AnyChannel::Cli(cli));
    }

    Ok(AnyChannel::Cli(CliChannel::new()))
}

#[cfg(feature = "tui")]
struct TuiHandle {
    user_tx: tokio::sync::mpsc::Sender<String>,
    agent_tx: tokio::sync::mpsc::Sender<zeph_tui::AgentEvent>,
    agent_rx: tokio::sync::mpsc::Receiver<zeph_tui::AgentEvent>,
    command_tx: tokio::sync::mpsc::Sender<zeph_tui::TuiCommand>,
    command_rx: tokio::sync::mpsc::Receiver<zeph_tui::TuiCommand>,
}

#[cfg(feature = "tui")]
async fn create_channel_with_tui(
    config: &Config,
    tui_active: bool,
    history: Option<CliHistory>,
) -> anyhow::Result<(AppChannel, Option<TuiHandle>)> {
    if tui_active {
        let (user_tx, user_rx) = tokio::sync::mpsc::channel(32);
        let (agent_tx, agent_rx) = tokio::sync::mpsc::channel(256);
        let agent_tx_clone = agent_tx.clone();
        // command_tx goes to App; command_rx is handled by forward_tui_commands task.
        let (command_tx, command_rx) = tokio::sync::mpsc::channel::<zeph_tui::TuiCommand>(16);
        let channel = TuiChannel::new(user_rx, agent_tx);
        let handle = TuiHandle {
            user_tx,
            agent_tx: agent_tx_clone,
            agent_rx,
            command_tx,
            command_rx,
        };
        return Ok((AppChannel::Tui(channel), Some(handle)));
    }
    let channel = create_channel_inner(config, history).await?;
    Ok((AppChannel::Standard(channel), None))
}

#[allow(dead_code)]
async fn create_channel(config: &Config) -> anyhow::Result<AnyChannel> {
    create_channel_inner(config, None).await
}

#[cfg(feature = "scheduler")]
async fn bootstrap_scheduler<C, T>(
    agent: zeph_core::agent::Agent<C, T>,
    config: &Config,
    shutdown_rx: watch::Receiver<bool>,
) -> zeph_core::agent::Agent<C, T>
where
    C: zeph_core::channel::Channel,
    T: zeph_tools::executor::ToolExecutor,
{
    if !config.scheduler.enabled {
        if config.agent.auto_update_check {
            // Fire-and-forget single check at startup when scheduler is disabled.
            let (tx, rx) = tokio::sync::mpsc::channel(1);
            let handler = UpdateCheckHandler::new(env!("CARGO_PKG_VERSION"), tx);
            tokio::spawn(async move {
                let _ = handler.execute(&serde_json::Value::Null).await;
            });
            return agent.with_update_notifications(rx);
        }
        return agent;
    }

    let store = match JobStore::open(&config.memory.sqlite_path).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("scheduler: failed to open store: {e}");
            return agent;
        }
    };

    let mut scheduler = Scheduler::new(store, shutdown_rx);

    let agent = if config.agent.auto_update_check {
        let (update_tx, update_rx) = tokio::sync::mpsc::channel(4);
        let update_task = match ScheduledTask::new(
            "update_check",
            "0 0 9 * * *",
            TaskKind::UpdateCheck,
            serde_json::Value::Null,
        ) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!("scheduler: invalid update_check cron: {e}");
                return agent;
            }
        };
        scheduler.add_task(update_task);
        scheduler.register_handler(
            &TaskKind::UpdateCheck,
            Box::new(UpdateCheckHandler::new(
                env!("CARGO_PKG_VERSION"),
                update_tx,
            )),
        );
        agent.with_update_notifications(update_rx)
    } else {
        agent
    };

    if let Err(e) = scheduler.init().await {
        tracing::warn!("scheduler init failed: {e}");
        return agent;
    }

    tokio::spawn(async move { scheduler.run().await });
    tracing::info!("scheduler started");

    agent
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

#[cfg(all(feature = "otel", not(feature = "tui")))]
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
    use serial_test::serial;
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
        assert_eq!(config.llm.provider, ProviderKind::Ollama);
        assert_eq!(config.llm.model, "llama3");
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
        assert_eq!(config.llm.provider, ProviderKind::Claude);
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
        assert!(config.llm.compatible.is_some());
        let compat = config.llm.compatible.unwrap();
        assert_eq!(compat[0].name, "groq");
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

    // R-02: default_vault_dir() env var code paths
    #[test]
    #[serial]
    fn default_vault_dir_xdg_config_home() {
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", "/tmp/xdg-test");
        }
        let dir = default_vault_dir();
        unsafe {
            std::env::remove_var("XDG_CONFIG_HOME");
        }
        assert_eq!(dir, PathBuf::from("/tmp/xdg-test/zeph"));
    }

    #[test]
    #[serial]
    fn default_vault_dir_appdata() {
        unsafe {
            std::env::remove_var("XDG_CONFIG_HOME");
            std::env::set_var("APPDATA", "/tmp/appdata-test");
        }
        let dir = default_vault_dir();
        unsafe {
            std::env::remove_var("APPDATA");
        }
        assert_eq!(dir, PathBuf::from("/tmp/appdata-test/zeph"));
    }

    #[test]
    #[serial]
    fn default_vault_dir_home_fallback() {
        unsafe {
            std::env::remove_var("XDG_CONFIG_HOME");
            std::env::remove_var("APPDATA");
            std::env::set_var("HOME", "/tmp/home-test");
        }
        let dir = default_vault_dir();
        unsafe {
            std::env::remove_var("HOME");
        }
        assert_eq!(dir, PathBuf::from("/tmp/home-test/.config/zeph"));
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

    // R-01: handle_vault_command() dispatch branches
    #[test]
    fn handle_vault_command_set_get_list_rm() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("vault-key.txt");
        let vault_path = dir.path().join("secrets.age");

        zeph_core::vault::AgeVaultProvider::init_vault(dir.path()).unwrap();

        handle_vault_command(
            VaultCommand::Set {
                key: "FOO".into(),
                value: "bar".into(),
            },
            Some(&key_path),
            Some(&vault_path),
        )
        .unwrap();

        handle_vault_command(VaultCommand::List, Some(&key_path), Some(&vault_path)).unwrap();

        handle_vault_command(
            VaultCommand::Get { key: "FOO".into() },
            Some(&key_path),
            Some(&vault_path),
        )
        .unwrap();

        handle_vault_command(
            VaultCommand::Rm { key: "FOO".into() },
            Some(&key_path),
            Some(&vault_path),
        )
        .unwrap();
    }

    // R-04: Get/Rm missing-key error paths
    #[test]
    fn handle_vault_command_get_missing_key_errors() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("vault-key.txt");
        let vault_path = dir.path().join("secrets.age");
        zeph_core::vault::AgeVaultProvider::init_vault(dir.path()).unwrap();

        let err = handle_vault_command(
            VaultCommand::Get {
                key: "NONEXISTENT".into(),
            },
            Some(&key_path),
            Some(&vault_path),
        )
        .unwrap_err();
        assert!(err.to_string().contains("key not found"));
    }

    #[test]
    fn handle_vault_command_rm_missing_key_errors() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("vault-key.txt");
        let vault_path = dir.path().join("secrets.age");
        zeph_core::vault::AgeVaultProvider::init_vault(dir.path()).unwrap();

        let err = handle_vault_command(
            VaultCommand::Rm {
                key: "NONEXISTENT".into(),
            },
            Some(&key_path),
            Some(&vault_path),
        )
        .unwrap_err();
        assert!(err.to_string().contains("key not found"));
    }
}
