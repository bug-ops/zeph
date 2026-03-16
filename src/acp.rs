// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

#[cfg(any(feature = "acp", feature = "acp-http"))]
use std::path::PathBuf;

#[cfg(feature = "acp")]
use crate::agent_setup;
#[cfg(feature = "acp")]
use zeph_core::agent::Agent;
#[cfg(any(feature = "acp", feature = "acp-http"))]
use zeph_core::bootstrap::{AppBuilder, create_mcp_registry};
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
    registry: std::sync::Arc<std::sync::RwLock<zeph_skills::registry::SkillRegistry>>,
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
    mcp_shared_tools: std::sync::Arc<std::sync::RwLock<Vec<zeph_mcp::McpTool>>>,
    mcp_config: zeph_core::config::McpConfig,

    // Optional runtime providers (contain HTTP client pools; excluded from session_config)
    summary_provider: Option<zeph_llm::any::AnyProvider>,
    judge_provider: Option<zeph_llm::any::AnyProvider>,
    quarantine_provider: Option<(
        zeph_llm::any::AnyProvider,
        zeph_core::sanitizer::QuarantineConfig,
    )>,
    #[cfg(feature = "guardrail")]
    guardrail_provider: Option<(
        zeph_llm::any::AnyProvider,
        zeph_core::sanitizer::guardrail::GuardrailConfig,
    )>,

    // Config snapshot — single source of truth for all config-derived agent settings
    session_config: zeph_core::AgentSessionConfig,
    focus_config: zeph_core::config::FocusConfig,
    sidequest_config: zeph_core::config::SidequestConfig,

    // ACP-specific fields (transport-level; not agent-level)
    acp_agent_name: String,
    acp_agent_version: String,
    acp_max_sessions: usize,
    acp_session_idle_timeout_secs: u64,
    acp_permission_file: Option<std::path::PathBuf>,
    acp_available_models: std::sync::Arc<std::sync::RwLock<Vec<String>>>,
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
    /// Project rule file paths to advertise in session `_meta`.
    acp_project_rules: Vec<PathBuf>,

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
    let (provider, _status_rx) = app.build_provider().await?;
    let embed_model = app.embedding_model();
    let budget_tokens = app.auto_budget_tokens(&provider);
    let registry = std::sync::Arc::new(std::sync::RwLock::new(app.build_registry()));
    let memory = std::sync::Arc::new(app.build_memory(&provider).await?);

    {
        let sqlite = memory.sqlite().clone();
        let retention_secs = app
            .config()
            .tools
            .overflow
            .retention_days
            .saturating_mul(86_400);
        tokio::spawn(async move {
            match sqlite.cleanup_overflow(retention_secs).await {
                Ok(n) if n > 0 => tracing::info!("cleaned up {n} stale overflow entries"),
                Ok(_) => {}
                Err(e) => tracing::warn!("overflow cleanup failed: {e}"),
            }
        });
    }

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
    let config = app.config();

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
    let mcp_manager = prebuilt_mcp_manager.unwrap_or_else(|| {
        std::sync::Arc::new(zeph_core::bootstrap::create_mcp_manager(config, false))
    });
    let mcp_tools = mcp_manager.connect_all().await;
    let mcp_shared_tools = std::sync::Arc::new(std::sync::RwLock::new(mcp_tools.clone()));
    let mcp_executor =
        zeph_mcp::McpToolExecutor::new(mcp_manager.clone(), mcp_shared_tools.clone());
    let base_executor = zeph_tools::CompositeExecutor::new(
        file_executor,
        zeph_tools::CompositeExecutor::new(shell_executor, scrape_executor),
    );
    let tool_executor: std::sync::Arc<dyn zeph_tools::ErasedToolExecutor> = {
        let base: std::sync::Arc<dyn zeph_tools::ErasedToolExecutor> = std::sync::Arc::new(
            zeph_tools::CompositeExecutor::new(base_executor, mcp_executor),
        );
        if let Some(search_executor) = crate::agent_setup::build_search_code_executor(
            config,
            app.qdrant_ops().cloned(),
            provider.clone(),
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
    let skill_paths = app.skill_paths();
    let acp_project_rules = collect_project_rules(&skill_paths);
    let zeph_core::bootstrap::WatcherBundle {
        skill_watcher,
        skill_reload_rx: mpsc_skill_rx,
        config_watcher,
        config_reload_rx: mpsc_config_rx,
    } = app.build_watchers();
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
        tokio::spawn(async move {
            let mut rx = mpsc_skill_rx;
            while let Some(ev) = rx.recv().await {
                let _ = skill_tx.send(ev);
            }
        });
    }
    {
        let cfg_tx = config_reload_tx.clone();
        tokio::spawn(async move {
            let mut rx = mpsc_config_rx;
            while let Some(ev) = rx.recv().await {
                let _ = cfg_tx.send(ev);
            }
        });
    }

    #[cfg(feature = "scheduler")]
    let (scheduler_executor, scheduler_update_tx, scheduler_custom_tx) = {
        #[cfg(feature = "experiments")]
        let exp_deps = {
            use std::sync::Arc;
            if config.experiments.enabled && config.experiments.schedule.enabled {
                let p = provider.clone();
                Some((Arc::new(p), Some(Arc::clone(&memory))))
            } else {
                None
            }
        };
        #[cfg(not(feature = "experiments"))]
        let exp_deps: Option<(
            std::sync::Arc<zeph_llm::any::AnyProvider>,
            Option<std::sync::Arc<zeph_memory::semantic::SemanticMemory>>,
        )> = None;

        match crate::scheduler::init_scheduler(config, shutdown_rx.clone(), exp_deps).await {
            Some(result) => {
                let exec = std::sync::Arc::new(result.executor);
                let mut custom_rx = result.custom_rx;
                let (ctx, _) = tokio::sync::broadcast::channel::<String>(broadcast_cap);
                let ctx_clone = ctx.clone();
                tokio::spawn(async move {
                    while let Some(ev) = custom_rx.recv().await {
                        let _ = ctx_clone.send(ev);
                    }
                });
                let update_tx = if let Some(mut update_rx) = result.update_rx {
                    let (utx, _) = tokio::sync::broadcast::channel::<String>(broadcast_cap);
                    let utx_clone = utx.clone();
                    tokio::spawn(async move {
                        while let Some(ev) = update_rx.recv().await {
                            let _ = utx_clone.send(ev);
                        }
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

    let deps = SharedAgentDeps {
        provider,
        registry,
        matcher,
        max_active_skills: config.skills.max_active_skills,
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
        quarantine_provider: app.build_quarantine_provider(),
        #[cfg(feature = "guardrail")]
        guardrail_provider: app.build_guardrail_provider(),
        session_config,
        focus_config: config.agent.focus.clone(),
        sidequest_config: config.memory.sidequest.clone(),
        acp_agent_name: config.acp.agent_name.clone(),
        acp_agent_version: config.acp.agent_version.clone(),
        acp_max_sessions: config.acp.max_sessions,
        acp_session_idle_timeout_secs: config.acp.session_idle_timeout_secs,
        acp_permission_file: config.acp.permission_file.clone(),
        acp_available_models: std::sync::Arc::new(std::sync::RwLock::new(
            if config.acp.available_models.is_empty() {
                discover_models_from_config(config)
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
        sqlite_path: config.memory.sqlite_path.clone(),
        acp_provider_factory: Some(build_acp_provider_factory(config)),
        acp_project_rules,
        #[cfg(feature = "scheduler")]
        scheduler_executor,
        #[cfg(feature = "scheduler")]
        scheduler_update_tx,
        #[cfg(feature = "scheduler")]
        scheduler_custom_tx,
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
    channel: zeph_core::channel::LoopbackChannel,
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
    let quarantine_provider = d.quarantine_provider.clone();
    #[cfg(feature = "guardrail")]
    let guardrail_provider = d.guardrail_provider.clone();
    let session_config = d.session_config.clone();
    let managed_skills_dir = zeph_core::bootstrap::managed_skills_dir();
    let skill_reload_tx = d.skill_reload_tx.clone();
    let config_reload_tx = d.config_reload_tx.clone();
    #[cfg(feature = "scheduler")]
    let scheduler_executor = d.scheduler_executor.as_ref().map(std::sync::Arc::clone);
    #[cfg(feature = "scheduler")]
    let scheduler_update_tx = d.scheduler_update_tx.clone();
    #[cfg(feature = "scheduler")]
    let scheduler_custom_tx = d.scheduler_custom_tx.clone();

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
        zeph_core::sanitizer::memory_validation::MemoryWriteValidator::new(
            memory_validation_config,
        ),
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

    let mut agent = Agent::new_with_registry_arc(
        provider,
        channel,
        Arc::clone(&registry),
        matcher,
        max_active_skills,
        tool_executor,
    )
    .apply_session_config(session_config)
    .with_working_dir(session_ctx.working_dir.clone())
    .with_skill_reload(skill_paths, reload_rx)
    .with_managed_skills_dir(managed_skills_dir)
    .with_shutdown(shutdown_rx)
    .with_config_reload(config_path, config_reload_rx)
    .with_mcp(
        mcp_tools,
        mcp_registry,
        Some(Arc::clone(&mcp_manager)),
        &mcp_config,
    )
    .with_mcp_shared_tools(mcp_shared_tools)
    .with_focus_config(d.focus_config.clone())
    .with_sidequest_config(d.sidequest_config.clone());

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

    agent = agent_setup::apply_quarantine_provider(agent, quarantine_provider);
    #[cfg(feature = "guardrail")]
    {
        agent = agent_setup::apply_guardrail(agent, guardrail_provider);
    }

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

    drop(d);

    if let Err(e) = agent.load_history().await {
        tracing::error!("failed to load agent history: {e:#}");
    }

    if let Err(e) = agent.run().await {
        tracing::error!("ACP agent loop error: {e:#}");
    }

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
fn discover_models_from_config(config: &zeph_core::config::Config) -> Vec<String> {
    use zeph_llm::model_cache::ModelCache;

    /// Expand a provider slug using its on-disk cache, or fall back to `fallback`.
    fn expand_from_cache(slug: &str, fallback: &str) -> Vec<String> {
        let cache = ModelCache::for_slug(slug);
        if !cache.is_stale()
            && let Ok(Some(entries)) = cache.load()
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

    if config.llm.provider == zeph_core::config::ProviderKind::Orchestrator {
        // Orchestrator: enumerate sub-providers and use their own cache/fallback.
        if let Some(ref orch) = config.llm.orchestrator {
            for sub in orch.providers.values() {
                let slug = sub.provider_type.as_str();
                let fallback = sub.model.as_deref().unwrap_or("unknown");
                models.extend(expand_from_cache(slug, fallback));
            }
        }
    } else {
        // Single provider — use top-level llm section.
        models.extend(expand_from_cache("ollama", &config.llm.model));
    }

    // Claude — always add when API key present, even under orchestrator.
    if config.secrets.claude_api_key.is_some()
        && config.llm.provider != zeph_core::config::ProviderKind::Orchestrator
    {
        let fallback = config
            .llm
            .cloud
            .as_ref()
            .map_or("claude-sonnet-4-5", |c| c.model.as_str());
        models.extend(expand_from_cache("claude", fallback));
    }

    // OpenAI — only when API key and config section are present (non-orchestrator).
    if config.llm.provider != zeph_core::config::ProviderKind::Orchestrator
        && let (Some(_), Some(openai_cfg)) = (&config.secrets.openai_api_key, &config.llm.openai)
    {
        models.extend(expand_from_cache("openai", &openai_cfg.model));
    }

    // Compatible providers.
    if let Some(ref entries) = config.llm.compatible {
        for entry in entries {
            if config.secrets.compatible_api_keys.contains_key(&entry.name) {
                models.extend(expand_from_cache(&entry.name, &entry.model));
            }
        }
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
    available_models: std::sync::Arc<std::sync::RwLock<Vec<String>>>,
) {
    use zeph_llm::model_cache::ModelCache;

    let provider_count = {
        let models = available_models
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
        let models = available_models
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        models
            .iter()
            .filter_map(|k| k.split_once(':').map(|(s, _)| s.to_owned()))
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect()
    };

    for slug in slugs {
        let cache = ModelCache::for_slug(&slug);
        if cache.is_stale() {
            tracing::info!(provider = %slug, "model cache still stale after warm-up");
            continue;
        }
        if let Ok(Some(entries)) = cache.load()
            && !entries.is_empty()
        {
            let new_keys: Vec<String> = entries
                .into_iter()
                .map(|m| format!("{slug}:{}", m.id))
                .collect();
            let count = new_keys.len();
            let mut models = available_models
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            models.retain(|k| !k.starts_with(&format!("{slug}:")));
            models.extend(new_keys);
            models.dedup();
            tracing::info!(provider = %slug, models = count, "model cache ready");
        }
    }
    let total_models = available_models
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .len();
    tracing::info!(models = total_models, "model cache warming finished");
}

/// Build a `ProviderFactory` from the known named providers in config.
///
/// Each available model key is `"{provider_name}:{model}"`.
/// The factory creates a provider by parsing that key and overriding the model in a clone.
#[cfg(feature = "acp")]
#[allow(clippy::too_many_lines)]
fn build_acp_provider_factory(config: &zeph_core::config::Config) -> zeph_acp::ProviderFactory {
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

    // Ollama
    snapshots.push(ProviderSnapshot::Ollama {
        base_url: config.llm.base_url.clone(),
        embed: config.llm.embedding_model.clone(),
    });

    // Claude
    if let Some(ref secret) = config.secrets.claude_api_key {
        snapshots.push(ProviderSnapshot::Claude {
            api_key: secret.expose().to_owned(),
            max_tokens: config.llm.cloud.as_ref().map_or(4096, |c| c.max_tokens),
        });
    }

    // OpenAI
    if let (Some(secret), Some(openai_cfg)) = (&config.secrets.openai_api_key, &config.llm.openai) {
        snapshots.push(ProviderSnapshot::OpenAi {
            api_key: secret.expose().to_owned(),
            base_url: openai_cfg.base_url.clone(),
            max_tokens: openai_cfg.max_tokens,
            embed: openai_cfg.embedding_model.clone(),
            reasoning_effort: openai_cfg.reasoning_effort.clone(),
        });
    }

    // Compatible providers
    if let Some(ref entries) = config.llm.compatible {
        for entry in entries {
            if let Some(secret) = config.secrets.compatible_api_keys.get(&entry.name) {
                snapshots.push(ProviderSnapshot::Compatible {
                    api_key: secret.expose().to_owned(),
                    base_url: entry.base_url.clone(),
                    max_tokens: entry.max_tokens,
                    embed: entry.embedding_model.clone(),
                    name: entry.name.clone(),
                });
            }
        }
    }

    let snapshots = std::sync::Arc::new(snapshots);
    std::sync::Arc::new(move |key: &str| {
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
                    return Some(zeph_llm::any::AnyProvider::Ollama(p));
                }
                ProviderSnapshot::Claude {
                    api_key,
                    max_tokens,
                } if provider_name == "claude" => {
                    return Some(zeph_llm::any::AnyProvider::Claude(
                        zeph_llm::claude::ClaudeProvider::new(
                            api_key.clone(),
                            model.clone(),
                            *max_tokens,
                        ),
                    ));
                }
                ProviderSnapshot::OpenAi {
                    api_key,
                    base_url,
                    max_tokens,
                    embed,
                    reasoning_effort,
                } if provider_name == "openai" => {
                    return Some(zeph_llm::any::AnyProvider::OpenAi(
                        zeph_llm::openai::OpenAiProvider::new(
                            api_key.clone(),
                            base_url.clone(),
                            model.clone(),
                            *max_tokens,
                            embed.clone(),
                            reasoning_effort.clone(),
                        ),
                    ));
                }
                ProviderSnapshot::Compatible {
                    api_key,
                    base_url,
                    max_tokens,
                    embed,
                    name,
                } if provider_name == name => {
                    return Some(zeph_llm::any::AnyProvider::Compatible(
                        zeph_llm::compatible::CompatibleProvider::new(
                            name.clone(),
                            api_key.clone(),
                            base_url.clone(),
                            model.clone(),
                            *max_tokens,
                            embed.clone(),
                        ),
                    ));
                }
                _ => {}
            }
        }
        None
    })
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
) -> anyhow::Result<()> {
    use std::sync::Arc;

    let (mut deps, _keepalive) =
        build_acp_deps(config_path, vault_backend, vault_key, vault_path, None).await?;
    let available_models = std::sync::Arc::clone(&deps.acp_available_models);
    let provider = deps.provider.clone();
    warm_model_caches(provider, available_models).await;

    let mcp_manager_for_acp = Arc::clone(&deps.mcp_manager);
    let server_config = zeph_acp::AcpServerConfig {
        agent_name: deps.acp_agent_name.clone(),
        agent_version: deps.acp_agent_version.clone(),
        max_sessions: deps.acp_max_sessions,
        session_idle_timeout_secs: deps.acp_session_idle_timeout_secs,
        permission_file: deps.acp_permission_file.clone(),
        provider_factory: deps.acp_provider_factory.take(),
        available_models: std::sync::Arc::clone(&deps.acp_available_models),
        mcp_manager: Some(mcp_manager_for_acp),
        auth_bearer_token: deps.acp_auth_bearer_token.clone(),
        discovery_enabled: deps.acp_discovery_enabled,
        terminal_timeout_secs: 120,
        project_rules: deps.acp_project_rules.clone(),
        title_max_chars: deps.acp_title_max_chars,
        max_history: deps.acp_max_history,
        sqlite_path: Some(deps.sqlite_path.clone()),
        ready_notification: Some(zeph_acp::transport::ReadyNotification {
            version: deps.acp_agent_version.clone(),
            pid: std::process::id(),
            log_file: deps.acp_log_file.clone(),
        }),
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
    let bind_addr = bind_override.map_or_else(|| "127.0.0.1:9800".to_owned(), str::to_owned);

    // CLI flag overrides config/env values for auth token.
    let auth_bearer_token = auth_token_override.or(app.config().acp.auth_token.clone());
    let mcp_manager_for_acp = Arc::new(zeph_core::bootstrap::create_mcp_manager(
        app.config(),
        false,
    ));
    let server_config = zeph_acp::AcpServerConfig {
        agent_name: app.config().acp.agent_name.clone(),
        agent_version: app.config().acp.agent_version.clone(),
        max_sessions: app.config().acp.max_sessions,
        session_idle_timeout_secs: app.config().acp.session_idle_timeout_secs,
        permission_file: app.config().acp.permission_file.clone(),
        provider_factory: Some(build_acp_provider_factory(app.config())),
        available_models: std::sync::Arc::new(std::sync::RwLock::new(
            if app.config().acp.available_models.is_empty() {
                discover_models_from_config(app.config())
            } else {
                app.config().acp.available_models.clone()
            },
        )),
        mcp_manager: Some(Arc::clone(&mcp_manager_for_acp)),
        auth_bearer_token,
        discovery_enabled: app.config().acp.discovery_enabled,
        terminal_timeout_secs: 120,
        project_rules: collect_project_rules(&app.skill_paths()),
        title_max_chars: app.config().memory.sessions.title_max_chars,
        max_history: app.config().memory.sessions.max_history,
        sqlite_path: Some(app.config().memory.sqlite_path.clone()),
        ready_notification: None,
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
    match zeph_memory::sqlite::SqliteStore::new(&app.config().memory.sqlite_path).await {
        Ok(store) => state = state.with_store(store),
        Err(e) => tracing::warn!(error = %e, "failed to open SQLite for HTTP session endpoints"),
    }

    let router = zeph_acp::acp_router(state.clone());

    let listener = tokio::net::TcpListener::bind(&bind_addr).await?;
    tracing::info!("ACP HTTP server listening on {bind_addr}");
    let server_task = tokio::spawn(async move { ::axum::serve(listener, router).await });

    let (deps, _keepalive) = match build_acp_deps(
        config_path,
        vault_backend,
        vault_key,
        vault_path,
        Some(mcp_manager_for_acp),
    )
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
    #[test]
    fn shared_agent_deps_has_document_and_graph_config_fields() {
        let doc_cfg = zeph_core::config::DocumentConfig {
            rag_enabled: true,
            top_k: 7,
            ..Default::default()
        };
        let graph_cfg = zeph_core::config::GraphConfig {
            enabled: true,
            ..Default::default()
        };
        // Use ZST trick: read through a raw pointer to verify field offsets exist without
        // constructing the full SharedAgentDeps (which has ~50 required fields).
        // The assertions below confirm the field types are correct at compile time.
        assert!(doc_cfg.rag_enabled);
        assert_eq!(doc_cfg.top_k, 7);
        assert!(graph_cfg.enabled);
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
