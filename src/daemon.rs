// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

#![cfg(feature = "a2a")]

use std::path::PathBuf;

use parking_lot::RwLock;

use crate::agent_setup;
use crate::bootstrap::{AppBuilder, create_mcp_registry};
#[cfg(feature = "gateway")]
use crate::gateway_spawn::spawn_gateway_server;
use tokio::sync::watch;
use zeph_core::agent::Agent;
use zeph_core::config::Config;
use zeph_llm::LlmProvider as _;

/// Build the [`zeph_a2a::types::AgentCard`] for the daemon's A2A server.
///
/// Derives capability flags from runtime-available signals so the served
/// `/.well-known/agent.json` accurately reflects what the running agent can handle:
///
/// - `images`: `provider.supports_vision()` — only the active LLM can consume image parts.
/// - `audio`: `config.llm.stt_provider_entry().is_some()` — STT presence is the
///   precondition for the agent loop to transcribe audio attachments rather than drop them.
/// - `files`: `config.a2a.advertise_files` — opt-in, because generic file attachments have
///   no built-in ingestion path; set `true` only when skills or MCP tools consume file parts.
fn build_default_card(
    config: &Config,
    public_url: &str,
    provider: &zeph_llm::any::AnyProvider,
) -> zeph_a2a::AgentCard {
    zeph_a2a::AgentCardBuilder::new(&config.agent.name, public_url, env!("CARGO_PKG_VERSION"))
        .description("Zeph AI agent")
        .streaming(true)
        .images(provider.supports_vision())
        .audio(config.llm.stt_provider_entry().is_some())
        .files(config.a2a.advertise_files)
        .build()
}

fn spawn_a2a_server(
    config: &Config,
    shutdown_rx: watch::Receiver<bool>,
    loopback_handle: zeph_core::LoopbackHandle,
    sanitizer: zeph_core::ContentSanitizer,
    // Intentionally not injected into the per-request handler tasks (those are
    // short-lived OneShot spawns managed by the A2A server internally).
    // The overflow cleanup, signal handler, and sentinel tasks in run_daemon
    // are also excluded — they are either fire-and-forget one-shots or
    // lifecycle-managed by DaemonSupervisor.
    supervisor: Option<zeph_core::TaskSupervisor>,
    provider: &zeph_llm::any::AnyProvider,
) {
    let public_url = if config.a2a.public_url.is_empty() {
        format!("http://{}:{}", config.a2a.host, config.a2a.port)
    } else {
        config.a2a.public_url.clone()
    };

    let card = build_default_card(config, &public_url, provider);

    let processor: std::sync::Arc<dyn zeph_a2a::TaskProcessor> =
        std::sync::Arc::new(AgentTaskProcessor {
            loopback_handle: std::sync::Arc::new(tokio::sync::Mutex::new(loopback_handle)),
            sanitizer,
            drain_timeout: std::time::Duration::from_millis(config.a2a.drain_timeout_ms),
        });
    let a2a_server = zeph_a2a::A2aServer::new(
        card,
        processor,
        &config.a2a.host,
        config.a2a.port,
        shutdown_rx,
    )
    .with_auth(config.a2a.auth_token.clone())
    .with_require_auth(config.a2a.require_auth)
    .with_rate_limit(config.a2a.rate_limit)
    .with_max_body_size(config.a2a.max_body_size);

    tracing::info!(
        "A2A server spawned on {}:{}",
        config.a2a.host,
        config.a2a.port
    );

    if let Some(sup) = supervisor {
        // Wrap the one-shot server in Arc<parking_lot::Mutex<Option<_>>> so the Fn factory
        // can hand it off on the first (and only) call. RunOnce tasks are never restarted,
        // so take() will be Some exactly once.
        let cell = std::sync::Arc::new(parking_lot::Mutex::new(Some(a2a_server)));
        sup.spawn(zeph_core::TaskDescriptor {
            name: "a2a_server",
            restart: zeph_core::RestartPolicy::RunOnce,
            factory: move || {
                let server = cell.lock().take();
                async move {
                    if let Some(s) = server {
                        if let Err(e) = s.serve().await {
                            tracing::error!("A2A server error: {e:#}");
                        }
                    } else {
                        tracing::warn!(
                            "a2a_server RunOnce factory called after handoff — \
                             task will not restart; this indicates a policy misconfiguration"
                        );
                    }
                }
            },
        });
    } else {
        tokio::spawn(async move {
            if let Err(e) = a2a_server.serve().await {
                tracing::error!("A2A server error: {e:#}");
            }
        });
    }
}

