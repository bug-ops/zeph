// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::agent_setup;
use crate::channel::build_cli_history;
#[cfg(not(feature = "tui"))]
use crate::channel::create_channel_inner;
#[cfg(feature = "tui")]
use crate::channel::{AppChannel, create_channel_with_tui};
use crate::cli::Cli;
#[cfg(feature = "scheduler")]
use crate::scheduler::bootstrap_scheduler;
#[cfg(any(feature = "acp", feature = "acp-http", feature = "tui"))]
use crate::tracing_init::init_file_logger;
#[cfg(not(feature = "tui"))]
use crate::tracing_init::init_subscriber;
use crate::tui_bridge::forward_status_to_stderr;
#[cfg(feature = "tui")]
use crate::tui_bridge::{TuiRunParams, run_tui_agent};

use zeph_channels::AnyChannel;
use zeph_core::agent::Agent;
#[cfg(not(feature = "tui"))]
use zeph_core::bootstrap::resolve_config_path;
use zeph_core::bootstrap::{AppBuilder, create_mcp_registry, warmup_provider};
use zeph_core::vault::Secret;

#[cfg(feature = "acp-http")]
use crate::acp::run_acp_http_server;
#[cfg(feature = "acp")]
use crate::acp::{print_acp_manifest, run_acp_server};
use crate::cli::Command;
use crate::commands::memory::handle_memory_command;
use crate::commands::skill::handle_skill_command;
use crate::commands::vault::handle_vault_command;
#[cfg(all(feature = "daemon", feature = "a2a"))]
use crate::daemon::run_daemon;
#[cfg(all(feature = "tui", feature = "a2a"))]
use crate::tui_remote::run_tui_remote;
#[cfg(feature = "index")]
use zeph_llm::provider::LlmProvider;

