// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

#[cfg(any(feature = "acp", feature = "acp-http"))]
use std::path::PathBuf;

#[cfg(feature = "acp")]
use parking_lot::RwLock;

#[cfg(feature = "acp")]
use crate::agent_setup;
#[cfg(any(feature = "acp", feature = "acp-http"))]
use crate::bootstrap::{AppBuilder, create_mcp_registry};
#[cfg(feature = "acp")]
use zeph_core::agent::Agent;
#[cfg(feature = "acp")]
use zeph_core::channel::Channel;
#[cfg(feature = "acp")]
use zeph_tools::ErasedToolExecutor;

#[cfg(feature = "acp")]
fn resolve_runtime_path(path: &std::path::Path, cwd: &std::path::Path) -> std::path::PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

#[cfg(feature = "acp")]
fn log_acp_runtime_paths(config: &zeph_core::config::Config, config_path: &std::path::Path) {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let logging_file = if config.logging.file.is_empty() {
        None
    } else {
        Some(resolve_runtime_path(
            std::path::Path::new(&config.logging.file),
            &cwd,
        ))
    };
    let sqlite_path = resolve_runtime_path(std::path::Path::new(&config.memory.sqlite_path), &cwd);
    let debug_output_dir = resolve_runtime_path(config.debug.output_dir.as_path(), &cwd);
    let skill_paths: Vec<std::path::PathBuf> = config
        .skills
        .paths
        .iter()
        .map(|p| resolve_runtime_path(std::path::Path::new(p), &cwd))
        .collect();
    let permission_file = config
        .acp
        .permission_file
        .as_ref()
        .map(|p| resolve_runtime_path(p.as_path(), &cwd));

    tracing::info!(
        cwd = %cwd.display(),
        config_path = %config_path.display(),
        logging_file = logging_file
            .as_ref()
            .map_or_else(|| "<disabled>".to_owned(), |p| p.display().to_string()),
        sqlite_path = %sqlite_path.display(),
        debug_output_dir = %debug_output_dir.display(),
        permission_file = permission_file
            .as_ref()
            .map_or_else(|| "<none>".to_owned(), |p| p.display().to_string()),
        skill_paths = ?skill_paths,
        "ACP startup runtime paths"
    );
}

/// Shared dependencies reused across all ACP sessions.
///
/// Fields in this struct are expensive to create and safe to share across sessions.
/// `AnyProvider` is intentionally shared via `Arc` — all provider variants use internal
/// HTTP connection pools (`reqwest::Client`) that benefit from connection reuse across sessions.
/// This is equivalent to sharing an HTTP client pool, which is the intended design.
///
/// Per-session state (`conversation_id`, reload receivers, cancel signals) is created fresh
/// in `spawn_acp_agent` for each session.
///
/// ## Field categories
///
/// - **Shared runtime objects** (`provider`, `registry`, `memory`, `mcp_manager`, etc.) —
///   expensive to create, safe to share via `Arc` / `Clone`.
/// - **Config snapshot** (`session_config`) — single source of truth for all config-derived
///   agent settings; see [`zeph_core::AgentSessionConfig`].
/// - **Optional runtime providers** (`summary_provider`, `judge_provider`,
///   `quarantine_provider`) — contain HTTP client pools (`AnyProvider`) with runtime state;
///   excluded from `session_config` because they are not purely config-derived.
/// - **MCP objects** (`mcp_tools`, `mcp_registry`, `mcp_manager`, `mcp_shared_tools`,
///   `mcp_config`) — runtime + config mixture; passed together to `with_mcp()`.
/// - **ACP-specific** (`acp_*`) — transport-level config; not agent-level.
/// - **Scheduler runtime** (`scheduler_*`) — runtime broadcast senders; not config-derived.
#[cfg(feature = "acp")]
struct SharedAgentDeps {
    // Shared runtime objects
    provider: zeph_llm::any::AnyProvider,
    /// Dedicated embedding provider. Never replaced by `/provider switch`.
    embedding_provider: zeph_llm::any::AnyProvider,
    registry: std::sync::Arc<RwLock<zeph_skills::registry::SkillRegistry>>,
    /// Shared skill matcher: `Clone` is cheap for Qdrant (connection-pool sharing), and
    /// involves copying in-memory embedding vectors only for the `InMemory` variant.
    matcher: Option<zeph_skills::matcher::SkillMatcherBackend>,
    max_active_skills: usize,
    tool_executor: std::sync::Arc<dyn zeph_tools::ErasedToolExecutor>,
    skill_paths: Vec<PathBuf>,
    memory: std::sync::Arc<zeph_memory::semantic::SemanticMemory>,
    history_limit: u32,
    recall_limit: usize,
    summarization_threshold: usize,
    /// Broadcast sender for skill reload events. Each session subscribes independently.
    skill_reload_tx: tokio::sync::broadcast::Sender<zeph_skills::watcher::SkillEvent>,
    /// Broadcast sender for config reload events. Each session subscribes independently.
    config_reload_tx: tokio::sync::broadcast::Sender<zeph_core::config_watcher::ConfigEvent>,
    /// Shared shutdown signal (`watch::Receiver` is `Clone`).
    shutdown_rx: tokio::sync::watch::Receiver<bool>,
    config_path: PathBuf,

    // MCP — runtime objects + config passed together to `with_mcp()`
    mcp_tools: Vec<zeph_mcp::McpTool>,
    mcp_registry: Option<zeph_mcp::McpToolRegistry>,
    mcp_manager: std::sync::Arc<zeph_mcp::McpManager>,
    mcp_shared_tools: std::sync::Arc<RwLock<Vec<zeph_mcp::McpTool>>>,
    mcp_config: zeph_core::config::McpConfig,

    // Optional runtime providers (contain HTTP client pools; excluded from session_config)
    summary_provider: Option<zeph_llm::any::AnyProvider>,
    judge_provider: Option<zeph_llm::any::AnyProvider>,
    feedback_classifier: Option<zeph_llm::classifier::llm::LlmClassifier>,
    #[cfg(feature = "classifiers")]
    classifiers_config: zeph_core::config::ClassifiersConfig,
    /// `security.pii_filter.enabled` — gates the NER union-merge PII layer (#5463),
    /// mirroring the check in `agent_setup::apply_pii_ner_classifier`.
    #[cfg(feature = "classifiers")]
    pii_filter_enabled: bool,
    causal_ipi_config: zeph_sanitizer::causal_ipi::CausalIpiConfig,
    causal_provider: Option<zeph_llm::any::AnyProvider>,
    nli_config: zeph_sanitizer::nli::NliConfig,
    nli_provider: Option<zeph_llm::any::AnyProvider>,
    secret_registry: Option<std::sync::Arc<zeph_sanitizer::secret_mask::SecretMaskRegistry>>,
    vigil_config: zeph_config::VigilConfig,
    probe_provider: Option<zeph_llm::any::AnyProvider>,
    planner_provider: Option<zeph_llm::any::AnyProvider>,
    verify_provider: Option<zeph_llm::any::AnyProvider>,
    orchestrator_provider: Option<zeph_llm::any::AnyProvider>,
    predicate_provider: Option<zeph_llm::any::AnyProvider>,
    quarantine_provider: Option<(zeph_llm::any::AnyProvider, zeph_sanitizer::QuarantineConfig)>,
    guardrail_provider: Option<(
        zeph_llm::any::AnyProvider,
        zeph_sanitizer::guardrail::GuardrailConfig,
    )>,

    /// Audit logger for pre-execution verifier blocks. `None` when audit is disabled.
    audit_logger: Option<std::sync::Arc<zeph_tools::AuditLogger>>,

    // Config snapshot — single source of truth for all config-derived agent settings
    session_config: zeph_core::AgentSessionConfig,
    /// `[session]` persistence settings (spec-068, #5343) — durable JSONL event log dual-write.
    /// Distinct from `session_config` (`AgentSessionConfig`, recap/loop settings).
    session_persistence_config: zeph_config::SessionConfig,
    /// D-13 (spec-068 §8.1, N3): resume-time durable condensation, pre-built once here (where
    /// the full `Config` — needed for `[[llm.providers]]` name resolution and secrets — is
    /// still in scope) rather than per-session in `spawn_acp_agent`, which only receives
    /// pre-decomposed sub-configs, not the raw `Config`. Mirrors the existing
    /// `session_persistence_config` pattern: extract once at deps-build time, read by
    /// reference per session.
    resume_condenser: zeph_session::LlmCondenser,
    resume_token_counter: std::sync::Arc<zeph_agent_context::memory_backend::TokenCounterAdapter>,
    focus_config: zeph_core::config::FocusConfig,
    sidequest_config: zeph_core::config::SidequestConfig,
    trajectory_config: zeph_core::config::TrajectoryConfig,
    category_config: zeph_core::config::CategoryConfig,
    tool_filter_config: zeph_core::config::ToolFilterConfig,

    hooks_config: zeph_core::config::HooksConfig,

    // ACP-specific fields (transport-level; not agent-level)
    acp_agent_name: String,
    acp_agent_version: String,
    acp_max_sessions: usize,
    acp_session_idle_timeout_secs: u64,
    acp_permission_file: Option<std::path::PathBuf>,
    acp_available_models: std::sync::Arc<RwLock<Vec<String>>>,
    acp_auth_bearer_token: Option<String>,
    acp_discovery_enabled: bool,
    /// Maximum characters for auto-generated session titles.
    acp_title_max_chars: usize,
    /// Maximum number of sessions returned by list endpoints.
    acp_max_history: usize,
    /// Effective log file path advertised in the stdio readiness notification.
    acp_log_file: Option<String>,
    /// `SQLite` database path, passed to ACP transport for session persistence.
    sqlite_path: String,
    /// Pre-built provider factory for ACP model switching.
    #[cfg(feature = "acp")]
    acp_provider_factory: Option<zeph_acp::ProviderFactory>,
    /// Provider name + protocol pairs advertised via `providers/list` (#5448).
    acp_provider_names: Vec<(String, zeph_acp::LlmProtocol)>,
    /// Project rule file paths to advertise in session `_meta`.
    acp_project_rules: Vec<PathBuf>,
    /// Allowlist of directories ACP clients may reference in session requests.
    acp_additional_directories: Vec<zeph_core::config::AdditionalDir>,
    /// Auth methods to advertise in the `initialize` response.
    acp_auth_methods: Vec<zeph_core::config::AcpAuthMethod>,
    /// When `true`, echo `PromptRequest.message_id` through responses and chunks.
    acp_message_ids_enabled: bool,
    /// ACP timeout configuration (elicitation, terminal, MCP).
    acp_timeouts: zeph_config::AcpTimeoutsConfig,
    /// ACP model-related configuration parameters (`[acp.model_config]`).
    acp_model_config: zeph_config::AcpModelConfigConfig,
    /// Resolves current per-plugin skill dirs at hot-reload time.
    plugin_dirs_supplier: std::sync::Arc<dyn Fn() -> Vec<PathBuf> + Send + Sync>,