pub(crate) struct AgentTaskProcessor {
    pub(crate) loopback_handle: std::sync::Arc<tokio::sync::Mutex<zeph_core::LoopbackHandle>>,
    pub(crate) sanitizer: zeph_core::ContentSanitizer,
    pub(crate) drain_timeout: std::time::Duration,
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

        let drain_timeout = self.drain_timeout;

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
            // A timeout guards against an agent loop panic that holds the sender Arc alive
            // without ever emitting Flush, which would otherwise block indefinitely.
            if !exited_on_flush {
                let drain = async {
                    loop {
                        match handle.output_rx.recv().await {
                            Some(zeph_core::LoopbackEvent::Flush) | None => break,
                            Some(_) => {} // discard tail events
                        }
                    }
                };
                if tokio::time::timeout(drain_timeout, drain).await.is_err() {
                    tracing::warn!(
                        timeout_ms = drain_timeout.as_millis(),
                        "A2A drain timeout: Flush not received within deadline; \
                         proceeding with degraded state"
                    );
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
    let embedding_provider = crate::bootstrap::create_embedding_provider(app.config(), &provider);
    let budget_tokens = app.auto_budget_tokens(&provider);

    let registry = std::sync::Arc::new(RwLock::new(app.build_registry()));
    let memory = std::sync::Arc::new(app.build_memory(&provider).await?);
    let all_meta_owned: Vec<zeph_skills::loader::SkillMeta> =
        registry.read().all_meta().into_iter().cloned().collect();
    let all_meta_refs: Vec<&zeph_skills::loader::SkillMeta> = all_meta_owned.iter().collect();
    let matcher = app
        .build_skill_matcher(&embedding_provider, &all_meta_refs, &memory)
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

    let daemon_cancel = tokio_util::sync::CancellationToken::new();
    let task_supervisor = zeph_core::TaskSupervisor::new(daemon_cancel.clone());
    {
        let mut rx = shutdown_rx.clone();
        let cancel = daemon_cancel;
        tokio::spawn(async move {
            let _ = rx.changed().await;
            cancel.cancel();
        });
    }

    let daemon_runtime_ctx = zeph_core::RuntimeContext {
        tui_mode: false,
        daemon_mode: true,
    };

    let filter_registry = if config.tools.filters.enabled {
        zeph_tools::OutputFilterRegistry::default_filters(&config.tools.filters)
    } else {
        zeph_tools::OutputFilterRegistry::new(false)
    };
    let mut shell_executor = zeph_tools::ShellExecutor::new(&config.tools.shell)
        .with_permissions(zeph_tools::build_permission_policy(
            &config.tools,
            config.security.autonomy_level,
        ))
        .with_output_filters(filter_registry);
    if config.tools.sandbox.enabled {
        let denied_present = !config.tools.sandbox.denied_domains.is_empty();
        match zeph_tools::sandbox::build_sandbox_with_policy(
            config.tools.sandbox.strict,
            config.tools.sandbox.fail_if_unavailable,
            denied_present,
        ) {
            Ok(backend) => {
                let name = backend.name();
                let policy = crate::agent_setup::sandbox_policy_from_config(&config.tools.sandbox);
                shell_executor = shell_executor.with_sandbox(std::sync::Arc::from(backend), policy);
                tracing::info!(backend = name, "OS sandbox enabled (daemon)");
            }
            Err(e) if config.tools.sandbox.strict || config.tools.sandbox.fail_if_unavailable => {
                panic!("sandbox initialization failed: {e}");
            }
            Err(e) => {
                tracing::warn!("OS sandbox unavailable, running without isolation: {e}");
            }
        }
    }
    let mut scrape_executor = zeph_tools::WebScrapeExecutor::new(&config.tools.scrape)
        .with_egress_config(config.tools.egress.clone());
    if config.tools.egress.enabled {
        let (egress_tx, egress_rx) = tokio::sync::mpsc::channel(256);
        let dropped = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        scrape_executor = scrape_executor.with_egress_tx(egress_tx, dropped);
        tokio::spawn(agent_setup::drain_egress_events(egress_rx, None));
    }
    let mut daemon_audit_logger: Option<std::sync::Arc<zeph_tools::AuditLogger>> = None;
    if config.tools.audit.enabled
        && let Ok(logger) =
            zeph_tools::AuditLogger::from_config(&config.tools.audit, daemon_runtime_ctx.tui_mode)
                .await
    {
        let logger = std::sync::Arc::new(logger);
        shell_executor = shell_executor.with_audit(std::sync::Arc::clone(&logger));
        scrape_executor = scrape_executor.with_audit(std::sync::Arc::clone(&logger));
        daemon_audit_logger = Some(logger);
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
    let mcp_manager_builder = crate::bootstrap::create_mcp_manager_with_vault(
        config,
        daemon_runtime_ctx.suppress_stderr(),
        app.age_vault_arc(),
    )
    .with_status_tx(status_tx);
    let mcp_manager_builder = crate::bootstrap::wire_trust_calibration(
        mcp_manager_builder,
        config,
        Some(memory.sqlite().pool()),
    )
    .await;
    let mcp_manager = std::sync::Arc::new(mcp_manager_builder);
    let (mcp_tools, _mcp_outcomes) = mcp_manager.connect_all().await;
    // Retain a reference for explicit pre-shutdown so child processes are killed while the
    // tokio runtime is still live (fixes #2693: ChildWithCleanup::drop races with shutdown).
    let shutdown_mcp_manager = std::sync::Arc::clone(&mcp_manager);
    let mcp_shared_tools = std::sync::Arc::new(RwLock::new(mcp_tools.clone()));
    let mcp_executor =
        zeph_mcp::McpToolExecutor::new(mcp_manager.clone(), mcp_shared_tools.clone());
    let shell_policy_handle = shell_executor.policy_handle();
    let cwd_executor = zeph_tools::SetCwdExecutor;
    let base_executor = zeph_tools::CompositeExecutor::new(
        file_executor,
        zeph_tools::CompositeExecutor::new(
            shell_executor,
            zeph_tools::CompositeExecutor::new(scrape_executor, cwd_executor),
        ),
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

    let mcp_embed_provider = {
        let discovery = &config.mcp.tool_discovery;
        if discovery.embedding_provider.is_empty() {
            provider.clone()
        } else {
            match crate::bootstrap::create_named_provider(&discovery.embedding_provider, config) {
                Ok(p) => {
                    tracing::info!(
                        provider = %discovery.embedding_provider,
                        "Using dedicated embed provider for MCP registry"
                    );
                    p
                }
                Err(e) => {
                    tracing::warn!(
                        provider = %discovery.embedding_provider,
                        "MCP registry embed_provider resolution failed, using main provider: {e:#}"
                    );
                    provider.clone()
                }
            }
        }
    };
    let mcp_registry = create_mcp_registry(
        config,
        &mcp_embed_provider,
        &mcp_tools,
        &embed_model,
        app.qdrant_ops(),
    )
    .await;

    let watchers = app.build_watchers();
    let _skill_watcher = watchers.skill_watcher;
    let reload_rx = watchers.skill_reload_rx.into_inner();
    let _config_watcher = watchers.config_watcher;
    let config_reload_rx = watchers.config_reload_rx.into_inner();
    let skill_paths = app.skill_paths_for_registry();
    let plugin_dirs_supplier = app.plugin_dirs_supplier();
    let config_path_owned = app.config_path().to_owned();
    let session_config = zeph_core::AgentSessionConfig::from_config(config, budget_tokens);

    let (loopback_channel, loopback_handle) = zeph_core::LoopbackChannel::pair(64);

    // Pre-resolve RL embed dim before embedding_provider is moved into the agent builder.
    let rl_embed_dim_resolved = if config.skills.rl_routing_enabled {
        Some(
            crate::runner::resolve_rl_embed_dim(
                &config.skills,
                &embedding_provider,
                config.timeouts.embedding_seconds,
            )
            .await,
        )
    } else {
        None
    };

    let agent = Box::pin(
        Agent::new_with_registry_arc(
            provider.clone(),
            embedding_provider.clone(),
            loopback_channel,
            registry,
            matcher,
            config.skills.max_active_skills.get(),
            tool_executor,
        )
        .apply_session_config(session_config)
        .with_skill_matching_config(
            config.skills.disambiguation_threshold,
            config.skills.two_stage_matching,
            config.skills.confusability_threshold,
        )
        .with_skill_reload(skill_paths, reload_rx)
        .with_plugin_dirs_supplier(plugin_dirs_supplier)
        .with_managed_skills_dir(crate::bootstrap::managed_skills_dir())
        .with_memory(
            std::sync::Arc::clone(&memory),
            conversation_id,
            config.memory.history_limit,
            config.memory.semantic.recall_limit,
            config.memory.summarization_threshold,
        )
        .with_shutdown(shutdown_rx.clone())
        .with_config_reload(config_path_owned, config_reload_rx)
        .with_plugins_dir(crate::bootstrap::plugins_dir(), {
            let mut blocked = config.tools.shell.blocked_commands.clone();
            blocked.sort();
            let mut allowed = config.tools.shell.allowed_commands.clone();
            allowed.sort();
            zeph_core::ShellOverlaySnapshot { blocked, allowed }
        })
        .with_shell_policy_handle(shell_policy_handle)
        .with_mcp(mcp_tools, mcp_registry, Some(mcp_manager), &config.mcp)
        .with_mcp_shared_tools(mcp_shared_tools)
        .with_hybrid_search(config.skills.hybrid_search)
        .with_rl_routing(
            config.skills.rl_routing_enabled,
            config.skills.rl_learning_rate,
            config.skills.rl_weight,
            config.skills.rl_persist_interval,
            config.skills.rl_warmup_updates,
        )
        .with_focus_and_sidequest_config(
            config.agent.focus.clone(),
            config.memory.sidequest.clone(),
        )
        .with_trajectory_and_category_config(
            config.memory.trajectory.clone(),
            config.memory.category.clone(),
        )
        .with_embedding_provider(embedding_provider.clone())
        .maybe_init_tool_schema_filter(config.agent.tool_filter.clone(), embedding_provider),
    )
    .await;

    let agent = if let Some(logger) = daemon_audit_logger {
        agent.with_audit_logger(logger)
    } else {
        agent
    };

    // SkillOrchestra: load persisted RL routing head weights if enabled.
    let agent = if let Some(dim) = rl_embed_dim_resolved {
        let head = crate::runner::load_rl_head(&memory)
            .await
            .unwrap_or_else(|| {
                // Cold start: no persisted weights yet, initialize a fresh head.
                tracing::info!(dim, "rl_head: cold start, initializing fresh routing head");
                zeph_skills::rl_head::RoutingHead::new(dim)
            });
        agent.with_rl_head(head)
    } else {
        agent
    };

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
    let verify_provider = app.build_verify_provider();
    let agent = if let Some(vp) = verify_provider {
        agent.with_verify_provider(vp)
    } else {
        agent
    };
    let agent = agent_setup::apply_quarantine_provider(agent, app.build_quarantine_provider());
    let agent = agent_setup::apply_guardrail(agent, app.build_guardrail_provider());
    #[cfg(feature = "classifiers")]
    let agent = agent_setup::apply_injection_classifier(agent, config);
    #[cfg(feature = "classifiers")]
    let agent = agent_setup::apply_enforcement_mode(agent, config);
    #[cfg(feature = "classifiers")]
    let agent = agent_setup::apply_three_class_classifier(agent, config);
    #[cfg(feature = "classifiers")]
    let agent = agent_setup::apply_pii_classifier(agent, config);
    let agent = agent_setup::apply_causal_analyzer(agent, provider.clone(), config);
    let agent = agent_setup::apply_vigil(agent, &config.security.vigil);

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
        .with_hooks_config(&config.hooks);

    agent.load_history().await?;
    agent
        .check_vector_store_health(config.memory.vector_backend.as_str())
        .await;

    let a2a_sanitizer = zeph_core::ContentSanitizer::new(&config.security.content_isolation);
    spawn_a2a_server(
        config,
        shutdown_rx.clone(),
        loopback_handle,
        a2a_sanitizer,
        Some(task_supervisor),
        &provider,
    );

    #[cfg(feature = "gateway")]
    if config.gateway.enabled {
        spawn_gateway_server(
            config,
            shutdown_rx.clone(),
            // Daemon mode has no MetricsSnapshot watch channel — skip Prometheus sync.
            #[cfg(feature = "prometheus")]
            None,
        );
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

    // Explicitly shut down MCP connections before agent.shutdown() so that child processes
    // are killed while the tokio runtime is still active (#2693).
    shutdown_mcp_manager.shutdown_all_shared().await;
    agent.shutdown().await;

    if let Err(e) = remove_pid_file(&pid_file) {
        tracing::warn!("failed to remove PID file: {e}");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeph_config::channels::A2aServerConfig;
    use zeph_config::providers::{ProviderEntry, ProviderKind, SttConfig};
    use zeph_llm::any::AnyProvider;
    use zeph_llm::mock::MockProvider;

    fn mock_provider() -> AnyProvider {
        AnyProvider::Mock(MockProvider::default())
    }

    fn config_with_a2a(advertise_files: bool) -> Config {
        Config {
            a2a: A2aServerConfig {
                advertise_files,
                ..A2aServerConfig::default()
            },
            ..Config::default()
        }
    }

    /// Build a config that has an STT provider entry wired up, so `stt_provider_entry()` returns `Some`.
    fn config_with_stt(advertise_files: bool) -> Config {
        let mut cfg = config_with_a2a(advertise_files);
        cfg.llm.providers = vec![ProviderEntry {
            name: Some("stt-provider".into()),
            provider_type: ProviderKind::Ollama,
            stt_model: Some("whisper".into()),
            ..ProviderEntry::default()
        }];
        cfg.llm.stt = Some(SttConfig {
            provider: "stt-provider".into(),
            language: "en".into(),
        });
        cfg
    }

    #[test]
    fn build_default_card_no_capabilities_by_default() {
        let cfg = config_with_a2a(false);
        let provider = mock_provider();
        // MockProvider::supports_vision() returns false; no STT; advertise_files=false
        let card = build_default_card(&cfg, "http://localhost:8080", &provider);
        assert!(
            !card.capabilities.images,
            "images must be false without vision support"
        );
        assert!(!card.capabilities.audio, "audio must be false without STT");
        assert!(
            !card.capabilities.files,
            "files must be false when advertise_files=false"
        );
        assert!(card.capabilities.streaming, "streaming must always be true");
    }

    #[test]
    fn build_default_card_audio_from_stt_config() {
        let cfg = config_with_stt(false);
        let provider = mock_provider();
        let card = build_default_card(&cfg, "http://localhost:8080", &provider);
        assert!(
            card.capabilities.audio,
            "audio must be true when STT provider is configured"
        );
        assert!(!card.capabilities.images);
        assert!(!card.capabilities.files);
    }

    #[test]
    fn build_default_card_files_from_advertise_files_flag() {
        let cfg = config_with_a2a(true);
        let provider = mock_provider();
        let card = build_default_card(&cfg, "http://localhost:8080", &provider);
        assert!(
            card.capabilities.files,
            "files must be true when advertise_files=true"
        );
        assert!(!card.capabilities.images);
        assert!(!card.capabilities.audio);
    }

    #[test]
    fn build_default_card_audio_and_files_without_images() {
        let cfg = config_with_stt(true);
        let provider = mock_provider();
        let card = build_default_card(&cfg, "http://localhost:8080", &provider);
        // images is still false because MockProvider::supports_vision() returns false
        assert!(!card.capabilities.images);
        assert!(card.capabilities.audio);
        assert!(card.capabilities.files);
        assert!(card.capabilities.streaming);
    }
}