#[allow(clippy::too_many_lines)]
pub(crate) async fn run(cli: Cli) -> anyhow::Result<()> {
    match cli.command {
        Some(Command::Init { output }) => return crate::init::run(output),
        Some(Command::Vault { command: vault_cmd }) => {
            return handle_vault_command(
                vault_cmd,
                cli.vault_key.as_deref(),
                cli.vault_path.as_deref(),
            );
        }
        Some(Command::Skill { command: skill_cmd }) => {
            tracing_subscriber::fmt::init();
            return handle_skill_command(skill_cmd, cli.config.as_deref()).await;
        }
        Some(Command::Memory { command: mem_cmd }) => {
            tracing_subscriber::fmt::init();
            return handle_memory_command(mem_cmd, cli.config.as_deref()).await;
        }
        None => {}
    }

    #[cfg(all(feature = "daemon", feature = "a2a"))]
    if cli.daemon {
        tracing_subscriber::fmt::init();
        return Box::pin(run_daemon(
            cli.config.as_deref(),
            cli.vault.as_deref(),
            cli.vault_key.as_deref(),
            cli.vault_path.as_deref(),
        ))
        .await;
    }

    #[cfg(feature = "acp")]
    if cli.acp_manifest {
        print_acp_manifest();
        return Ok(());
    }

    #[cfg(feature = "acp")]
    if cli.acp {
        init_file_logger();
        return run_acp_server(
            cli.config.as_deref(),
            cli.vault.as_deref(),
            cli.vault_key.as_deref(),
            cli.vault_path.as_deref(),
        )
        .await;
    }

    #[cfg(feature = "acp-http")]
    if cli.acp_http {
        init_file_logger();
        return run_acp_http_server(
            cli.config.as_deref(),
            cli.vault.as_deref(),
            cli.vault_key.as_deref(),
            cli.vault_path.as_deref(),
            cli.acp_http_bind.as_deref(),
            cli.acp_auth_token,
        )
        .await;
    }

    #[cfg(all(feature = "tui", feature = "a2a"))]
    if let Some(url) = cli.connect {
        init_file_logger();
        return run_tui_remote(url, cli.config.as_deref()).await;
    }

    #[cfg(feature = "tui")]
    let tui_active = cli.tui;
    #[cfg(feature = "tui")]
    if tui_active {
        init_file_logger();
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
    let memory = std::sync::Arc::new(app.build_memory(&provider).await?);

    let all_meta = registry.all_meta();
    let matcher = app.build_skill_matcher(&provider, &all_meta, &memory).await;
    let skill_count = all_meta.len();
    if matcher.is_some() {
        tracing::info!("skill matcher initialized for {skill_count} skill(s)");
    } else {
        tracing::info!("skill matcher unavailable, using all {skill_count} skill(s)");
    }

    let cli_history = build_cli_history(&memory).await;

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
    let config = app.config();

    {
        let overflow_cfg = config.tools.overflow.clone();
        tokio::task::spawn_blocking(move || {
            zeph_tools::cleanup_overflow_files(&overflow_cfg);
        });
    }

    let permission_policy = config
        .tools
        .permission_policy(config.security.autonomy_level);
    let skill_paths = app.skill_paths();

    #[cfg(feature = "tui")]
    let with_tool_events = tui_handle.is_some();
    #[cfg(not(feature = "tui"))]
    let with_tool_events = false;

    let tool_setup =
        agent_setup::build_tool_setup(config, permission_policy.clone(), with_tool_events).await;
    let memory_executor = zeph_core::memory_tools::MemoryToolExecutor::new(
        std::sync::Arc::clone(&memory),
        conversation_id,
    );
    let base: std::sync::Arc<dyn zeph_tools::ErasedToolExecutor> =
        std::sync::Arc::new(tool_setup.executor);
    let tool_executor = zeph_tools::DynExecutor(std::sync::Arc::new(
        zeph_tools::CompositeExecutor::new(memory_executor, zeph_tools::DynExecutor(base)),
    ));
    let mcp_tools = tool_setup.mcp_tools;
    let mcp_manager = tool_setup.mcp_manager;
    let mcp_shared_tools = tool_setup.mcp_shared_tools;
    #[cfg(feature = "tui")]
    let shell_executor_for_tui = tool_setup.tool_event_rx;
    #[cfg(not(feature = "tui"))]
    let _tool_event_rx = tool_setup.tool_event_rx;

    let watchers = app.build_watchers();
    let _skill_watcher = watchers.skill_watcher;
    let reload_rx = watchers.skill_reload_rx;
    let _config_watcher = watchers.config_watcher;
    let config_reload_rx = watchers.config_reload_rx;

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
    let cache_pool = memory.sqlite().pool().clone();

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
    .with_managed_skills_dir(zeph_core::bootstrap::managed_skills_dir())
    .with_memory(
        std::sync::Arc::clone(&memory),
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
    .with_redact_credentials(config.memory.redact_credentials)
    .with_tool_summarization(config.tools.summarize_output)
    .with_overflow_config(config.tools.overflow.clone())
    .with_permission_policy(permission_policy.clone())
    .with_config_reload(config_path, config_reload_rx)
    .with_available_secrets(
        config
            .secrets
            .custom
            .iter()
            .map(|(k, v)| (k.clone(), Secret::new(v.expose().to_owned()))),
    )
    .with_autosave_config(
        config.memory.autosave_assistant,
        config.memory.autosave_min_length,
    )
    .with_tool_call_cutoff(config.memory.tool_call_cutoff);

    let agent = agent_setup::apply_response_cache(
        agent,
        config.llm.response_cache_enabled,
        cache_pool,
        config.llm.response_cache_ttl_secs,
    );
    let agent =
        agent_setup::apply_cost_tracker(agent, config.cost.enabled, config.cost.max_daily_cents);
    let agent = agent_setup::apply_summary_provider(agent, summary_provider);

    #[cfg(feature = "index")]
    let (agent, _index_watcher) = agent_setup::apply_code_index(
        agent,
        &config.index,
        &config.memory.qdrant_url,
        index_provider,
        index_pool,
        provider_has_tools,
    )
    .await;

    let agent = agent.with_mcp(mcp_tools, mcp_registry, Some(mcp_manager), &config.mcp);
    let agent = agent.with_mcp_shared_tools(mcp_shared_tools);
    let agent = agent.with_learning(config.skills.learning.clone());

    let agent = {
        let mut mgr = zeph_core::subagent::SubAgentManager::new(config.agents.max_concurrent);
        let mut agent_dirs: Vec<std::path::PathBuf> =
            vec![std::path::PathBuf::from(".zeph/agents")];
        if let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) {
            agent_dirs.push(home.join(".config/zeph/agents"));
        }
        agent_dirs.extend(config.agents.extra_dirs.clone());
        if let Err(e) = mgr.load_definitions(&agent_dirs) {
            tracing::warn!("sub-agent definition loading failed: {e:#}");
        }
        agent.with_subagent_manager(mgr)
    };

    #[cfg(feature = "scheduler")]
    let agent = bootstrap_scheduler(agent, config, shutdown_rx.clone()).await;

    #[cfg(feature = "candle")]
    let agent = agent_setup::apply_candle_stt(agent, config.llm.stt.as_ref());

    #[cfg(feature = "stt")]
    let agent = {
        let openai_base_url = config
            .llm
            .openai
            .as_ref()
            .map_or("https://api.openai.com/v1", |o| o.base_url.as_str())
            .to_owned();
        let api_key = config
            .secrets
            .openai_api_key
            .as_ref()
            .map_or(String::new(), |k| k.expose().to_string());
        agent_setup::apply_whisper_stt(agent, config.llm.stt.as_ref(), &openai_base_url, api_key)
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
    agent
        .check_vector_store_health(config.memory.vector_backend.as_str())
        .await;

    agent_setup::spawn_ctrl_c_handler(agent.cancel_signal(), shutdown_tx);
    agent.load_history().await?;

    #[cfg(feature = "tui")]
    if let Some(tui_handle) = tui_handle {
        return Box::pin(run_tui_agent(
            agent,
            TuiRunParams {
                tui_handle,
                config,
                status_rx,
                tool_rx: shell_executor_for_tui,
                metrics_rx: tui_metrics_rx,
                warmup_provider: warmup_provider_clone,
            },
        ))
        .await;
    }

    warmup_provider(&warmup_provider_clone).await;
    tokio::spawn(forward_status_to_stderr(status_rx));
    let result = Box::pin(agent.run()).await;
    agent.shutdown().await;
    result
}