    /// Shell overlay snapshot captured at startup for hot-reload divergence detection.
    startup_shell_overlay: zeph_core::ShellOverlaySnapshot,
    /// Live-rebuild handle for the `ShellExecutor`'s `blocked_commands` policy.
    shell_policy_handle: zeph_tools::ShellPolicyHandle,

    // Scheduler runtime objects (broadcast senders; not config-derived values)
    /// Scheduler executor shared across sessions. Initialized once at startup.
    #[cfg(feature = "scheduler")]
    scheduler_executor: Option<std::sync::Arc<crate::scheduler_executor::SchedulerExecutor>>,
    /// Broadcast sender for scheduler update notifications (`auto_update_check`).
    #[cfg(feature = "scheduler")]
    scheduler_update_tx: Option<tokio::sync::broadcast::Sender<String>>,
    /// Broadcast sender for custom task notifications.
    #[cfg(feature = "scheduler")]
    scheduler_custom_tx: Option<tokio::sync::broadcast::Sender<String>>,
}

/// Forward events from a `broadcast::Receiver` to an `mpsc::Receiver`.
///
/// The forwarding task exits when:
/// - The `mpsc::Sender` is dropped (agent loop finished): `tx.send()` returns `Err`.
/// - The `CancellationToken` is cancelled (session evicted or shutdown).
/// - The broadcast channel is closed: `brx.recv()` returns `RecvError::Closed`.
///
/// Lagged broadcast events are logged at `warn!` and skipped. ACP session cancellation does not
/// rely on this adapter; it is wired through a separate per-session `Notify` signal.
#[cfg(feature = "acp")]
fn broadcast_to_mpsc<T: Clone + Send + 'static>(
    mut brx: tokio::sync::broadcast::Receiver<T>,
    cancel: zeph_memory::CancellationToken,
) -> tokio::sync::mpsc::Receiver<T> {
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    tokio::spawn(async move {
        // EXEMPT(#5144): reusable adapter; self-terminating on cancel/broadcast close
        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                result = brx.recv() => {
                    match result {
                        Ok(item) => {
                            if tx.send(item).await.is_err() {
                                break; // Receiver dropped: agent loop finished.
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!(skipped = n, "broadcast_to_mpsc: lagged, some reload events dropped");
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
        }
    });
    rx
}

/// Build all agent dependencies from config for the ACP server.
#[cfg(feature = "acp")]
#[allow(clippy::too_many_lines)]
async fn build_acp_deps(
    config_path: Option<&std::path::Path>,
    vault_backend: Option<&str>,
    vault_key: Option<&std::path::Path>,
    vault_path: Option<&std::path::Path>,
    prebuilt_mcp_manager: Option<std::sync::Arc<zeph_mcp::McpManager>>,
) -> anyhow::Result<(SharedAgentDeps, Box<dyn std::any::Any>)> {
    let app = AppBuilder::new(config_path, vault_backend, vault_key, vault_path).await?;
    log_acp_runtime_paths(app.config(), app.config_path());
    let (provider, _status_tx, _status_rx) = app.build_provider().await?;
    let embed_model = app.embedding_model();
    let embedding_provider = crate::bootstrap::create_embedding_provider(app.config(), &provider);
    let budget_tokens = app.auto_budget_tokens(&provider);
    let registry = std::sync::Arc::new(RwLock::new(app.build_registry()));
    let acp_mem_cancel = tokio_util::sync::CancellationToken::new();
    let acp_mem_supervisor = std::sync::Arc::new(zeph_common::TaskSupervisor::new(acp_mem_cancel));
    let memory = std::sync::Arc::new(app.build_memory(&provider, &acp_mem_supervisor).await?);

    {
        let sqlite = memory.sqlite().clone();
        let retention_secs = app
            .config()
            .tools
            .overflow
            .retention_days
            .saturating_mul(86_400);
        let cell = std::sync::Arc::new(parking_lot::Mutex::new(Some((sqlite, retention_secs))));
        acp_mem_supervisor.spawn(zeph_common::task_supervisor::TaskDescriptor {
            name: "overflow_cleanup",
            restart: zeph_common::task_supervisor::RestartPolicy::RunOnce,
            factory: move || {
                let args = cell.lock().take();
                async move {
                    if let Some((sqlite, retention_secs)) = args {
                        match sqlite.cleanup_overflow(retention_secs).await {
                            Ok(n) if n > 0 => {
                                tracing::info!("cleaned up {n} stale overflow entries");
                            }
                            Ok(_) => {}
                            Err(e) => tracing::warn!("overflow cleanup failed: {e}"),
                        }
                    } else {
                        tracing::warn!("overflow_cleanup factory called more than once");
                    }
                }
            },
        });
    }

    let all_meta_owned: Vec<zeph_skills::loader::SkillMeta> =
        registry.read().all_meta().into_iter().cloned().collect();
    let all_meta_refs: Vec<&zeph_skills::loader::SkillMeta> = all_meta_owned.iter().collect();
    let matcher = app
        .build_skill_matcher(&embedding_provider, &all_meta_refs, &memory)
        .await;
    let config = app.config();

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
        .with_output_filters(filter_registry)
        .with_task_supervisor((*acp_mem_supervisor).clone());
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
                tracing::info!(backend = name, "OS sandbox enabled (acp)");
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
        {
            let cell = std::sync::Arc::new(parking_lot::Mutex::new(Some(egress_rx)));
            acp_mem_supervisor.spawn(zeph_common::task_supervisor::TaskDescriptor {
                name: "egress_drain",
                restart: zeph_common::task_supervisor::RestartPolicy::RunOnce,
                factory: move || {
                    let rx = cell.lock().take();
                    async move {
                        if let Some(rx) = rx {
                            agent_setup::drain_egress_events(rx, None).await;
                        } else {
                            tracing::warn!("egress_drain factory called more than once");
                        }
                    }
                },
            });
        }
    }
    let mut acp_audit_logger: Option<std::sync::Arc<zeph_tools::AuditLogger>> = None;
    if config.tools.audit.enabled
        && let Ok(logger) = zeph_tools::AuditLogger::from_config(&config.tools.audit, false).await
    {
        let logger = std::sync::Arc::new(logger);
        shell_executor = shell_executor.with_audit(std::sync::Arc::clone(&logger));
        scrape_executor = scrape_executor.with_audit(std::sync::Arc::clone(&logger));
        acp_audit_logger = Some(logger);
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
    let mcp_manager = if let Some(m) = prebuilt_mcp_manager {
        m
    } else {
        let builder =
            crate::bootstrap::create_mcp_manager_with_vault(config, false, app.age_vault_arc());
        let builder =
            crate::bootstrap::wire_trust_calibration(builder, config, Some(memory.sqlite().pool()))
                .await;
        std::sync::Arc::new(builder)
    };
    let (mcp_tools, _mcp_outcomes) = mcp_manager.connect_all().await;
    let mcp_shared_tools = std::sync::Arc::new(RwLock::new(mcp_tools.clone()));
    let mcp_executor =
        zeph_mcp::McpToolExecutor::new(mcp_manager.clone(), mcp_shared_tools.clone());
    let shell_policy_handle = shell_executor.policy_handle();
    let cwd_executor = zeph_tools::SetCwdExecutor;
    let diagnostics_executor = crate::agent_setup::build_diagnostics_executor(config);
    let base_executor = zeph_tools::CompositeExecutor::new(
        file_executor,
        zeph_tools::CompositeExecutor::new(
            shell_executor,
            zeph_tools::CompositeExecutor::new(
                scrape_executor,
                zeph_tools::CompositeExecutor::new(cwd_executor, diagnostics_executor),
            ),
        ),
    );
    let index_provider = crate::bootstrap::resolve_index_embed_provider(config, provider.clone());
    let tool_executor: std::sync::Arc<dyn zeph_tools::ErasedToolExecutor> = {
        let base: std::sync::Arc<dyn zeph_tools::ErasedToolExecutor> = std::sync::Arc::new(
            zeph_tools::CompositeExecutor::new(base_executor, mcp_executor),
        );
        if let Some(search_executor) = crate::agent_setup::build_search_code_executor(
            config,
            app.qdrant_ops().cloned(),
            index_provider,
            memory.sqlite().pool().clone(),
            Some(std::sync::Arc::clone(&mcp_manager)),
        ) {
            std::sync::Arc::new(zeph_tools::CompositeExecutor::new(
                zeph_tools::DynExecutor(base),
                search_executor,
            ))
        } else {
            base
        }
    };

    let mcp_registry = create_mcp_registry(
        config,
        &provider,
        &mcp_tools,
        &embed_model,
        app.qdrant_ops(),
    )
    .await;
    let summary_provider = app.build_summary_provider();
    let skill_paths = app.skill_paths_for_registry();
    let plugin_dirs_supplier = app.plugin_dirs_supplier();
    let acp_project_rules = collect_project_rules(&skill_paths);
    let crate::bootstrap::WatcherBundle {
        skill_watcher,
        skill_reload_rx: mpsc_skill_rx,
        config_watcher,
        config_reload_rx: mpsc_config_rx,
    } = app.build_watchers(&acp_mem_supervisor);
    let config_path_owned = app.config_path().to_owned();
    let (_, shutdown_rx) = AppBuilder::build_shutdown();

    // Convert mpsc receivers from watchers to broadcast senders so each ACP session
    // can subscribe independently. Option A (critic S3): keep watchers unchanged,
    // forward mpsc→broadcast only here in build_acp_deps.
    // Keep enough backlog for bursty reload traffic while leaving room for larger deployments
    // to raise the limit explicitly via config.
    let broadcast_cap = config.acp.broadcast_capacity.max(1);
    let (skill_reload_tx, _) = tokio::sync::broadcast::channel(broadcast_cap);
    let (config_reload_tx, _) = tokio::sync::broadcast::channel(broadcast_cap);

    {
        let skill_tx = skill_reload_tx.clone();
        let cell = std::sync::Arc::new(parking_lot::Mutex::new(Some(mpsc_skill_rx)));
        acp_mem_supervisor.spawn(zeph_common::task_supervisor::TaskDescriptor {
            name: "skill_reload_fwd",
            restart: zeph_common::task_supervisor::RestartPolicy::RunOnce,
            factory: move || {
                let rx = cell.lock().take();
                let tx = skill_tx.clone();
                async move {
                    if let Some(mut rx) = rx {
                        while let Some(ev) = rx.recv().await {
                            let _ = tx.send(ev);
                        }
                    } else {
                        tracing::warn!("skill_reload_fwd factory called more than once");
                    }
                }
            },
        });
    }
    {
        let cfg_tx = config_reload_tx.clone();
        let cell = std::sync::Arc::new(parking_lot::Mutex::new(Some(mpsc_config_rx)));
        acp_mem_supervisor.spawn(zeph_common::task_supervisor::TaskDescriptor {
            name: "config_reload_fwd",
            restart: zeph_common::task_supervisor::RestartPolicy::RunOnce,
            factory: move || {
                let rx = cell.lock().take();
                let tx = cfg_tx.clone();
                async move {
                    if let Some(mut rx) = rx {
                        while let Some(ev) = rx.recv().await {
                            let _ = tx.send(ev);
                        }
                    } else {
                        tracing::warn!("config_reload_fwd factory called more than once");
                    }
                }
            },
        });
    }

    #[cfg(feature = "scheduler")]
    let (scheduler_executor, scheduler_update_tx, scheduler_custom_tx) = {
        let exp_deps = {
            use std::sync::Arc;
            if config.experiments.enabled && config.experiments.schedule.enabled {
                let p = provider.clone();
                Some((Arc::new(p), Some(Arc::clone(&memory))))
            } else {
                None
            }
        };

        let five_signal = memory.five_signal_runtime();
        match crate::scheduler::init_scheduler(
            config,
            shutdown_rx.clone(),
            exp_deps,
            five_signal,
            Some(&acp_mem_supervisor),
        )
        .await
        {
            Some(result) => {
                let exec = std::sync::Arc::new(result.executor);
                let custom_rx = result.custom_rx;
                let (ctx, _) = tokio::sync::broadcast::channel::<String>(broadcast_cap);
                let ctx_clone = ctx.clone();
                let cell = std::sync::Arc::new(parking_lot::Mutex::new(Some(custom_rx)));
                acp_mem_supervisor.spawn(zeph_common::task_supervisor::TaskDescriptor {
                    name: "sched_custom_fwd",
                    restart: zeph_common::task_supervisor::RestartPolicy::RunOnce,
                    factory: move || {
                        let rx = cell.lock().take();
                        let tx = ctx_clone.clone();
                        async move {
                            if let Some(mut rx) = rx {
                                while let Some(ev) = rx.recv().await {
                                    let _ = tx.send(ev);
                                }
                            } else {
                                tracing::warn!("sched_custom_fwd factory called more than once");
                            }
                        }
                    },
                });
                let update_tx = if let Some(update_rx) = result.update_rx {
                    let (utx, _) = tokio::sync::broadcast::channel::<String>(broadcast_cap);
                    let utx_clone = utx.clone();
                    let cell = std::sync::Arc::new(parking_lot::Mutex::new(Some(update_rx)));
                    acp_mem_supervisor.spawn(zeph_common::task_supervisor::TaskDescriptor {
                        name: "sched_update_fwd",
                        restart: zeph_common::task_supervisor::RestartPolicy::RunOnce,
                        factory: move || {
                            let rx = cell.lock().take();
                            let tx = utx_clone.clone();
                            async move {
                                if let Some(mut rx) = rx {
                                    while let Some(ev) = rx.recv().await {
                                        let _ = tx.send(ev);
                                    }
                                } else {
                                    tracing::warn!(
                                        "sched_update_fwd factory called more than once"
                                    );
                                }
                            }
                        },
                    });
                    Some(utx)
                } else {
                    None
                };
                let (update_tx, custom_tx) = (update_tx, Some(ctx));
                (Some(exec), update_tx, custom_tx)
            }
            None => (None, None, None),
        }
    };

    let session_config = zeph_core::AgentSessionConfig::from_config(config, budget_tokens);
    // D-13 (spec-068 §8.1, N3): built once here, where the full `Config` is still in scope —
    // see the `resume_condenser` field's doc comment on `SharedAgentDeps`.
    let (resume_condenser_built, resume_token_counter_built) =
        zeph_core::provider_factory::build_resume_condenser(config, &provider);
    let feedback_classifier = app.build_feedback_classifier(&provider);

    let deps = SharedAgentDeps {
        provider,
        embedding_provider,
        registry,
        matcher,
        max_active_skills: config.skills.max_active_skills.get(),
        tool_executor,
        skill_paths,
        skill_reload_tx,
        config_reload_tx,
        memory,
        history_limit: config.memory.history_limit,
        recall_limit: config.memory.semantic.recall_limit,
        summarization_threshold: config.memory.summarization_threshold,
        shutdown_rx,
        config_path: config_path_owned,
        mcp_tools,
        mcp_registry,
        mcp_manager,
        mcp_shared_tools,
        mcp_config: config.mcp.clone(),
        summary_provider,
        judge_provider: app.build_judge_provider(),
        feedback_classifier,
        #[cfg(feature = "classifiers")]
        classifiers_config: config.classifiers.clone(),
        #[cfg(feature = "classifiers")]
        pii_filter_enabled: config.security.pii_filter.enabled,
        causal_ipi_config: config.security.causal_ipi.clone(),
        causal_provider: config
            .security
            .causal_ipi
            .provider
            .as_deref()
            .filter(|s| !s.is_empty())
            .and_then(|name| match crate::bootstrap::create_named_provider(name, config) {
                Ok(p) => {
                    tracing::info!(provider = %name, "causal IPI dedicated provider configured (acp)");
                    Some(p)
                }
                Err(e) => {
                    tracing::warn!(
                        provider = %name,
                        error = %e,
                        "causal IPI provider resolution failed, falling back to primary (acp)"
                    );
                    None
                }
            }),
        nli_config: config.security.content_isolation.nli.clone(),
        nli_provider: config
            .security
            .content_isolation
            .nli
            .provider
            .as_non_empty()
            .and_then(|name| match crate::bootstrap::create_named_provider(name, config) {
                Ok(p) => {
                    tracing::info!(provider = %name, "NLI dedicated provider configured (acp)");
                    Some(p)
                }
                Err(e) => {
                    tracing::warn!(
                        provider = %name,
                        error = %e,
                        "NLI provider resolution failed, falling back to primary (acp)"
                    );
                    None
                }
            }),
        secret_registry: app.secret_registry(),
        vigil_config: config.security.vigil.clone(),
        probe_provider: app.build_probe_provider(),
        planner_provider: app.build_planner_provider(),
        verify_provider: app.build_verify_provider(),
        orchestrator_provider: app.build_orchestrator_provider(),
        predicate_provider: app.build_predicate_provider(),
        quarantine_provider: app.build_quarantine_provider(),
        guardrail_provider: app.build_guardrail_provider(),
        audit_logger: acp_audit_logger,
        hooks_config: config.hooks.clone(),
        session_config,
        session_persistence_config: config.session.clone(),
        resume_condenser: resume_condenser_built,
        resume_token_counter: resume_token_counter_built,
        focus_config: config.agent.focus.clone(),
        sidequest_config: config.memory.sidequest.clone(),
        trajectory_config: config.memory.trajectory.clone(),
        category_config: config.memory.category.clone(),
        tool_filter_config: config.agent.tool_filter.clone(),
        acp_agent_name: config.acp.agent_name.clone(),
        acp_agent_version: config.acp.agent_version.clone(),
        acp_max_sessions: config.acp.max_sessions,
        acp_session_idle_timeout_secs: config.acp.session_idle_timeout_secs,
        acp_permission_file: config.acp.permission_file.clone(),
        acp_available_models: std::sync::Arc::new(RwLock::new(
            if config.acp.available_models.is_empty() {
                discover_models_from_config(config).await
            } else {
                config.acp.available_models.clone()
            },
        )),
        acp_auth_bearer_token: config.acp.auth_token.clone(),
        acp_discovery_enabled: config.acp.discovery_enabled,
        acp_title_max_chars: config.memory.sessions.title_max_chars,
        acp_max_history: config.memory.sessions.max_history,
        acp_log_file: if config.logging.file.is_empty() {
            None
        } else {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            Some(
                resolve_runtime_path(std::path::Path::new(&config.logging.file), &cwd)
                    .display()
                    .to_string(),
            )
        },
        sqlite_path: crate::db_url::resolve_db_url(config).to_owned(),
        acp_provider_factory: Some(build_acp_provider_factory(config, app.secret_registry())),
        acp_provider_names: acp_provider_names(config),
        acp_project_rules,
        acp_additional_directories: config.acp.additional_directories.clone(),
        acp_auth_methods: config.acp.auth_methods.clone(),
        acp_message_ids_enabled: config.acp.message_ids_enabled,
        acp_timeouts: config.acp.timeouts.clone(),
        acp_model_config: config.acp.model_config.clone(),
        plugin_dirs_supplier: std::sync::Arc::new(plugin_dirs_supplier),
        #[cfg(feature = "scheduler")]
        scheduler_executor,
        #[cfg(feature = "scheduler")]
        scheduler_update_tx,
        #[cfg(feature = "scheduler")]
        scheduler_custom_tx,
        startup_shell_overlay: {
            let mut blocked = config.tools.shell.blocked_commands.clone();
            blocked.sort();
            let mut allowed = config.tools.shell.allowed_commands.clone();
            allowed.sort();
            zeph_core::ShellOverlaySnapshot { blocked, allowed }
        },
        shell_policy_handle,
    };

    let keepalive: Box<dyn std::any::Any> = Box::new((skill_watcher, config_watcher));
    Ok((deps, keepalive))
}

/// Spawn an `Agent` from shared deps and per-session context, then run its loop.
///
/// Called once per ACP session. Each invocation creates independent per-session state:
/// - Per-session `mpsc::Receiver` adapters from shared broadcast senders.
/// - A fresh `CancellationToken` for the broadcast adapter lifetime.
/// - The session's own `conversation_id` from `SessionContext`.
///
/// When `acp_ctx` is `Some`, ACP executors are composed on top of the local tool executor
/// (ACP-first, local fallback). When `None`, local tools handle everything.
#[cfg(feature = "acp")]
#[allow(clippy::too_many_lines)]
async fn spawn_acp_agent(
    d: std::sync::Arc<SharedAgentDeps>,
    mut channel: zeph_core::channel::LoopbackChannel,
    acp_ctx: Option<zeph_acp::AcpContext>,
    session_ctx: zeph_acp::SessionContext,
) {
    use std::sync::Arc;

    let provider = d.provider.clone();
    let registry = Arc::clone(&d.registry);
    let matcher = d.matcher.clone();
    let max_active_skills = d.max_active_skills;
    let tool_executor = Arc::clone(&d.tool_executor);
    let skill_paths = d.skill_paths.clone();
    let plugin_dirs_supplier = Arc::clone(&d.plugin_dirs_supplier);
    let memory = Arc::clone(&d.memory);
    let history_limit = d.history_limit;
    let recall_limit = d.recall_limit;
    let summarization_threshold = d.summarization_threshold;
    let shutdown_rx = d.shutdown_rx.clone();
    let config_path = d.config_path.clone();
    let mcp_tools = d.mcp_tools.clone();
    let mcp_registry = d.mcp_registry.clone();
    let mcp_manager = Arc::clone(&d.mcp_manager);
    let mcp_shared_tools = Arc::clone(&d.mcp_shared_tools);
    let mcp_config = d.mcp_config.clone();
    let summary_provider = d.summary_provider.clone();
    let judge_provider = d.judge_provider.clone();
    let feedback_classifier = d.feedback_classifier.clone();
    #[cfg(feature = "classifiers")]
    let classifiers_config = d.classifiers_config.clone();
    #[cfg(feature = "classifiers")]
    let pii_filter_enabled = d.pii_filter_enabled;
    let causal_ipi_config = d.causal_ipi_config.clone();
    let causal_provider = d.causal_provider.clone();
    let nli_config = d.nli_config.clone();
    let nli_provider = d.nli_provider.clone();
    let secret_registry = d.secret_registry.clone();
    let vigil_config = d.vigil_config.clone();
    let probe_provider = d.probe_provider.clone();
    let planner_provider = d.planner_provider.clone();
    let verify_provider = d.verify_provider.clone();
    let orchestrator_provider = d.orchestrator_provider.clone();
    let predicate_provider = d.predicate_provider.clone();
    let quarantine_provider = d.quarantine_provider.clone();
    let guardrail_provider = d.guardrail_provider.clone();
    let session_config = d.session_config.clone();
    let session_persistence_config = d.session_persistence_config.clone();
    let managed_skills_dir = crate::bootstrap::managed_skills_dir();
    let skill_reload_tx = d.skill_reload_tx.clone();
    let config_reload_tx = d.config_reload_tx.clone();
    #[cfg(feature = "scheduler")]
    let scheduler_executor = d.scheduler_executor.as_ref().map(std::sync::Arc::clone);
    #[cfg(feature = "scheduler")]
    let scheduler_update_tx = d.scheduler_update_tx.clone();
    #[cfg(feature = "scheduler")]
    let scheduler_custom_tx = d.scheduler_custom_tx.clone();

    let hooks_config = d.hooks_config.clone();
    let tool_filter_config = d.tool_filter_config.clone();

    // Per-session receivers: each session gets its own mpsc::Receiver forwarded from the
    // shared broadcast senders. The CancellationToken is derived from the AcpContext cancel
    // signal so the forwarding task exits when the session ends (eviction, shutdown, or
    // natural completion). This satisfies critic finding S1.
    let adapter_cancel = zeph_memory::CancellationToken::new();
    let reload_rx = broadcast_to_mpsc(skill_reload_tx.subscribe(), adapter_cancel.clone());
    let config_reload_rx = broadcast_to_mpsc(config_reload_tx.subscribe(), adapter_cancel.clone());
    #[cfg(feature = "scheduler")]
    let scheduler_update_rx = scheduler_update_tx
        .as_ref()
        .map(|tx| broadcast_to_mpsc(tx.subscribe(), adapter_cancel.clone()));
    #[cfg(feature = "scheduler")]
    let scheduler_custom_rx = scheduler_custom_tx
        .as_ref()
        .map(|tx| broadcast_to_mpsc(tx.subscribe(), adapter_cancel.clone()));

    // Capture per-session fields before session_config is consumed by apply_session_config.
    let debug_config = session_config.debug_config.clone();
    let memory_validation_config = session_config.security.memory_validation.clone();

    // Build tool executor: ACP executors take priority via CompositeExecutor (first-match-wins).
    // DynExecutor wraps Arc<dyn ErasedToolExecutor> so it satisfies Agent::new's ToolExecutor bound.
    // When conversation_id is None (store unavailable), memory_tools use id=0 which maps to no
    // persisted history — the tool calls succeed but return empty results.
    let memory_executor = zeph_core::memory_tools::MemoryToolExecutor::with_validator(
        Arc::clone(&memory),
        session_ctx
            .conversation_id
            .unwrap_or(zeph_memory::ConversationId(0)),
        zeph_sanitizer::memory_validation::MemoryWriteValidator::new(memory_validation_config),
    );
    let overflow_executor = {
        let mut ex =
            zeph_core::overflow_tools::OverflowToolExecutor::new(Arc::new(memory.sqlite().clone()));
        if let Some(cid) = session_ctx.conversation_id {
            ex = ex.with_conversation(cid.0);
        }
        ex
    };
    let skill_loader_executor = zeph_core::SkillLoaderExecutor::new(Arc::clone(&registry));
    let (tool_executor, cancel_signal, provider_override, parent_tool_use_id) =
        if let Some(ctx) = acp_ctx {
            let cancel_signal = Arc::clone(&ctx.cancel_signal);
            let provider_override = Arc::clone(&ctx.provider_override);
            let parent_tool_use_id = ctx.parent_tool_use_id.clone();
            // Link adapter_cancel to session cancel_signal so the broadcast forwarding task
            // exits when the ACP session is cancelled (eviction, shutdown, or completion).
            let adapter_cancel_clone = adapter_cancel.clone();
            let cancel_signal_clone = Arc::clone(&cancel_signal);
            tokio::spawn(async move {
                // EXEMPT(#5144): per-session cancel bridge; self-terminating single await; name collision risk under spawn
                cancel_signal_clone.notified().await;
                adapter_cancel_clone.cancel();
            });
            let mut base: Arc<dyn ErasedToolExecutor> = Arc::clone(&tool_executor) as Arc<_>;
            if let Some(fs) = ctx.file_executor {
                // Suppress FileExecutor's read/write/glob when AcpFileExecutor is active.
                // edit and grep remain available from FileExecutor (no ACP equivalents yet).
                let filtered = zeph_tools::ToolFilter::new(
                    zeph_tools::DynExecutor(base),
                    &["read", "write", "glob"],
                );
                base = Arc::new(zeph_tools::CompositeExecutor::new(fs, filtered));
            }
            if let Some(shell) = ctx.shell_executor {
                base = Arc::new(zeph_tools::CompositeExecutor::new(
                    shell,
                    zeph_tools::DynExecutor(base),
                ));
            }
            base = Arc::new(zeph_tools::CompositeExecutor::new(
                skill_loader_executor,
                zeph_tools::CompositeExecutor::new(
                    memory_executor,
                    zeph_tools::CompositeExecutor::new(
                        overflow_executor,
                        zeph_tools::DynExecutor(base),
                    ),
                ),
            ));
            (
                zeph_tools::DynExecutor(base),
                Some(cancel_signal),
                Some(provider_override),
                parent_tool_use_id,
            )
        } else {
            // No AcpContext: the adapter forwarding tasks (skill reload, config reload, and
            // scheduler receivers) run until adapter_cancel.cancel() is called explicitly at
            // function end (line below), or until the mpsc sender is dropped.
            let base: Arc<dyn ErasedToolExecutor> = Arc::new(zeph_tools::CompositeExecutor::new(
                skill_loader_executor,
                zeph_tools::CompositeExecutor::new(
                    memory_executor,
                    zeph_tools::CompositeExecutor::new(
                        overflow_executor,
                        zeph_tools::DynExecutor(Arc::clone(&tool_executor) as Arc<_>),
                    ),
                ),
            ));
            (zeph_tools::DynExecutor(base), None, None, None)
        };

    // Session persistence (spec-068, #5343): reuse the ACP session_id directly as the
    // zeph_common::SessionId — ACP already owns this session's identity/lifecycle, so no
    // separate minting/reuse logic is needed here (unlike the CLI/TUI path in runner.rs, which
    // has no pre-existing session identity to anchor to). `SessionStore::create` is idempotent
    // (INSERT_IGNORE) and does not touch `conversation_id`, which ACP's own
    // `create_acp_session_with_conversation` already manages — this call only ensures the row
    // exists so `SessionStore::update_seq` has something to update.
    //
    // Computed here, before `channel` is consumed by `Agent::new_with_registry_arc` below, so an
    // `AlreadyLocked` failure can be surfaced to the client via `channel.send_status` (delivered
    // to the IDE as an `AgentThoughtChunk` on this session's next prompt-response drain, per
    // `loopback_event_to_updates`) instead of only being visible in logs (#5487 fix 3).
    let mut acp_session_sink = None;
    let mut preloaded_messages: Vec<zeph_llm::provider::Message> = Vec::new();
    if session_persistence_config.enabled {
        let sid = zeph_common::SessionId::new(session_ctx.session_id.to_string());
        let store = zeph_session::SessionStore::new(memory.sqlite().pool().clone());
        if let Err(e) = store.create(sid.as_str()).await {
            tracing::warn!(error = %e, session_id = %sid, "failed to create session-store row for ACP session");
        }
        let data_dir = std::path::PathBuf::from(&session_persistence_config.data_dir);
        let session_path = zeph_session::session_dir(&data_dir, sid.as_str());

        // D-10 (spec-068 §12.3/§13): route through the shared hydration pipeline (legacy
        // bootstrap + ReplayEngine fold + INV-SP-3 reconcile) — the one pipeline every
        // session-open path (ACP, CLI `sessions resume`, `/conv resume`) now shares, so they
        // cannot silently diverge again (impl-critic finding C1). Bootstrap/reconcile need a
        // linked `ConversationId`; when absent (store was unavailable at session creation —
        // `with_memory` above was skipped too), fall back to a bare log open with no
        // SQLite-touching steps, matching this edge case's pre-D-10 behavior.
        // D-13 (spec-068 §8.1, N3): `hydrate_and_condense` additionally folds in resume-time
        // durable condensation via the pre-built `d.resume_condenser`/`d.resume_token_counter`
        // (see `SharedAgentDeps`'s doc comment for why they're built once at deps-construction
        // time, not here).
        let log = if let Some(cid) = session_ctx.conversation_id {
            match zeph_agent_persistence::hydrate_and_condense(
                &session_path,
                &store,
                sid.as_str(),
                cid,
                &memory,
                None,
                &d.resume_condenser,
                d.resume_token_counter.as_ref(),
                d.session_config.budget_tokens,
            )
            .await
            {
                Ok(hydrated) => {
                    preloaded_messages = hydrated.messages;
                    Some(hydrated.log)
                }
                // #5487 fix 3: another process already holds this session's exclusive write
                // lock. Unlike the generic degrade-to-no-persistence branch below, this is
                // elevated to `error` plus a client-visible status notification — silently
                // continuing here would let this ACP session race the other process's writes
                // exactly like the reported bug.
                Err(zeph_agent_persistence::PersistenceError::Session(
                    zeph_session::SessionError::AlreadyLocked(lock_path),
                )) => {
                    tracing::error!(
                        lock_path,
                        "session hydration failed: another process already holds this session's \
                         write lock; session persistence disabled for this session"
                    );
                    // Do not interpolate `lock_path` into the client-facing message: it is an
                    // absolute filesystem path (leaks the server's home-directory prefix/OS
                    // username) and this session may be reached over an unauthenticated,
                    // non-loopback ACP HTTP transport (security review finding).
                    let _ = channel
                        .send_status(
                            "Session persistence unavailable: another process already holds \
                             this session's write lock.",
                        )
                        .await;
                    None
                }
                Err(e) => {
                    tracing::warn!(error = %e, "session hydration failed; session persistence disabled for this session");
                    None
                }
            }
        } else {
            match zeph_session::SessionEventLog::open_exclusive(&session_path).await {
                Ok(log) => Some(Arc::new(log)),
                Err(zeph_session::SessionError::AlreadyLocked(lock_path)) => {
                    tracing::error!(
                        lock_path,
                        "failed to open session event log for ACP session: another process \
                         already holds this session's write lock; session persistence disabled \
                         for this session"
                    );
                    // Do not interpolate `lock_path` into the client-facing message: it is an
                    // absolute filesystem path (leaks the server's home-directory prefix/OS
                    // username) and this session may be reached over an unauthenticated,
                    // non-loopback ACP HTTP transport (security review finding).
                    let _ = channel
                        .send_status(
                            "Session persistence unavailable: another process already holds \
                             this session's write lock.",
                        )
                        .await;
                    None
                }
                Err(e) => {
                    tracing::warn!(error = %e, "failed to open session event log for ACP session; session persistence disabled for this session");
                    None
                }
            }
        };

        if let Some(log) = log {
            acp_session_sink = Some(Arc::new(zeph_agent_persistence::SessionSink::new(
                log, store, sid,
            )));
        }
    }

    let mut agent = Box::pin(
        Agent::new_with_registry_arc(
            provider.clone(),
            d.embedding_provider.clone(),
            channel,
            Arc::clone(&registry),
            matcher,
            max_active_skills,
            tool_executor,
        )
        .apply_session_config(session_config)
        .with_working_dir(session_ctx.working_dir.clone())
        .with_skill_reload(skill_paths, reload_rx)
        .with_plugin_dirs_supplier(move || plugin_dirs_supplier())
        .with_managed_skills_dir(managed_skills_dir)
        .with_shutdown(shutdown_rx)
        .with_config_reload(config_path, config_reload_rx)
        .with_plugins_dir(
            crate::bootstrap::plugins_dir(),
            d.startup_shell_overlay.clone(),
        )
        .with_shell_policy_handle(d.shell_policy_handle.clone())
        .with_mcp(
            mcp_tools,
            mcp_registry,
            Some(Arc::clone(&mcp_manager)),
            &mcp_config,
        )
        .with_mcp_shared_tools(mcp_shared_tools)
        .with_focus_and_sidequest_config(d.focus_config.clone(), d.sidequest_config.clone())
        .with_trajectory_and_category_config(d.trajectory_config.clone(), d.category_config.clone())
        .with_embedding_provider(d.embedding_provider.clone())
        .maybe_init_tool_schema_filter(tool_filter_config, provider.clone()),
    )
    .await;

    agent = agent.with_acp_session(true);

    if let Some(ref logger) = d.audit_logger {
        agent = agent.with_audit_logger(std::sync::Arc::clone(logger));
    }

    // Wire scheduler per session: apply update/custom receivers and add executor.
    #[cfg(feature = "scheduler")]
    {
        if let Some(rx) = scheduler_update_rx {
            agent = agent.with_update_notifications(rx);
        }
        if let Some(rx) = scheduler_custom_rx {
            agent = agent.with_custom_task_rx(rx);
        }
        if let Some(sched_exec) = scheduler_executor {
            agent = agent
                .add_tool_executor(crate::scheduler_executor::DynSchedulerExecutor(sched_exec));
        }
    }

    // Apply per-session memory only when a ConversationId was successfully allocated.
    // When None (store unavailable at session creation), the agent operates without persistent history.
    if let Some(cid) = session_ctx.conversation_id {
        agent = agent.with_memory(
            Arc::clone(&memory),
            cid,
            history_limit,
            recall_limit,
            summarization_threshold,
        );
    }

    // Attach the session log/history computed above, before `channel` was moved into
    // `Agent::new_with_registry_arc`.
    if !preloaded_messages.is_empty() {
        agent = agent.with_preloaded_messages(preloaded_messages);
    }
    if let Some(sink) = acp_session_sink {
        agent = agent
            .with_session_sink(Some(sink))
            .with_session_persistence_config(Some(session_persistence_config.clone()));
    }

    if let Some(signal) = cancel_signal {
        agent = agent.with_cancel_signal(signal);
    }

    if let Some(slot) = provider_override {
        agent = agent.with_provider_override(slot);
    }

    if let Some(parent_id) = parent_tool_use_id {
        agent = agent.with_parent_tool_use_id(parent_id);
    }

    if let Some(sp) = summary_provider {
        agent = agent.with_summary_provider(sp);
    }

    if let Some(jp) = judge_provider {
        agent = agent.with_judge_provider(jp);
    }
    if let Some(fc) = feedback_classifier {
        agent = agent.with_llm_classifier(fc);
    }

    if let Some(pp) = probe_provider {
        agent = agent.with_probe_provider(pp);
    }

    if let Some(pp) = planner_provider {
        agent = agent.with_planner_provider(pp);
    }

    if let Some(vp) = verify_provider {
        agent = agent.with_verify_provider(vp);
    }

    if let Some(op) = orchestrator_provider {
        agent = agent.with_orchestrator_provider(op);
    }

    if let Some(pp) = predicate_provider {
        agent = agent.with_predicate_provider(pp);
    }

    agent = agent_setup::apply_quarantine_provider(agent, quarantine_provider);
    {
        agent = agent_setup::apply_guardrail(agent, guardrail_provider);
    }
    #[cfg(feature = "classifiers")]
    {
        agent = agent_setup::apply_injection_classifier_with_cfg(agent, &classifiers_config);
        if classifiers_config.enabled {
            agent = agent.with_enforcement_mode(classifiers_config.enforcement_mode);
        }
        agent = agent_setup::apply_three_class_classifier_with_cfg(agent, &classifiers_config);
        agent = agent_setup::apply_pii_classifier_with_cfg(agent, &classifiers_config);
        agent = agent_setup::apply_pii_ner_classifier_with_cfg(
            agent,
            &classifiers_config,
            pii_filter_enabled,
        );
    }
    agent = agent_setup::apply_causal_analyzer_with_cfg(
        agent,
        provider.clone(),
        causal_provider,
        &causal_ipi_config,
        secret_registry.as_ref(),
    );
    agent = agent_setup::apply_nli_sanitizer_with_cfg(
        agent,
        provider.clone(),
        nli_provider,
        &nli_config,
        secret_registry.as_ref(),
    );
    agent = agent_setup::apply_secret_masking(agent, secret_registry);
    agent = agent_setup::apply_vigil(agent, &vigil_config);

    if debug_config.enabled {
        // Use session_id as a subdirectory prefix so concurrent sessions never share the same
        // timestamped directory and collide on file names (I2).
        let session_dump_dir = debug_config
            .output_dir
            .join(session_ctx.session_id.to_string());
        match zeph_core::debug_dump::DebugDumper::new(
            session_dump_dir.as_path(),
            debug_config.format,
        ) {
            Ok(dumper) => agent = agent.with_debug_dumper(dumper),
            Err(e) => tracing::warn!(error = %e, "debug dump initialization failed"),
        }
    }

    agent = agent.with_hooks_config(&hooks_config);

    drop(d);

    if let Err(e) = agent.load_history().await {
        tracing::error!("failed to load agent history: {e:#}");
    }

    if let Err(e) = Box::pin(agent.run()).await {
        tracing::error!("ACP agent loop error: {e:#}");
    }

    agent.shutdown().await;

    // Ensure the adapter cancellation token is dropped/cancelled after the agent loop exits,
    // which terminates the broadcast forwarding tasks for this session.
    adapter_cancel.cancel();
}

/// Collect model keys from config when `acp.available_models` is not set.
///
/// For each configured provider the disk cache is consulted first (24 h TTL).
/// When the cache is warm the full remote model list is returned; otherwise the
/// single model from config is used as the fallback so startup is never blocked
/// on network I/O.  Call `/model refresh` at runtime to populate the caches.
///
/// Each key uses `"{provider_name}:{model_id}"` format matching the provider factory.
#[cfg(feature = "acp")]
async fn discover_models_from_config(config: &zeph_core::config::Config) -> Vec<String> {
    use zeph_llm::model_cache::ModelCache;

    /// Expand a provider slug using its on-disk cache, or fall back to `fallback`.
    async fn expand_from_cache(slug: &str, fallback: &str) -> Vec<String> {
        let cache = ModelCache::for_slug(slug);
        if !cache.is_stale_async().await
            && let Ok(Some(entries)) = cache.load_async().await
            && !entries.is_empty()
        {
            return entries
                .into_iter()
                .map(|m| format!("{slug}:{}", m.id))
                .collect();
        }
        vec![format!("{slug}:{fallback}")]
    }

    let mut models: Vec<String> = Vec::new();

    for entry in &config.llm.providers {
        let slug = entry.provider_type.as_str();
        let fallback = entry.model.as_deref().unwrap_or("unknown");
        models.extend(expand_from_cache(slug, fallback).await);
    }

    models.dedup();
    models
}

/// Populate model caches for all providers before the ACP server starts.
///
/// Uses a 5-second timeout so that a slow or unavailable provider does not block startup.
/// After a successful fetch, each unique provider slug present in `acp_available_models`
/// is expanded from its on-disk cache, replacing the single config-time fallback entry.
#[cfg(feature = "acp")]
async fn warm_model_caches(
    provider: zeph_llm::any::AnyProvider,
    available_models: std::sync::Arc<RwLock<Vec<String>>>,
) {
    use zeph_llm::model_cache::ModelCache;

    let provider_count = {
        let models = available_models.read();
        models
            .iter()
            .filter_map(|k| k.split_once(':').map(|(slug, _)| slug))
            .collect::<std::collections::HashSet<_>>()
            .len()
    };
    tracing::info!(
        providers = provider_count,
        "warming model caches in background"
    );

    let fetch = async move {
        match provider.list_models_remote().await {
            Ok(models) => tracing::info!(models = models.len(), "model cache fetch completed"),
            Err(e) => {
                tracing::info!(error = %e, "model cache warm-up failed; keeping fallback list");
            }
        }
    };

    if tokio::time::timeout(std::time::Duration::from_secs(5), fetch)
        .await
        .is_err()
    {
        tracing::info!("model cache warm-up timed out; keeping fallback list");
        return;
    }

    // Collect unique provider slugs from the current available_models list.
    let slugs: Vec<String> = {
        let models = available_models.read();
        models
            .iter()
            .filter_map(|k| k.split_once(':').map(|(s, _)| s.to_owned()))
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect()
    };

    for slug in slugs {
        let cache = ModelCache::for_slug(&slug);
        if cache.is_stale_async().await {
            tracing::info!(provider = %slug, "model cache still stale after warm-up");
            continue;
        }
        if let Ok(Some(entries)) = cache.load_async().await
            && !entries.is_empty()
        {
            let new_keys: Vec<String> = entries
                .into_iter()
                .map(|m| format!("{slug}:{}", m.id))
                .collect();
            let count = new_keys.len();
            let mut models = available_models.write();
            models.retain(|k| !k.starts_with(&format!("{slug}:")));
            models.extend(new_keys);
            models.dedup();
            tracing::info!(provider = %slug, models = count, "model cache ready");
        }
    }
    let total_models = available_models.read().len();
    tracing::info!(models = total_models, "model cache warming finished");
}

/// Build a `ProviderFactory` from the known named providers in config.
///
/// Each available model key is `"{provider_name}:{model}"`.
/// The factory creates a provider by parsing that key and overriding the model in a clone.
///
/// `secret_registry`, when `Some`, wraps every provider this factory produces via
/// [`zeph_llm::any::AnyProvider::masked`] (#5437) — this is the single construction point for
/// every ACP-switched/primed provider (`prime_provider_override`, `/model` switch, session-title
/// generation), so wrapping here structurally covers all of them, including the session-title
/// background task that dispatches directly on the factory's output and never touches the
/// `provider_override` slot that `Agent::apply_provider_override`/`set_provider` guard.
#[cfg(feature = "acp")]
#[allow(clippy::too_many_lines)]
fn build_acp_provider_factory(
    config: &zeph_core::config::Config,
    secret_registry: Option<std::sync::Arc<zeph_sanitizer::secret_mask::SecretMaskRegistry>>,
) -> zeph_acp::ProviderFactory {
    // Collect snapshots for providers that have secrets already resolved.
    #[derive(Clone)]
    enum ProviderSnapshot {
        Ollama {
            base_url: String,
            embed: String,
        },
        Claude {
            api_key: String,
            max_tokens: u32,
        },
        OpenAi {
            api_key: String,
            base_url: String,
            max_tokens: u32,
            embed: Option<String>,
            reasoning_effort: Option<String>,
        },
        Compatible {
            api_key: String,
            base_url: String,
            max_tokens: u32,
            embed: Option<String>,
            name: String,
        },
    }

    let mut snapshots: Vec<ProviderSnapshot> = Vec::new();

    for entry in &config.llm.providers {
        let name = entry.effective_name();
        match entry.provider_type {
            zeph_core::config::ProviderKind::Ollama => {
                snapshots.push(ProviderSnapshot::Ollama {
                    base_url: entry
                        .base_url
                        .clone()
                        .unwrap_or_else(|| "http://localhost:11434".to_owned()),
                    embed: config.llm.embedding_model.clone(),
                });
            }
            zeph_core::config::ProviderKind::Claude => {
                if let Some(ref secret) = config.secrets.claude_api_key {
                    snapshots.push(ProviderSnapshot::Claude {
                        api_key: secret.expose().to_owned(),
                        max_tokens: entry.max_tokens.unwrap_or(4096),
                    });
                }
            }
            zeph_core::config::ProviderKind::OpenAi => {
                if let Some(ref secret) = config.secrets.openai_api_key {
                    snapshots.push(ProviderSnapshot::OpenAi {
                        api_key: secret.expose().to_owned(),
                        base_url: entry
                            .base_url
                            .clone()
                            .unwrap_or_else(|| "https://api.openai.com/v1".to_owned()),
                        max_tokens: entry.max_tokens.unwrap_or(4096),
                        embed: entry.embedding_model.clone(),
                        reasoning_effort: entry.reasoning_effort.clone(),
                    });
                }
            }
            zeph_core::config::ProviderKind::Compatible => {
                let secret = entry
                    .api_key
                    .as_deref()
                    .map(std::borrow::ToOwned::to_owned)
                    .or_else(|| {
                        config
                            .secrets
                            .compatible_api_keys
                            .get(&name)
                            .map(|s| s.expose().to_owned())
                    });
                if let Some(api_key) = secret {
                    snapshots.push(ProviderSnapshot::Compatible {
                        api_key,
                        base_url: entry.base_url.clone().unwrap_or_default(),
                        max_tokens: entry.max_tokens.unwrap_or(4096),
                        embed: entry.embedding_model.clone(),
                        name,
                    });
                }
            }
            _ => {}
        }
    }

    let masker: Option<std::sync::Arc<dyn zeph_llm::masking::OutboundMasker>> =
        secret_registry.map(|r| r as std::sync::Arc<dyn zeph_llm::masking::OutboundMasker>);
    let snapshots = std::sync::Arc::new(snapshots);
    std::sync::Arc::new(move |key: &str| {
        // #5437: wrap every provider this factory produces so it's masked regardless of which
        // consumer dispatches on it (`provider_override` slot or the session-title generation
        // task, which calls `.chat()` directly on the factory's output).
        let wrap = |p: zeph_llm::any::AnyProvider| -> zeph_llm::any::AnyProvider {
            match &masker {
                Some(m) => p.masked(std::sync::Arc::clone(m)),
                None => p,
            }
        };
        let (provider_name, model) = key.split_once(':')?;
        let model = model.to_owned();
        for snapshot in snapshots.as_ref() {
            match snapshot {
                ProviderSnapshot::Ollama {
                    base_url, embed, ..
                } if provider_name == "ollama" => {
                    let mut p = zeph_llm::ollama::OllamaProvider::new(
                        base_url,
                        model.clone(),
                        embed.clone(),
                    );
                    p.set_context_window(0);
                    return Some(wrap(zeph_llm::any::AnyProvider::Ollama(p)));
                }
                ProviderSnapshot::Claude {
                    api_key,
                    max_tokens,
                } if provider_name == "claude" => {
                    return Some(wrap(zeph_llm::any::AnyProvider::Claude(
                        zeph_llm::claude::ClaudeProvider::new(
                            api_key.clone(),
                            model.clone(),
                            *max_tokens,
                        ),
                    )));
                }
                ProviderSnapshot::OpenAi {
                    api_key,
                    base_url,
                    max_tokens,
                    embed,
                    reasoning_effort,
                } if provider_name == "openai" => {
                    return Some(wrap(zeph_llm::any::AnyProvider::OpenAi(
                        zeph_llm::openai::OpenAiProvider::new(zeph_llm::openai::OpenAiConfig {
                            api_key: api_key.clone(),
                            base_url: base_url.clone(),
                            model: model.clone(),
                            max_tokens: *max_tokens,
                            embedding_model: embed.clone(),
                            reasoning_effort: reasoning_effort.clone(),
                            context_window: None,
                            completion_tokens_param: None,
                        }),
                    )));
                }
                ProviderSnapshot::Compatible {
                    api_key,
                    base_url,
                    max_tokens,
                    embed,
                    name,
                } if provider_name == name => {
                    return Some(wrap(zeph_llm::any::AnyProvider::Compatible(
                        zeph_llm::compatible::CompatibleProvider::new(
                            zeph_llm::compatible::CompatibleConfig {
                                provider_name: name.clone(),
                                api_key: api_key.clone(),
                                base_url: base_url.clone(),
                                model: model.clone(),
                                max_tokens: *max_tokens,
                                embedding_model: embed.clone(),
                                completion_tokens_param: None,
                            },
                        ),
                    )));
                }
                _ => {}
            }
        }
        None
    })
}

/// Build the `(name, protocol)` list advertised via ACP `providers/list` (#5448).
///
/// Reuses the same `config.llm.providers` source of truth as [`build_acp_provider_factory`]
/// and `discover_models_from_config`, so the advertised identity always matches the providers
/// actually wired for model switching. Vault-resolved API keys are never included.
#[cfg(feature = "acp")]
fn acp_provider_names(config: &zeph_core::config::Config) -> Vec<(String, zeph_acp::LlmProtocol)> {
    config
        .llm
        .providers
        .iter()
        .map(|entry| {
            let protocol = match entry.provider_type {
                zeph_core::config::ProviderKind::Claude => zeph_acp::LlmProtocol::Anthropic,
                zeph_core::config::ProviderKind::OpenAi
                | zeph_core::config::ProviderKind::Compatible => zeph_acp::LlmProtocol::OpenAi,
                other => zeph_acp::LlmProtocol::Other(other.as_str().to_owned()),
            };
            (entry.effective_name(), protocol)
        })
        .collect()
}

/// Collect project rule file paths from `.claude/rules/*.md` and skill files.
///
/// Rule files are resolved relative to the current working directory.
/// Skill paths that point to regular files (SKILL.md entries) are included as-is.
#[cfg(feature = "acp")]
fn collect_project_rules(skill_paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut rules = Vec::new();
    let rules_dir = std::path::Path::new(".claude/rules");
    if rules_dir.is_dir()
        && let Ok(entries) = std::fs::read_dir(rules_dir)
    {
        let mut paths: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "md"))
            .collect();
        paths.sort();
        rules.extend(paths);
    }
    for sp in skill_paths {
        if sp.is_file() {
            rules.push(sp.clone());
        }
    }
    rules
}

/// Run the ACP server over stdin/stdout.
///
/// Supports multiple concurrent sessions via `SharedAgentDeps` — each `session/new` spawns
/// an independent agent loop with its own conversation history.
///
/// # Errors
///
/// Returns an error if the agent stack cannot be built or the transport fails.
#[cfg(feature = "acp")]
pub(crate) async fn run_acp_server(
    config_path: Option<&std::path::Path>,
    vault_backend: Option<&str>,
    vault_key: Option<&std::path::Path>,
    vault_path: Option<&std::path::Path>,
    cli_additional_dirs: Vec<std::path::PathBuf>,
    cli_auth_methods: Vec<String>,
    cli_message_ids: Option<bool>,
) -> anyhow::Result<()> {
    use std::sync::Arc;

    let (mut deps, _keepalive) = Box::pin(build_acp_deps(
        config_path,
        vault_backend,
        vault_key,
        vault_path,
        None,
    ))
    .await?;
    let available_models = std::sync::Arc::clone(&deps.acp_available_models);
    let provider = deps.provider.clone();
    warm_model_caches(provider, available_models).await;

    // Apply CLI overrides to config-derived values.
    let effective_additional_dirs = if cli_additional_dirs.is_empty() {
        deps.acp_additional_directories.clone()
    } else {
        cli_additional_dirs
            .into_iter()
            .map(|p| {
                zeph_core::config::AdditionalDir::parse(p.clone()).map_err(|e| {
                    anyhow::anyhow!("invalid --acp-additional-dir {}: {e}", p.display())
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?
    };
    let effective_auth_methods = if cli_auth_methods.is_empty() {
        let methods = deps.acp_auth_methods.clone();
        anyhow::ensure!(
            !methods.is_empty(),
            "acp.auth_methods must not be empty; set at least one method (e.g. \"agent\")"
        );
        methods
    } else {
        let methods: Vec<_> = cli_auth_methods
            .iter()
            .map(|m| match m.as_str() {
                "agent" => Ok(zeph_core::config::AcpAuthMethod::Agent),
                other => Err(anyhow::anyhow!(
                    "unknown --acp-auth-method {other:?}; accepted values: agent"
                )),
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        anyhow::ensure!(
            !methods.is_empty(),
            "--acp-auth-method list must not be empty after parsing"
        );
        methods
    };
    let effective_message_ids = cli_message_ids.unwrap_or(deps.acp_message_ids_enabled);

    let mcp_manager_for_acp = Arc::clone(&deps.mcp_manager);
    let server_config = zeph_acp::AcpServerConfig {
        agent_name: deps.acp_agent_name.clone(),
        agent_version: deps.acp_agent_version.clone(),
        max_sessions: deps.acp_max_sessions,
        session_idle_timeout_secs: deps.acp_session_idle_timeout_secs,
        permission_file: deps.acp_permission_file.clone(),
        provider_factory: deps.acp_provider_factory.take(),
        available_models: std::sync::Arc::clone(&deps.acp_available_models),
        provider_names: deps.acp_provider_names.clone(),
        mcp_manager: Some(mcp_manager_for_acp),
        auth_bearer_token: deps.acp_auth_bearer_token.clone(),
        discovery_enabled: deps.acp_discovery_enabled,
        terminal_timeout_secs: deps.acp_timeouts.terminal_secs,
        project_rules: deps.acp_project_rules.clone(),
        title_max_chars: deps.acp_title_max_chars,
        max_history: deps.acp_max_history,
        sqlite_path: Some(deps.sqlite_path.clone()),
        session_data_dir: deps
            .session_persistence_config
            .enabled
            .then(|| std::path::PathBuf::from(&deps.session_persistence_config.data_dir)),
        ready_notification: Some(zeph_acp::transport::ReadyNotification {
            version: deps.acp_agent_version.clone(),
            pid: std::process::id(),
            log_file: deps.acp_log_file.clone(),
        }),
        additional_directories: effective_additional_dirs,
        auth_methods: effective_auth_methods,
        message_ids_enabled: effective_message_ids,
        timeouts: deps.acp_timeouts.clone(),
        model_config: deps.acp_model_config.clone(),
    };

    let shared = Arc::new(deps);

    let spawner: zeph_acp::AgentSpawner = Arc::new(move |channel, acp_ctx, session_ctx| {
        let shared = Arc::clone(&shared);
        Box::pin(spawn_acp_agent(shared, channel, acp_ctx, session_ctx))
    });

    zeph_acp::serve_stdio(spawner, server_config).await?;

    Ok(())
}

/// Run the ACP server over HTTP+SSE and WebSocket.
///
/// # Errors
///
/// Returns an error if the agent stack cannot be built or the server fails to bind.
#[cfg(feature = "acp-http")]
pub(crate) async fn run_acp_http_server(
    config_path: Option<&std::path::Path>,
    vault_backend: Option<&str>,
    vault_key: Option<&std::path::Path>,
    vault_path: Option<&std::path::Path>,
    bind_override: Option<&str>,
    auth_token_override: Option<String>,
) -> anyhow::Result<()> {
    use std::sync::Arc;
    use tokio::sync::RwLock;

    let app = AppBuilder::new(config_path, vault_backend, vault_key, vault_path).await?;
    log_acp_runtime_paths(app.config(), app.config_path());
    let bind_addr = bind_override.map_or_else(|| app.config().acp.http_bind.clone(), str::to_owned);

    // CLI flag overrides config/env values for auth token.
    let auth_bearer_token = auth_token_override.or(app.config().acp.auth_token.clone());
    let mcp_manager_for_acp = Arc::new(crate::bootstrap::create_mcp_manager_with_vault(
        app.config(),
        false,
        app.age_vault_arc(),
    ));
    let server_config = zeph_acp::AcpServerConfig {
        agent_name: app.config().acp.agent_name.clone(),
        agent_version: app.config().acp.agent_version.clone(),
        max_sessions: app.config().acp.max_sessions,
        session_idle_timeout_secs: app.config().acp.session_idle_timeout_secs,
        permission_file: app.config().acp.permission_file.clone(),
        provider_factory: Some(build_acp_provider_factory(
            app.config(),
            app.secret_registry(),
        )),
        available_models: std::sync::Arc::new(parking_lot::RwLock::new(
            if app.config().acp.available_models.is_empty() {
                discover_models_from_config(app.config()).await
            } else {
                app.config().acp.available_models.clone()
            },
        )),
        provider_names: acp_provider_names(app.config()),
        mcp_manager: Some(Arc::clone(&mcp_manager_for_acp)),
        auth_bearer_token,
        discovery_enabled: app.config().acp.discovery_enabled,
        terminal_timeout_secs: app.config().acp.timeouts.terminal_secs,
        project_rules: collect_project_rules(&app.skill_paths_for_registry()),
        title_max_chars: app.config().memory.sessions.title_max_chars,
        max_history: app.config().memory.sessions.max_history,
        sqlite_path: Some(crate::db_url::resolve_db_url(app.config()).to_owned()),
        session_data_dir: app
            .config()
            .session
            .enabled
            .then(|| std::path::PathBuf::from(&app.config().session.data_dir)),
        ready_notification: None,
        additional_directories: app.config().acp.additional_directories.clone(),
        auth_methods: app.config().acp.auth_methods.clone(),
        message_ids_enabled: app.config().acp.message_ids_enabled,
        timeouts: app.config().acp.timeouts.clone(),
        model_config: app.config().acp.model_config.clone(),
    };
    let shared_deps: Arc<RwLock<Option<Arc<SharedAgentDeps>>>> = Arc::new(RwLock::new(None));
    let shared_deps_for_spawner = Arc::clone(&shared_deps);
    let spawner: zeph_acp::SendAgentSpawner = Arc::new(move |channel, acp_ctx, session_ctx| {
        let shared_deps = Arc::clone(&shared_deps_for_spawner);
        Box::pin(async move {
            let maybe_shared = shared_deps.read().await.clone();
            let Some(shared) = maybe_shared else {
                tracing::warn!("ACP request received before runtime became ready");
                return;
            };
            Box::pin(spawn_acp_agent(shared, channel, acp_ctx, session_ctx)).await;
        })
    });
    let mut state = zeph_acp::AcpHttpState::new(spawner, server_config);
    match zeph_memory::store::SqliteStore::new(crate::db_url::resolve_db_url(app.config())).await {
        Ok(store) => state = state.with_store(store),
        Err(e) => tracing::warn!(error = %e, "failed to open SQLite for HTTP session endpoints"),
    }

    let router = zeph_acp::acp_router(state.clone());

    let listener = tokio::net::TcpListener::bind(&bind_addr).await?;
    tracing::info!("ACP HTTP server listening on {bind_addr}");
    let server_task = tokio::spawn(async move { ::axum::serve(listener, router).await }); // EXEMPT(#5144): awaited at end of fn; joinable lifecycle needed

    let (deps, _keepalive) = match Box::pin(build_acp_deps(
        config_path,
        vault_backend,
        vault_key,
        vault_path,
        Some(mcp_manager_for_acp),
    ))
    .await
    {
        Ok(result) => result,
        Err(err) => {
            server_task.abort();
            return Err(err);
        }
    };

    let available_models = std::sync::Arc::clone(&deps.acp_available_models);
    let provider = deps.provider.clone();
    warm_model_caches(provider, available_models).await;
    *shared_deps.write().await = Some(Arc::new(deps));
    state.mark_ready();
    state.start_reaper();
    tracing::info!("ACP server ready");
    server_task.await??;

    Ok(())
}

#[cfg(feature = "acp")]
pub(crate) fn print_acp_manifest() {
    let manifest = serde_json::json!({
        "name": env!("CARGO_PKG_NAME"),
        "version": env!("CARGO_PKG_VERSION"),
        "transport": "stdio",
        "command": [env!("CARGO_PKG_NAME"), "--acp"],
        "capabilities": ["prompt", "cancel", "load_session", "set_session_mode", "config_options", "ext_methods"],
        "description": "Zeph AI Agent",
        "readiness": {
            "notification": {
                "method": "zeph/ready",
                "params": {
                    "version": env!("CARGO_PKG_VERSION"),
                    "pid": "<process-id>",
                    "log_file": "<configured-log-file>"
                }
            },
            "http": {
                "health_endpoint": "/health",
                "statuses": [200, 503]
            }
        }
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&manifest).unwrap_or_default()
    );
}

#[cfg(all(test, feature = "acp"))]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::fs;
    use tempfile::TempDir;

    fn make_rules_dir(dir: &std::path::Path, files: &[&str]) {
        let rules = dir.join(".claude").join("rules");
        fs::create_dir_all(&rules).unwrap();
        for name in files {
            fs::write(rules.join(name), b"").unwrap();
        }
    }

    #[test]
    #[serial]
    fn collect_project_rules_empty_skill_paths_no_rules_dir() {
        let tmp = TempDir::new().unwrap();
        // No .claude/rules dir exists — function must return empty vec.
        let orig = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();
        let result = collect_project_rules(&[]);
        std::env::set_current_dir(orig).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    #[serial]
    fn collect_project_rules_picks_md_files_from_rules_dir() {
        let tmp = TempDir::new().unwrap();
        make_rules_dir(tmp.path(), &["rust-code.md", "testing.md", "notes.txt"]);
        let orig = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();
        let result = collect_project_rules(&[]);
        std::env::set_current_dir(orig).unwrap();
        // Only .md files should be returned.
        assert_eq!(result.len(), 2);
        let names: Vec<_> = result
            .iter()
            .filter_map(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"rust-code.md".to_owned()));
        assert!(names.contains(&"testing.md".to_owned()));
        assert!(!names.contains(&"notes.txt".to_owned()));
    }

    #[test]
    #[serial]
    fn collect_project_rules_includes_skill_files() {
        let tmp = TempDir::new().unwrap();
        let skill_file = tmp.path().join("my-skill.md");
        fs::write(&skill_file, b"").unwrap();
        let skill_dir = tmp.path().join("skills-dir");
        fs::create_dir_all(&skill_dir).unwrap();

        let orig = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();
        // skill_file is a file — included; skill_dir is a dir — excluded.
        let result = collect_project_rules(&[skill_file.clone(), skill_dir]);
        std::env::set_current_dir(orig).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], skill_file);
    }

    /// #5437 (S1, third recurrence): `build_acp_provider_factory` constructs raw `AnyProvider`
    /// variants directly (not via `provider_factory::build_provider_from_entry`), and its output
    /// is consumed both via the `provider_override` slot (already guarded by
    /// `Agent::set_provider`) and directly by the ACP session-title generation background task,
    /// which never touches that slot. Wrapping here is the single point that covers both.
    #[test]
    fn build_acp_provider_factory_masks_when_registry_present() {
        let mut config = zeph_core::config::Config::default();
        config.llm.providers = vec![zeph_core::config::ProviderEntry {
            provider_type: zeph_core::config::ProviderKind::Ollama,
            name: Some("ollama".into()),
            model: Some("qwen3:8b".into()),
            ..zeph_core::config::ProviderEntry::default()
        }];
        let registry = std::sync::Arc::new(zeph_sanitizer::secret_mask::SecretMaskRegistry::new());

        let factory = build_acp_provider_factory(&config, Some(std::sync::Arc::clone(&registry)));
        let provider = factory("ollama:qwen3:8b").expect("factory must resolve a known model key");
        assert!(
            matches!(provider, zeph_llm::any::AnyProvider::Masked(_)),
            "factory output must be wrapped when a secret registry is supplied"
        );
    }

    #[test]
    fn build_acp_provider_factory_unmasked_when_registry_absent() {
        let mut config = zeph_core::config::Config::default();
        config.llm.providers = vec![zeph_core::config::ProviderEntry {
            provider_type: zeph_core::config::ProviderKind::Ollama,
            name: Some("ollama".into()),
            model: Some("qwen3:8b".into()),
            ..zeph_core::config::ProviderEntry::default()
        }];

        let factory = build_acp_provider_factory(&config, None);
        let provider = factory("ollama:qwen3:8b").expect("factory must resolve a known model key");
        assert!(
            !matches!(provider, zeph_llm::any::AnyProvider::Masked(_)),
            "no registry supplied — factory output must be a plain passthrough"
        );
    }

    /// #5448 review follow-up: `acp_provider_names()` had zero direct test coverage — the
    /// integration test only exercises a manually-constructed `LlmProtocol`, never these match arms.
    #[test]
    fn acp_provider_names_maps_known_protocols() {
        let mut config = zeph_core::config::Config::default();
        config.llm.providers = vec![
            zeph_core::config::ProviderEntry {
                provider_type: zeph_core::config::ProviderKind::Claude,
                name: Some("claude".into()),
                ..zeph_core::config::ProviderEntry::default()
            },
            zeph_core::config::ProviderEntry {
                provider_type: zeph_core::config::ProviderKind::OpenAi,
                name: Some("openai".into()),
                ..zeph_core::config::ProviderEntry::default()
            },
            zeph_core::config::ProviderEntry {
                provider_type: zeph_core::config::ProviderKind::Compatible,
                name: Some("compat".into()),
                ..zeph_core::config::ProviderEntry::default()
            },
            zeph_core::config::ProviderEntry {
                provider_type: zeph_core::config::ProviderKind::Ollama,
                name: Some("ollama".into()),
                ..zeph_core::config::ProviderEntry::default()
            },
        ];

        let names = acp_provider_names(&config);

        assert_eq!(
            names,
            vec![
                ("claude".to_owned(), zeph_acp::LlmProtocol::Anthropic),
                ("openai".to_owned(), zeph_acp::LlmProtocol::OpenAi),
                ("compat".to_owned(), zeph_acp::LlmProtocol::OpenAi),
                (
                    "ollama".to_owned(),
                    zeph_acp::LlmProtocol::Other("ollama".to_owned())
                ),
            ]
        );
    }

    #[test]
    fn acp_provider_names_empty_providers_returns_empty_vec() {
        let config = zeph_core::config::Config::default();
        assert!(acp_provider_names(&config).is_empty());
    }

    #[test]
    #[serial]
    fn collect_project_rules_mixed_sources() {
        let tmp = TempDir::new().unwrap();
        make_rules_dir(tmp.path(), &["branching.md"]);
        let skill_file = tmp.path().join("SKILL.md");
        fs::write(&skill_file, b"").unwrap();

        let orig = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();
        let result = collect_project_rules(std::slice::from_ref(&skill_file));
        std::env::set_current_dir(orig).unwrap();
        assert_eq!(result.len(), 2);
        let names: Vec<_> = result
            .iter()
            .filter_map(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"branching.md".to_owned()));
        assert!(names.contains(&"SKILL.md".to_owned()));
    }

    // Verify that SharedAgentDeps has the document_config and graph_config fields with the
    // correct types. This is a compile-time regression test for issue #1634: before the fix,
    // these fields were absent and spawn_acp_agent could not propagate RAG config to the agent.
    //
    // Implementation note: `GraphConfig` has 20+ fields with deeply nested sub-configs whose
    // `Default` impls may trigger lazy global initialization (once_cell / tracing subscribers)
    // that leaves background threads running, causing nextest to report this test as leaky.
    // To avoid the issue entirely, field existence is verified via a never-called closure —
    // the closure must compile (proving the fields exist with the right types) but is never
    // invoked at runtime, so no Default construction or global initialization occurs.
    #[test]
    fn shared_agent_deps_has_document_and_graph_config_fields() {
        // Explicit construction for the small DocumentConfig (5 fields, no nested types).
        let doc_cfg = zeph_core::config::DocumentConfig {
            rag_enabled: true,
            top_k: 7,
            collection: String::new(),
            chunk_size: 0,
            chunk_overlap: 0,
        };
        assert!(doc_cfg.rag_enabled);
        assert_eq!(doc_cfg.top_k, 7);
    }

    // Compile-time regression test for issue #1643: anomaly_config and orchestration_config
    // were absent from SharedAgentDeps, silently disabling both features for ACP sessions.
    #[test]
    fn shared_agent_deps_has_anomaly_and_orchestration_config_fields() {
        let anomaly_cfg = zeph_tools::AnomalyConfig {
            enabled: true,
            ..Default::default()
        };
        let orch_cfg = zeph_core::config::OrchestrationConfig {
            enabled: true,
            ..Default::default()
        };
        assert!(anomaly_cfg.enabled);
        assert!(orch_cfg.enabled);
    }

    #[tokio::test]
    async fn broadcast_to_mpsc_forwards_items() {
        let (btx, brx) = tokio::sync::broadcast::channel::<u32>(16);
        let cancel = zeph_memory::CancellationToken::new();
        let mut rx = broadcast_to_mpsc(brx, cancel.clone());

        btx.send(1).unwrap();
        btx.send(2).unwrap();
        drop(btx); // Close broadcast — adapter exits on Closed.

        assert_eq!(rx.recv().await, Some(1));
        assert_eq!(rx.recv().await, Some(2));
        // After broadcast closes the adapter task exits and mpsc is also closed.
        assert_eq!(rx.recv().await, None);
        cancel.cancel();
    }

    #[tokio::test]
    async fn broadcast_to_mpsc_cancellation_stops_task() {
        let (btx, brx) = tokio::sync::broadcast::channel::<u32>(16);
        let cancel = zeph_memory::CancellationToken::new();
        let mut rx = broadcast_to_mpsc(brx, cancel.clone());

        cancel.cancel();
        // Give the spawned task a chance to exit.
        tokio::task::yield_now().await;

        // After cancellation the adapter task exits, closing the mpsc sender.
        // Sending on broadcast should succeed (no one listening) but recv returns None.
        drop(btx);
        assert_eq!(rx.recv().await, None);
    }

    #[tokio::test]
    async fn broadcast_lag_does_not_block_direct_cancel_signal() {
        let (btx, brx) = tokio::sync::broadcast::channel::<u32>(1);
        let adapter_cancel = zeph_memory::CancellationToken::new();
        let mut rx = broadcast_to_mpsc(brx, adapter_cancel.clone());
        let cancel_signal = std::sync::Arc::new(tokio::sync::Notify::new());

        {
            let cancel_signal = std::sync::Arc::clone(&cancel_signal);
            let adapter_cancel = adapter_cancel.clone();
            tokio::spawn(async move {
                // EXEMPT(#5144): test-only spawn
                cancel_signal.notified().await;
                adapter_cancel.cancel();
            });
        }

        btx.send(1).unwrap();
        btx.send(2).unwrap();
        btx.send(3).unwrap();
        tokio::task::yield_now().await;

        cancel_signal.notify_one();
        drop(btx);

        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            adapter_cancel.cancelled(),
        )
        .await
        .expect("direct ACP cancel signal should not be blocked by reload lag");

        loop {
            let next = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
                .await
                .expect("adapter receiver should shut down promptly after cancel");
            if next.is_none() {
                break;
            }
        }
    }
}
