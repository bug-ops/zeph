// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

#![cfg(feature = "a2a")]

use std::path::PathBuf;

use crate::agent_setup;
#[cfg(feature = "gateway")]
use crate::gateway_spawn::spawn_gateway_server;
use tokio::sync::watch;
use zeph_core::agent::Agent;
use zeph_core::bootstrap::{AppBuilder, create_mcp_registry};
use zeph_core::config::Config;

fn spawn_a2a_server(
    config: &Config,
    shutdown_rx: watch::Receiver<bool>,
    loopback_handle: zeph_core::LoopbackHandle,
    sanitizer: zeph_core::ContentSanitizer,
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
            loopback_handle: std::sync::Arc::new(tokio::sync::Mutex::new(loopback_handle)),
            sanitizer,
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

pub(crate) struct AgentTaskProcessor {
    pub(crate) loopback_handle: std::sync::Arc<tokio::sync::Mutex<zeph_core::LoopbackHandle>>,
    pub(crate) sanitizer: zeph_core::ContentSanitizer,
}

impl zeph_a2a::TaskProcessor for AgentTaskProcessor {
    fn process(
        &self,
        _task_id: String,
        message: zeph_a2a::Message,
        event_tx: tokio::sync::mpsc::Sender<zeph_a2a::ProcessorEvent>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), zeph_a2a::A2aError>> + Send>>
    {
        let handle = self.loopback_handle.clone();
        let sanitizer = self.sanitizer.clone();

        Box::pin(async move {
            // Inbound A2A messages come from external agents and are treated as
            // ExternalUntrusted — sanitize before forwarding to the agent loop.
            // Use all_text_content() to concatenate ALL Part::Text entries; text_content()
            // returns only the first part and would silently drop subsequent text.
            let raw_text = message.all_text_content();
            let user_text = sanitizer
                .sanitize(
                    &raw_text,
                    zeph_core::ContentSource::new(zeph_core::ContentSourceKind::A2aMessage),
                )
                .body;
            let mut handle = handle.lock().await;

            handle
                .input_tx
                .send(zeph_core::ChannelMessage {
                    text: user_text,
                    attachments: vec![],
                })
                .await
                .map_err(|_| zeph_a2a::A2aError::Server("agent channel closed".to_owned()))?;

            event_tx
                .send(zeph_a2a::ProcessorEvent::StatusUpdate {
                    state: zeph_a2a::TaskState::Working,
                    is_final: false,
                })
                .await
                .map_err(|_| zeph_a2a::A2aError::Server("event channel closed".to_owned()))?;

            let mut exited_on_flush = false;
            while let Some(event) = handle.output_rx.recv().await {
                match event {
                    zeph_core::LoopbackEvent::Chunk(text) => {
                        let _ = event_tx
                            .send(zeph_a2a::ProcessorEvent::ArtifactChunk {
                                text,
                                is_final: false,
                            })
                            .await;
                    }
                    zeph_core::LoopbackEvent::Flush => {
                        let _ = event_tx
                            .send(zeph_a2a::ProcessorEvent::ArtifactChunk {
                                text: String::new(),
                                is_final: true,
                            })
                            .await;
                        exited_on_flush = true;
                        break;
                    }
                    zeph_core::LoopbackEvent::FullMessage(text) => {
                        let _ = event_tx
                            .send(zeph_a2a::ProcessorEvent::ArtifactChunk {
                                text,
                                is_final: true,
                            })
                            .await;
                        break;
                    }
                    zeph_core::LoopbackEvent::Status(_)
                    | zeph_core::LoopbackEvent::ToolStart(_)
                    | zeph_core::LoopbackEvent::ToolOutput(_)
                    | zeph_core::LoopbackEvent::Usage { .. }
                    | zeph_core::LoopbackEvent::SessionTitle(_)
                    | zeph_core::LoopbackEvent::Plan(_)
                    | zeph_core::LoopbackEvent::ThinkingChunk(_)
                    | zeph_core::LoopbackEvent::Stop(_) => {}
                }
            }

            // Wait for Flush — the definitive end-of-turn sentinel always emitted by the
            // agent loop after FullMessage or stop-hint paths. This prevents stale tail
            // events (e.g. the Flush that follows FullMessage, Usage, SessionTitle) from
            // leaking into the next request's recv loop.
            if !exited_on_flush {
                loop {
                    match handle.output_rx.recv().await {
                        Some(zeph_core::LoopbackEvent::Flush) | None => break,
                        Some(_) => {} // discard tail events
                    }
                }
            }

            let _ = event_tx
                .send(zeph_a2a::ProcessorEvent::StatusUpdate {
                    state: zeph_a2a::TaskState::Completed,
                    is_final: true,
                })
                .await;

            Ok(())
        })
    }
}

#[allow(clippy::too_many_lines)]
pub(crate) async fn run_daemon(
    config_path: Option<&std::path::Path>,
    vault: Option<&str>,
    vault_key: Option<&std::path::Path>,
    vault_path: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    use zeph_core::daemon::{
        ComponentHandle, DaemonSupervisor, is_process_alive, read_pid_file, remove_pid_file,
        write_pid_file,
    };

    let app = AppBuilder::new(config_path, vault, vault_key, vault_path).await?;
    let config = app.config();

    // Check for a stale or live PID file before writing a new one.
    if let Ok(existing_pid) = read_pid_file(&config.daemon.pid_file) {
        if is_process_alive(existing_pid) {
            anyhow::bail!(
                "another daemon instance is already running (PID {existing_pid}); \
                 stop it before starting a new one"
            );
        }
        tracing::info!(
            pid = existing_pid,
            "removing stale PID file from previous run"
        );
        if let Err(e) = remove_pid_file(&config.daemon.pid_file) {
            tracing::warn!("failed to remove stale PID file: {e}");
        }
    }
    if let Err(e) = write_pid_file(&config.daemon.pid_file) {
        tracing::warn!("failed to write PID file: {e}");
    }
    tracing::info!(pid_file = %config.daemon.pid_file, "daemon started");

    let (provider, status_tx, _status_rx) = app.build_provider().await?;
    let embed_model = app.embedding_model();
    let embedding_provider =
        zeph_core::bootstrap::create_embedding_provider(app.config(), &provider);
    let budget_tokens = app.auto_budget_tokens(&provider);

    let registry = std::sync::Arc::new(std::sync::RwLock::new(app.build_registry()));
    let memory = std::sync::Arc::new(app.build_memory(&provider).await?);
    let all_meta_owned: Vec<zeph_skills::loader::SkillMeta> = registry
        .read()
        .expect("registry read lock")
        .all_meta()
        .into_iter()
        .cloned()
        .collect();
    let all_meta_refs: Vec<&zeph_skills::loader::SkillMeta> = all_meta_owned.iter().collect();
    let matcher = app
        .build_skill_matcher(&provider, &all_meta_refs, &memory)
        .await;
    let skill_count = all_meta_owned.len();
    tracing::info!("skills loaded: {skill_count}");

    let conversation_id = match memory.sqlite().latest_conversation_id().await? {
        Some(id) => id,
        None => memory.sqlite().create_conversation().await?,
    };

    {
        let sqlite = memory.sqlite().clone();
        let retention_secs = config.tools.overflow.retention_days.saturating_mul(86_400);
        tokio::spawn(async move {
            match sqlite.cleanup_overflow(retention_secs).await {
                Ok(n) if n > 0 => tracing::info!("cleaned up {n} stale overflow entries"),
                Ok(_) => {}
                Err(e) => tracing::warn!("overflow cleanup failed: {e}"),
            }
        });
    }

    let (shutdown_tx, shutdown_rx) = AppBuilder::build_shutdown();

    let filter_registry = if config.tools.filters.enabled {
        zeph_tools::OutputFilterRegistry::default_filters(&config.tools.filters)
    } else {
        zeph_tools::OutputFilterRegistry::new(false)
    };
    let mut shell_executor = zeph_tools::ShellExecutor::new(&config.tools.shell)
        .with_permissions(
            config
                .tools
                .permission_policy(config.security.autonomy_level),
        )
        .with_output_filters(filter_registry);
    let mut scrape_executor = zeph_tools::WebScrapeExecutor::new(&config.tools.scrape);
    if config.tools.audit.enabled
        && let Ok(logger) = zeph_tools::AuditLogger::from_config(&config.tools.audit).await
    {
        let logger = std::sync::Arc::new(logger);
        shell_executor = shell_executor.with_audit(std::sync::Arc::clone(&logger));
        scrape_executor = scrape_executor.with_audit(logger);
    }
    let file_executor = zeph_tools::FileExecutor::new(
        config
            .tools
            .shell
            .allowed_paths
            .iter()
            .map(PathBuf::from)
            .collect(),
    );
    let mcp_manager = std::sync::Arc::new(
        zeph_core::bootstrap::create_mcp_manager_with_vault(config, false, app.age_vault_arc())
            .with_status_tx(status_tx),
    );
    let (mcp_tools, _mcp_outcomes) = mcp_manager.connect_all().await;
    let mcp_shared_tools = std::sync::Arc::new(std::sync::RwLock::new(mcp_tools.clone()));
    let mcp_executor =
        zeph_mcp::McpToolExecutor::new(mcp_manager.clone(), mcp_shared_tools.clone());
    let base_executor = zeph_tools::CompositeExecutor::new(
        file_executor,
        zeph_tools::CompositeExecutor::new(shell_executor, scrape_executor),
    );
    let memory_executor = zeph_core::memory_tools::MemoryToolExecutor::with_validator(
        std::sync::Arc::clone(&memory),
        conversation_id,
        zeph_sanitizer::memory_validation::MemoryWriteValidator::new(
            config.security.memory_validation.clone(),
        ),
    );
    let overflow_executor = zeph_core::overflow_tools::OverflowToolExecutor::new(
        std::sync::Arc::new(memory.sqlite().clone()),
    )
    .with_conversation(conversation_id.0);
    let skill_loader_executor =
        zeph_core::SkillLoaderExecutor::new(std::sync::Arc::clone(&registry));
    let base_tool: std::sync::Arc<dyn zeph_tools::ErasedToolExecutor> = std::sync::Arc::new(
        zeph_tools::CompositeExecutor::new(base_executor, mcp_executor),
    );
    let tool_executor =
        zeph_tools::DynExecutor(std::sync::Arc::new(zeph_tools::CompositeExecutor::new(
            skill_loader_executor,
            zeph_tools::CompositeExecutor::new(
                memory_executor,
                zeph_tools::CompositeExecutor::new(
                    overflow_executor,
                    zeph_tools::DynExecutor(base_tool),
                ),
            ),
        )));

    let mcp_registry = create_mcp_registry(
        config,
        &provider,
        &mcp_tools,
        &embed_model,
        app.qdrant_ops(),
    )
    .await;

    let watchers = app.build_watchers();
    let _skill_watcher = watchers.skill_watcher;
    let reload_rx = watchers.skill_reload_rx;
    let _config_watcher = watchers.config_watcher;
    let config_reload_rx = watchers.config_reload_rx;
    let skill_paths = app.skill_paths();
    let config_path_owned = app.config_path().to_owned();
    let session_config = zeph_core::AgentSessionConfig::from_config(config, budget_tokens);

    let (loopback_channel, loopback_handle) = zeph_core::LoopbackChannel::pair(64);

    let agent = Box::pin(
        Agent::new_with_registry_arc(
            provider.clone(),
            loopback_channel,
            registry,
            matcher,
            config.skills.max_active_skills,
            tool_executor,
        )
        .apply_session_config(session_config)
        .with_disambiguation_threshold(config.skills.disambiguation_threshold)
        .with_skill_reload(skill_paths, reload_rx)
        .with_managed_skills_dir(zeph_core::bootstrap::managed_skills_dir())
        .with_memory(
            std::sync::Arc::clone(&memory),
            conversation_id,
            config.memory.history_limit,
            config.memory.semantic.recall_limit,
            config.memory.summarization_threshold,
        )
        .with_shutdown(shutdown_rx.clone())
        .with_config_reload(config_path_owned, config_reload_rx)
        .with_mcp(mcp_tools, mcp_registry, Some(mcp_manager), &config.mcp)
        .with_mcp_shared_tools(mcp_shared_tools)
        .with_hybrid_search(config.skills.hybrid_search)
        .with_focus_config(config.agent.focus.clone())
        .with_sidequest_config(config.memory.sidequest.clone())
        .with_embedding_provider(embedding_provider)
        .maybe_init_tool_schema_filter(&config.agent.tool_filter, &provider),
    )
    .await;

    // Wire tool dependency graph if enabled (#2024).
    let agent = if config.tools.dependencies.enabled && !config.tools.dependencies.rules.is_empty()
    {
        let graph = zeph_tools::ToolDependencyGraph::new(config.tools.dependencies.rules.clone());
        let always_on: std::collections::HashSet<String> =
            config.agent.tool_filter.always_on.iter().cloned().collect();
        agent
            .with_tool_dependency_graph(graph, always_on)
            .with_dependency_config(config.tools.dependencies.clone())
    } else {
        agent
    };

    let summary_provider = app.build_summary_provider();
    let agent = if let Some(sp) = summary_provider {
        agent.with_summary_provider(sp)
    } else {
        agent
    };
    let probe_provider = app.build_probe_provider();
    let agent = if let Some(pp) = probe_provider {
        agent.with_probe_provider(pp)
    } else {
        agent
    };
    let planner_provider = app.build_planner_provider();
    let agent = if let Some(pp) = planner_provider {
        agent.with_planner_provider(pp)
    } else {
        agent
    };
    let agent = agent_setup::apply_quarantine_provider(agent, app.build_quarantine_provider());
    #[cfg(feature = "guardrail")]
    let agent = agent_setup::apply_guardrail(agent, app.build_guardrail_provider());
    #[cfg(feature = "classifiers")]
    let agent = agent_setup::apply_injection_classifier(agent, config);
    #[cfg(feature = "classifiers")]
    let agent = agent_setup::apply_pii_classifier(agent, config);

    let judge_provider = app.build_judge_provider();
    let agent = if let Some(jp) = judge_provider {
        agent.with_judge_provider(jp)
    } else {
        agent
    };
    let agent = if let Some(fc) = app.build_feedback_classifier(&provider) {
        agent.with_llm_classifier(fc)
    } else {
        agent
    };

    let agent = if config.cost.enabled {
        let tracker =
            zeph_core::cost::CostTracker::new(true, f64::from(config.cost.max_daily_cents));
        agent.with_cost_tracker(tracker)
    } else {
        agent
    };

    let agent = if config.tools.anomaly.enabled {
        agent.with_anomaly_detector(zeph_tools::AnomalyDetector::new(
            config.tools.anomaly.window_size,
            config.tools.anomaly.error_threshold,
            config.tools.anomaly.critical_threshold,
        ))
    } else {
        agent
    };

    let mut agent = agent
        .with_document_config(config.memory.documents.clone())
        .with_graph_config(config.memory.graph.clone());

    agent.load_history().await?;
    agent
        .check_vector_store_health(config.memory.vector_backend.as_str())
        .await;

    let a2a_sanitizer = zeph_core::ContentSanitizer::new(&config.security.content_isolation);
    spawn_a2a_server(config, shutdown_rx.clone(), loopback_handle, a2a_sanitizer);

    #[cfg(feature = "gateway")]
    if config.gateway.enabled {
        spawn_gateway_server(config, shutdown_rx.clone());
    }

    let pid_file = config.daemon.pid_file.clone();
    let mut supervisor = DaemonSupervisor::new(&config.daemon, shutdown_rx.clone());

    let shutdown_tx_signal = shutdown_tx.clone();
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            let mut sigterm =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("failed to register SIGTERM handler");
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    tracing::info!("received Ctrl-C, initiating daemon shutdown");
                }
                _ = sigterm.recv() => {
                    tracing::info!("received SIGTERM, initiating daemon shutdown");
                }
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("received Ctrl-C, initiating daemon shutdown");
        }
        let _ = shutdown_tx_signal.send(true);
    });

    // Spawn a sentinel task for the supervisor to track; agent runs in current task.
    let mut sentinel_rx = shutdown_rx.clone();
    let sentinel = tokio::spawn(async move {
        let _ = sentinel_rx.changed().await;
        Ok(())
    });
    supervisor.add_component(ComponentHandle::new("agent-sentinel", sentinel));

    tokio::select! {
        result = agent.run() => {
            if let Err(e) = result {
                tracing::error!("agent exited with error: {e:#}");
            }
        }
        () = supervisor.run() => {}
    }

    agent.shutdown().await;

    if let Err(e) = remove_pid_file(&pid_file) {
        tracing::warn!("failed to remove PID file: {e}");
    }

    Ok(())
}
