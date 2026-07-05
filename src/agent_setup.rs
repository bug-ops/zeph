// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use parking_lot::RwLock;

use zeph_core::RuntimeContext;
use zeph_core::channel::Channel;
use zeph_core::config::Config;
use zeph_tools::{
    LspSearchBackend, SearchCodeExecutor, SearchCodeHit, SearchCodeSource, SemanticSearchBackend,
};

/// Adapter that bridges `PolicyLlmClient` to `AnyProvider::chat`.
///
/// Defined in the binary crate to keep `zeph-tools` decoupled from `zeph-llm`. Shared by
/// all three live entry points that construct an `AdversarialPolicyGateExecutor` (CLI's
/// `runner.rs`, `acp.rs`'s `spawn_acp_agent`, `daemon.rs`'s `run_daemon`).
pub(crate) struct AdversarialPolicyLlmAdapter {
    pub(crate) provider: zeph_llm::any::AnyProvider,
}

impl zeph_tools::PolicyLlmClient for AdversarialPolicyLlmAdapter {
    fn chat<'a>(
        &'a self,
        messages: &'a [zeph_tools::PolicyMessage],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'a>>
    {
        Box::pin(async move {
            let llm_messages: Vec<zeph_llm::provider::Message> = messages
                .iter()
                .map(|m| {
                    zeph_llm::provider::Message::from_legacy(
                        match m.role {
                            zeph_tools::PolicyRole::System => zeph_llm::provider::Role::System,
                            _ => zeph_llm::provider::Role::User,
                        },
                        m.content.clone(),
                    )
                })
                .collect();

            let result: Result<String, zeph_llm::LlmError> =
                zeph_llm::provider::LlmProvider::chat(&self.provider, &llm_messages).await;
            result.map_err(|e| e.to_string())
        })
    }
}

pub(crate) struct ToolSetup {
    pub(crate) executor: zeph_tools::DynExecutor,
    /// TACO compressor handle for `maybe_autodream` hit-count flushing. `None` when disabled.
    pub(crate) taco_compressor: Option<std::sync::Arc<zeph_tools::RuleBasedCompressor>>,
    pub(crate) mcp_tools: Vec<zeph_mcp::McpTool>,
    pub(crate) mcp_outcomes: Vec<zeph_mcp::ServerConnectOutcome>,
    pub(crate) mcp_manager: Arc<zeph_mcp::McpManager>,
    pub(crate) mcp_shared_tools: Arc<RwLock<Vec<zeph_mcp::McpTool>>>,
    pub(crate) tool_event_rx: Option<tokio::sync::mpsc::Receiver<zeph_tools::ToolEvent>>,
    /// Watch receiver for MCP tool list updates from `tools/list_changed` notifications.
    pub(crate) mcp_tool_rx: tokio::sync::watch::Receiver<Vec<zeph_mcp::McpTool>>,
    /// Receiver for elicitation requests from MCP server handlers.
    pub(crate) mcp_elicitation_rx: Option<tokio::sync::mpsc::Receiver<zeph_mcp::ElicitationEvent>>,
    /// Audit logger to pass to the agent for pre-execution block recording. `None` when audit is disabled.
    pub(crate) audit_logger: Option<Arc<zeph_tools::AuditLogger>>,
    /// Egress event receiver. `None` when egress logging is disabled.
    pub(crate) egress_rx: Option<tokio::sync::mpsc::Receiver<zeph_tools::EgressEvent>>,
    /// Live-rebuild handle for the `ShellExecutor`'s `blocked_commands` policy.
    pub(crate) shell_policy_handle: zeph_tools::ShellPolicyHandle,
    /// Receiver end of the background-completion channel. Passed to the agent via
    /// `Agent::with_background_completion_rx` so it can drain completions into the next turn.
    pub(crate) background_completion_rx:
        Option<tokio::sync::mpsc::Receiver<zeph_tools::BackgroundCompletion>>,
    /// Shared reference to the `ShellExecutor` for background-run TUI metrics.
    pub(crate) shell_executor_handle: Option<Arc<zeph_tools::ShellExecutor>>,
    /// Per-session risk chain accumulator wired to `ShellExecutor`.
    ///
    /// Pass the same `Arc` to `AgentBuilder::with_risk_chain_accumulator` so
    /// `begin_turn()` resets the per-turn score at each turn boundary.
    pub(crate) risk_chain_accumulator: Arc<zeph_tools::RiskChainAccumulator>,
}

#[derive(Clone)]
struct SemanticCodeSearch {
    store: CodeStore,
    provider: std::sync::Arc<zeph_llm::any::AnyProvider>,
    score_threshold: f32,
}

impl SemanticSearchBackend for SemanticCodeSearch {
    fn search<'a>(
        &'a self,
        query: &'a str,
        file_pattern: Option<&'a str>,
        max_results: usize,
    ) -> Pin<
        Box<
            dyn std::future::Future<Output = Result<Vec<SearchCodeHit>, zeph_tools::ToolError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            use zeph_llm::provider::LlmProvider;

            let matcher = file_pattern
                .map(glob::Pattern::new)
                .transpose()
                .map_err(|e| zeph_tools::ToolError::InvalidParams {
                    message: format!("invalid file_pattern: {e}"),
                })?;
            let raw = self.provider.embed(query).await.map_err(|e| {
                zeph_tools::ToolError::Execution(std::io::Error::other(e.to_string()))
            })?;
            // Normalize the raw embedding before passing to store.search, which requires
            // EmbeddingVector<Normalized>. Embedding providers do not guarantee unit-length
            // output; skipping normalization caused silent near-zero Qdrant cosine scores
            // (#3421).
            let vector =
                zeph_common::EmbeddingVector::<zeph_common::Unnormalized>::new(raw).normalize();
            let mut hits = self
                .store
                .search(vector, max_results.saturating_mul(2), None)
                .await
                .map_err(|e| {
                    zeph_tools::ToolError::Execution(std::io::Error::other(e.to_string()))
                })?;
            hits.retain(|hit| hit.score >= self.score_threshold);

            let mut out = hits
                .into_iter()
                .filter(|hit| {
                    matcher.as_ref().is_none_or(|pattern: &glob::Pattern| {
                        pattern.matches_path(std::path::Path::new(&hit.file_path))
                    })
                })
                .map(|hit| SearchCodeHit {
                    file_path: std::fs::canonicalize(&hit.file_path)
                        .unwrap_or_else(|_| PathBuf::from(&hit.file_path))
                        .display()
                        .to_string(),
                    line_start: hit.line_range.0,
                    line_end: hit.line_range.1,
                    snippet: hit
                        .code
                        .lines()
                        .next()
                        .unwrap_or_default()
                        .trim()
                        .to_string(),
                    source: SearchCodeSource::Semantic,
                    score: hit.score,
                    symbol_name: hit.entity_name,
                })
                .collect::<Vec<_>>();
            out.truncate(max_results);
            Ok(out)
        })
    }
}

#[derive(Clone)]
struct McpCodeSearch {
    manager: Arc<zeph_mcp::McpManager>,
    server_id: String,
}

#[derive(serde::Deserialize)]
struct LspPosition {
    line: u32,
    character: u32,
}

#[derive(serde::Deserialize)]
struct LspRange {
    start: LspPosition,
    end: LspPosition,
}

#[derive(serde::Deserialize)]
struct LspLocation {
    uri: String,
    range: LspRange,
}

#[derive(serde::Deserialize)]
struct LspSymbolInformation {
    name: String,
    location: LspLocation,
}

impl LspSearchBackend for McpCodeSearch {
    fn workspace_symbol<'a>(
        &'a self,
        symbol: &'a str,
        file_pattern: Option<&'a str>,
        max_results: usize,
    ) -> Pin<
        Box<
            dyn std::future::Future<Output = Result<Vec<SearchCodeHit>, zeph_tools::ToolError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let matcher = file_pattern
                .map(glob::Pattern::new)
                .transpose()
                .map_err(|e| zeph_tools::ToolError::InvalidParams {
                    message: format!("invalid file_pattern: {e}"),
                })?;
            let args = serde_json::json!({ "query": symbol });
            let value = mcp_text_json(
                &self.manager,
                &self.server_id,
                "workspace_symbol_search",
                args,
            )
            .await?;
            let mut symbols: Vec<LspSymbolInformation> =
                serde_json::from_value(value).map_err(|e| {
                    zeph_tools::ToolError::Execution(std::io::Error::other(e.to_string()))
                })?;
            symbols.truncate(max_results);
            Ok(symbols
                .into_iter()
                .filter(|item| {
                    matcher.as_ref().is_none_or(|pattern: &glob::Pattern| {
                        pattern.matches_path(std::path::Path::new(&uri_to_path(&item.location.uri)))
                    })
                })
                .map(|item| SearchCodeHit {
                    file_path: uri_to_path(&item.location.uri),
                    line_start: item.location.range.start.line as usize,
                    line_end: item.location.range.end.line as usize,
                    snippet: format!(
                        "{} at {}:{}",
                        item.name,
                        item.location.range.start.line,
                        item.location.range.start.character
                    ),
                    source: SearchCodeSource::LspSymbol,
                    score: SearchCodeSource::LspSymbol.default_score(),
                    symbol_name: Some(item.name),
                })
                .collect())
        })
    }

    fn references<'a>(
        &'a self,
        symbol: &'a str,
        file_pattern: Option<&'a str>,
        max_results: usize,
    ) -> Pin<
        Box<
            dyn std::future::Future<Output = Result<Vec<SearchCodeHit>, zeph_tools::ToolError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let value = mcp_text_json(
                &self.manager,
                &self.server_id,
                "workspace_symbol_search",
                serde_json::json!({ "query": symbol }),
            )
            .await?;
            let defs: Vec<LspSymbolInformation> = serde_json::from_value(value).map_err(|e| {
                zeph_tools::ToolError::Execution(std::io::Error::other(e.to_string()))
            })?;
            let Some(def) = defs.first() else {
                return Ok(vec![]);
            };
            let matcher = file_pattern
                .map(glob::Pattern::new)
                .transpose()
                .map_err(|e| zeph_tools::ToolError::InvalidParams {
                    message: format!("invalid file_pattern: {e}"),
                })?;
            let args = serde_json::json!({
                "file_path": uri_to_path(&def.location.uri),
                "line": def.location.range.start.line,
                "character": def.location.range.start.character,
                "include_declaration": false,
            });
            let value =
                mcp_text_json(&self.manager, &self.server_id, "get_references", args).await?;
            let mut refs: Vec<LspLocation> = serde_json::from_value(value).map_err(|e| {
                zeph_tools::ToolError::Execution(std::io::Error::other(e.to_string()))
            })?;
            refs.truncate(max_results);
            Ok(refs
                .into_iter()
                .filter(|location| {
                    matcher.as_ref().is_none_or(|pattern: &glob::Pattern| {
                        pattern.matches_path(std::path::Path::new(&uri_to_path(&location.uri)))
                    })
                })
                .map(|location| SearchCodeHit {
                    file_path: uri_to_path(&location.uri),
                    line_start: location.range.start.line as usize,
                    line_end: location.range.end.line as usize,
                    snippet: format!(
                        "reference at {}:{}",
                        location.range.start.line, location.range.start.character
                    ),
                    source: SearchCodeSource::LspReferences,
                    score: SearchCodeSource::LspReferences.default_score(),
                    symbol_name: Some(symbol.to_owned()),
                })
                .collect())
        })
    }
}

async fn mcp_text_json(
    manager: &Arc<zeph_mcp::McpManager>,
    server_id: &str,
    tool_name: &str,
    args: serde_json::Value,
) -> Result<serde_json::Value, zeph_tools::ToolError> {
    let result = manager
        .call_tool(server_id, tool_name, args)
        .await
        .map_err(|e| zeph_tools::ToolError::Execution(std::io::Error::other(e.to_string())))?;
    let text = result
        .content
        .iter()
        .find_map(|content| content.as_text().map(|t| t.text.as_str()))
        .ok_or_else(|| {
            zeph_tools::ToolError::Execution(std::io::Error::other(
                "mcpls returned no text content",
            ))
        })?;
    serde_json::from_str(text)
        .map_err(|e| zeph_tools::ToolError::Execution(std::io::Error::other(e.to_string())))
}

fn uri_to_path(uri: &str) -> String {
    url::Url::parse(uri)
        .ok()
        .and_then(|url| url.to_file_path().ok())
        .unwrap_or_else(|| PathBuf::from(uri))
        .display()
        .to_string()
}

/// Drains egress events from the bounded channel, updates metrics, and traces each hop.
///
/// Spawned as a background task per session when `tools.egress.enabled = true`.
/// Exits when the sender side is dropped (session ends).
pub(crate) async fn drain_egress_events(
    mut rx: tokio::sync::mpsc::Receiver<zeph_tools::EgressEvent>,
    metrics_tx: Option<tokio::sync::watch::Sender<zeph_core::metrics::MetricsSnapshot>>,
) {
    while let Some(ev) = rx.recv().await {
        if let Some(ref tx) = metrics_tx {
            tx.send_modify(|m| {
                m.egress_requests_total += 1;
                if ev.blocked {
                    m.egress_blocked_total += 1;
                }
            });
        }
        if ev.blocked {
            tracing::debug!(
                url = %ev.url,
                host = %ev.host,
                tool = %ev.tool,
                block_reason = ?ev.block_reason,
                correlation_id = %ev.correlation_id,
                "egress blocked"
            );
        } else {
            tracing::trace!(
                url = %ev.url,
                host = %ev.host,
                tool = %ev.tool,
                status = ?ev.status,
                duration_ms = ev.duration_ms,
                correlation_id = %ev.correlation_id,
                "egress request"
            );
        }
    }
}

async fn drain_embedding_guard_events(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<zeph_mcp::EmbeddingGuardEvent>,
) {
    while let Some(event) = rx.recv().await {
        match &event.result {
            zeph_mcp::EmbeddingGuardResult::Anomalous {
                distance,
                threshold,
            } => {
                tracing::warn!(
                    server_id = event.server_id,
                    tool_name = %event.tool_name,
                    distance,
                    threshold,
                    "embedding anomaly detected in MCP tool output"
                );
            }
            zeph_mcp::EmbeddingGuardResult::RegexFallback {
                injection_detected: true,
            } => {
                tracing::warn!(
                    server_id = event.server_id,
                    tool_name = %event.tool_name,
                    "regex injection detected in MCP tool output (cold-start fallback)"
                );
            }
            _ => {}
        }
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(crate) async fn build_tool_setup(
    config: &Config,
    permission_policy: zeph_tools::PermissionPolicy,
    with_tool_events: bool,
    bare: bool,
    runtime_ctx: RuntimeContext,
    age_vault: Option<&Arc<std::sync::RwLock<zeph_core::vault::AgeVaultProvider>>>,
    status_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    pool: Option<&zeph_db::DbPool>,
    provider: &zeph_llm::any::AnyProvider,
    supervisor: Option<&zeph_common::TaskSupervisor>,
) -> ToolSetup {
    let filter_registry = if config.tools.filters.enabled {
        zeph_tools::OutputFilterRegistry::default_filters(&config.tools.filters)
    } else {
        zeph_tools::OutputFilterRegistry::new(false)
    };
    let mut shell_executor = zeph_tools::ShellExecutor::new(&config.tools.shell)
        .with_permissions(permission_policy)
        .with_output_filters(filter_registry);
    if let Some(sup) = supervisor {
        shell_executor = shell_executor.with_task_supervisor(sup.clone());
    }
    if config.tools.sandbox.enabled {
        let denied_present = !config.tools.sandbox.denied_domains.is_empty();
        let _span = tracing::info_span!(
            "tools.sandbox.denied_domains_check",
            denied_count = config.tools.sandbox.denied_domains.len(),
            fail_if_unavailable = config.tools.sandbox.fail_if_unavailable,
        )
        .entered();
        match zeph_tools::sandbox::build_sandbox_with_policy(
            config.tools.sandbox.strict,
            config.tools.sandbox.fail_if_unavailable,
            denied_present,
        ) {
            Ok(backend) => {
                let name = backend.name();
                let policy = sandbox_policy_from_config(&config.tools.sandbox);
                shell_executor = shell_executor.with_sandbox(std::sync::Arc::from(backend), policy);
                tracing::info!(backend = name, "OS sandbox enabled");
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
    let mut egress_rx: Option<tokio::sync::mpsc::Receiver<zeph_tools::EgressEvent>> = None;
    if config.tools.egress.enabled {
        let (egress_tx, rx) = tokio::sync::mpsc::channel(256);
        let dropped = Arc::new(std::sync::atomic::AtomicU64::new(0));
        scrape_executor = scrape_executor.with_egress_tx(egress_tx, Arc::clone(&dropped));
        egress_rx = Some(rx);
    }
    let mut audit_logger: Option<Arc<zeph_tools::AuditLogger>> = None;
    if config.tools.audit.enabled
        && let Ok(logger) =
            zeph_tools::AuditLogger::from_config(&config.tools.audit, runtime_ctx.tui_mode).await
    {
        let logger = Arc::new(logger);
        shell_executor = shell_executor.with_audit(Arc::clone(&logger));
        scrape_executor = scrape_executor.with_audit(Arc::clone(&logger));
        audit_logger = Some(logger);
    }
    if config.tools.audit.tool_risk_summary {
        zeph_tools::log_tool_risk_summary(&[
            "shell",
            "web_scrape",
            "fetch",
            "file_read",
            "file_write",
        ]);
    }

    let tool_event_rx = if with_tool_events {
        let (tool_tx, tool_rx) =
            tokio::sync::mpsc::channel::<zeph_tools::ToolEvent>(zeph_tools::TOOL_EVENT_CHANNEL_CAP);
        shell_executor = shell_executor.with_tool_event_tx(tool_tx);
        Some(tool_rx)
    } else {
        None
    };

    // Background-completion channel: the agent drains this at turn start and injects
    // deferred completions into the message history as a single user-role block (N1).
    let (bg_completion_tx, bg_completion_rx) = tokio::sync::mpsc::channel::<
        zeph_tools::BackgroundCompletion,
    >(config.tools.shell.max_background_runs * 2);
    shell_executor = shell_executor.with_background_completion_tx(bg_completion_tx);

    let file_executor = zeph_tools::FileExecutor::new(
        config
            .tools
            .shell
            .allowed_paths
            .iter()
            .map(PathBuf::from)
            .collect(),
    );

    let mut mcp_manager_builder = crate::bootstrap::create_mcp_manager_with_vault(
        config,
        runtime_ctx.suppress_stderr(),
        age_vault,
    );
    if let Some(tx) = status_tx {
        mcp_manager_builder = mcp_manager_builder.with_status_tx(tx);
    }
    mcp_manager_builder =
        crate::bootstrap::wire_trust_calibration(mcp_manager_builder, config, pool).await;
    if config.security.content_isolation.embedding_guard.enabled {
        let guard_config = &config.security.content_isolation.embedding_guard;
        let embed_fn = Arc::new(provider.embed_fn());
        let (guard, rx) = zeph_mcp::EmbeddingAnomalyGuard::new(
            embed_fn,
            guard_config.threshold,
            guard_config.min_samples,
            guard_config.ema_floor,
        );
        mcp_manager_builder = mcp_manager_builder.with_embedding_guard(guard);
        if let Some(sup) = supervisor {
            let fut = drain_embedding_guard_events(rx);
            let cell = std::sync::Arc::new(parking_lot::Mutex::new(Some(fut)));
            sup.spawn(zeph_common::TaskDescriptor {
                name: "embed_guard_drain",
                restart: zeph_common::RestartPolicy::RunOnce,
                factory: move || {
                    let f = cell.lock().take();
                    async move {
                        if let Some(f) = f {
                            f.await;
                        }
                    }
                },
            });
        } else {
            tokio::spawn(drain_embedding_guard_events(rx)); // EXEMPT(#5143): supervisor not available at this call site (acp.rs passes None)
        }
    }
    let mcp_manager = Arc::new(mcp_manager_builder);
    let (mcp_tools, mcp_outcomes) = if bare {
        (Vec::new(), Vec::new())
    } else {
        let result = mcp_manager.connect_all().await;
        tracing::info!("discovered {} MCP tool(s)", result.0.len());
        result
    };

    // Subscribe before spawning the refresh task so no events are missed.
    let mcp_tool_rx = mcp_manager.subscribe_tool_changes();
    // Take the elicitation receiver before Arc-wrapping the manager.
    let mcp_elicitation_rx = mcp_manager.take_elicitation_rx();
    if !bare {
        // Spawn the background task that processes tools/list_changed events.
        mcp_manager.spawn_refresh_task(supervisor);
    }

    let mcp_shared_tools = Arc::new(RwLock::new(mcp_tools.clone()));
    let mcp_executor =
        zeph_mcp::McpToolExecutor::new(mcp_manager.clone(), mcp_shared_tools.clone());
    let risk_chain_accumulator = Arc::new(zeph_tools::RiskChainAccumulator::new(None));
    shell_executor = shell_executor.with_risk_chain(Arc::clone(&risk_chain_accumulator));
    tracing::info!("security.risk_chain: RiskChainAccumulator wired to ShellExecutor");
    let shell_policy_handle = shell_executor.policy_handle();
    let shell_executor = Arc::new(shell_executor);
    let shell_executor_handle = Some(Arc::clone(&shell_executor));
    let diagnostics_executor = build_diagnostics_executor(config);
    let base_executor = build_base_executor_chain(
        file_executor,
        shell_executor,
        scrape_executor,
        diagnostics_executor,
    );
    let composite = zeph_tools::CompositeExecutor::new(base_executor, mcp_executor);
    let (executor, taco_compressor) =
        build_compressed_executor(composite, &config.tools.compression, pool).await;

    ToolSetup {
        executor,
        taco_compressor,
        mcp_tools,
        mcp_outcomes,
        mcp_manager,
        mcp_shared_tools,
        tool_event_rx,
        mcp_tool_rx,
        mcp_elicitation_rx,
        audit_logger,
        egress_rx,
        shell_policy_handle,
        background_completion_rx: Some(bg_completion_rx),
        shell_executor_handle,
        risk_chain_accumulator,
    }
}

/// Wrap `inner` in a [`zeph_tools::CompressedExecutor`] when `cfg.enabled = true` and a DB pool
/// is available. Returns the executor and a compressor handle for hit-count flushing during
/// `maybe_autodream`. Falls back to a plain [`zeph_tools::DynExecutor`] when disabled.
async fn build_compressed_executor<
    E: zeph_tools::ToolExecutor + zeph_tools::ErasedToolExecutor + 'static,
>(
    inner: E,
    cfg: &zeph_config::ToolCompressionConfig,
    pool: Option<&zeph_db::DbPool>,
) -> (
    zeph_tools::DynExecutor,
    Option<Arc<zeph_tools::RuleBasedCompressor>>,
) {
    if cfg.enabled {
        if let Some(pool) = pool {
            let store = Arc::new(zeph_tools::CompressionRuleStore::new(Arc::new(
                pool.clone(),
            )));
            match zeph_tools::RuleBasedCompressor::load(
                store,
                cfg.min_lines_to_compress,
                cfg.regex_compile_timeout_ms,
            )
            .await
            {
                Ok(compressor) => {
                    tracing::info!("tools.compression: TACO enabled, rule-based compressor loaded");
                    let compressor = Arc::new(compressor);
                    let compressed = zeph_tools::CompressedExecutor::new(
                        inner,
                        Arc::clone(&compressor) as Arc<dyn zeph_tools::OutputCompressor>,
                        cfg.min_lines_to_compress,
                    );
                    return (
                        zeph_tools::DynExecutor(Arc::new(compressed)),
                        Some(compressor),
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "tools.compression: failed to load rules, running without compression"
                    );
                }
            }
        } else {
            tracing::warn!("tools.compression: enabled but no DB pool available, skipping");
        }
    }
    (zeph_tools::DynExecutor(Arc::new(inner)), None)
}

use zeph_core::agent::Agent;
use zeph_core::config::IndexConfig;
use zeph_core::cost::CostTracker;
use zeph_index::{
    indexer::{CodeIndexer, IndexerConfig},
    store::CodeStore,
    watcher::IndexWatcher,
};
use zeph_memory::QdrantOps;

pub(crate) type CodeIndexerSetup = (
    Option<IndexWatcher>,
    Option<tokio::sync::watch::Receiver<zeph_index::IndexProgress>>,
);

pub(crate) fn spawn_ctrl_c_handler(
    cancel_signal: std::sync::Arc<tokio::sync::Notify>,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
    supervisor: Option<&zeph_common::TaskSupervisor>,
) {
    let fut = async move {
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
    };
    if let Some(sup) = supervisor {
        let cell = std::sync::Arc::new(parking_lot::Mutex::new(Some(fut)));
        sup.spawn(zeph_common::TaskDescriptor {
            name: "ctrl_c_handler",
            restart: zeph_common::RestartPolicy::RunOnce,
            factory: move || {
                let f = cell.lock().take();
                async move {
                    if let Some(f) = f {
                        f.await;
                    }
                }
            },
        });
    } else {
        tokio::spawn(fut); // EXEMPT(#5143): supervisor not available at this call site (standalone usage)
    }
}

pub(crate) fn apply_response_cache<C: Channel>(
    agent: Agent<C>,
    enabled: bool,
    pool: zeph_db::DbPool,
    ttl_secs: u64,
    semantic_cache_enabled: bool,
    embed_model: String,
    cancel: tokio_util::sync::CancellationToken,
) -> (Agent<C>, Option<tokio::task::JoinHandle<()>>) {
    if !enabled {
        if semantic_cache_enabled {
            tracing::warn!("semantic_cache_enabled has no effect without response_cache_enabled");
        }
        return (agent, None);
    }
    let cache = std::sync::Arc::new(zeph_memory::ResponseCache::new(pool, ttl_secs));
    let cache_clone = std::sync::Arc::clone(&cache);
    let handle = tokio::spawn(async move {
        // EXEMPT(#5143): returns JoinHandle used by caller (runner.rs:3561 cache_cleanup_handle.abort())
        let mut interval = tokio::time::interval(std::time::Duration::from_hours(1));
        interval.tick().await; // skip immediate first tick
        loop {
            tokio::select! {
                () = cancel.cancelled() => {
                    tracing::debug!("response cache cleanup loop: shutting down");
                    break;
                }
                _ = interval.tick() => {
                    match cache_clone.cleanup(&embed_model).await {
                        Ok(n) if n > 0 => tracing::debug!("cleaned up {n} cache entries"),
                        Ok(_) => {}
                        Err(e) => tracing::warn!("response cache cleanup failed: {e:#}"),
                    }
                }
            }
        }
    });
    (agent.with_response_cache(cache), Some(handle))
}

pub(crate) fn apply_cost_tracker<C: Channel>(
    agent: Agent<C>,
    config: &zeph_core::config::Config,
) -> Agent<C> {
    if !config.cost.enabled {
        return agent;
    }
    let mut tracker = CostTracker::new(true, f64::from(config.cost.max_daily_cents));
    for entry in &config.llm.providers {
        if entry.provider_type == zeph_config::ProviderKind::Cocoon
            && let (Some(pricing), Some(model)) = (&entry.cocoon_pricing, &entry.model)
        {
            tracker = tracker.with_pricing(
                model,
                zeph_core::cost::ModelPricing {
                    prompt_cents_per_1k: pricing.prompt_cents_per_1k,
                    completion_cents_per_1k: pricing.completion_cents_per_1k,
                    // Cocoon sidecar does not charge separately for cached tokens
                    cache_read_cents_per_1k: 0.0,
                    cache_write_cents_per_1k: 0.0,
                },
            );
        }
    }
    agent.with_cost_tracker(tracker)
}

pub(crate) fn apply_summary_provider<C: Channel>(
    agent: Agent<C>,
    summary_provider: Option<zeph_llm::any::AnyProvider>,
) -> Agent<C> {
    if let Some(sp) = summary_provider {
        agent.with_summary_provider(sp)
    } else {
        agent
    }
}

pub(crate) fn apply_quarantine_provider<C: Channel>(
    agent: Agent<C>,
    quarantine: Option<(zeph_llm::any::AnyProvider, zeph_sanitizer::QuarantineConfig)>,
) -> Agent<C> {
    if let Some((provider, config)) = quarantine {
        let qs = zeph_sanitizer::quarantine::QuarantinedSummarizer::new(provider, &config);
        agent.with_quarantine_summarizer(qs)
    } else {
        agent
    }
}
pub(crate) fn apply_guardrail<C: Channel>(
    agent: Agent<C>,
    guardrail: Option<(
        zeph_llm::any::AnyProvider,
        zeph_sanitizer::guardrail::GuardrailConfig,
    )>,
) -> Agent<C> {
    if let Some((provider, config)) = guardrail {
        match zeph_sanitizer::guardrail::GuardrailFilter::new(provider, &config) {
            Ok(filter) => agent.with_guardrail(filter),
            Err(e) => {
                tracing::warn!(error = %e, "guardrail filter construction failed, guardrail disabled");
                agent
            }
        }
    } else {
        agent
    }
}

/// Wire the `CandleClassifier` injection backend into the agent's sanitizer.
///
/// Only active when `classifiers.enabled = true` in config.
#[cfg(feature = "classifiers")]
pub(crate) fn apply_injection_classifier<C: Channel>(
    agent: zeph_core::agent::Agent<C>,
    config: &Config,
) -> zeph_core::agent::Agent<C> {
    apply_injection_classifier_with_cfg(agent, &config.classifiers)
}

/// Wire the `CandleClassifier` injection backend into the agent's sanitizer (takes `ClassifiersConfig` directly).
#[cfg(feature = "classifiers")]
pub(crate) fn apply_injection_classifier_with_cfg<C: Channel>(
    agent: zeph_core::agent::Agent<C>,
    classifiers: &zeph_core::config::ClassifiersConfig,
) -> zeph_core::agent::Agent<C> {
    if !classifiers.enabled {
        return agent;
    }
    let mut classifier =
        zeph_llm::classifier::candle::CandleClassifier::new(classifiers.injection_model.as_str());
    if let Some(hash) = &classifiers.injection_model_sha256 {
        classifier = classifier.with_sha256(hash.as_str());
    }
    if let Some(token) = &classifiers.hf_token {
        classifier = classifier.with_hf_token(token.as_str());
    }
    let backend = std::sync::Arc::new(classifier);
    tracing::info!(
        repo_id = %classifiers.injection_model,
        scan_user_input = classifiers.scan_user_input,
        "ML injection classifier attached (model loads lazily on first use)"
    );
    agent
        .with_injection_classifier(
            backend,
            classifiers.timeout_ms,
            classifiers.injection_threshold,
            classifiers.injection_threshold_soft,
        )
        .with_scan_user_input(classifiers.scan_user_input)
}

/// Wire the `CandlePiiClassifier` NER backend into the agent's sanitizer.
///
/// Only active when `classifiers.enabled = true` and `classifiers.pii_enabled = true`.
#[cfg(feature = "classifiers")]
pub(crate) fn apply_pii_classifier<C: Channel>(
    agent: zeph_core::agent::Agent<C>,
    config: &Config,
) -> zeph_core::agent::Agent<C> {
    apply_pii_classifier_with_cfg(agent, &config.classifiers)
}

/// Wire the `CandlePiiClassifier` NER backend into the agent's sanitizer (takes `ClassifiersConfig` directly).
#[cfg(feature = "classifiers")]
pub(crate) fn apply_pii_classifier_with_cfg<C: Channel>(
    agent: zeph_core::agent::Agent<C>,
    classifiers: &zeph_core::config::ClassifiersConfig,
) -> zeph_core::agent::Agent<C> {
    if !classifiers.enabled || !classifiers.pii_enabled {
        return agent;
    }
    let mut pii_backend = zeph_llm::classifier::candle_pii::CandlePiiClassifier::new(
        classifiers.pii_model.as_str(),
        classifiers.pii_threshold,
    );
    if let Some(hash) = &classifiers.pii_model_sha256 {
        pii_backend = pii_backend.with_sha256(hash.as_str());
    }
    if let Some(token) = &classifiers.hf_token {
        pii_backend = pii_backend.with_hf_token(token.as_str());
    }
    let backend_arc: std::sync::Arc<dyn zeph_llm::classifier::PiiDetector> =
        std::sync::Arc::new(pii_backend);
    tracing::info!(
        repo_id = %classifiers.pii_model,
        threshold = classifiers.pii_threshold,
        allowlist_len = classifiers.pii_ner_allowlist.len(),
        "PII classifier attached (model loads lazily on first use)"
    );
    let agent = agent.with_pii_detector(backend_arc, classifiers.pii_threshold);
    if classifiers.pii_ner_allowlist.is_empty() {
        agent
    } else {
        agent.with_pii_ner_allowlist(classifiers.pii_ner_allowlist.clone())
    }
}

/// Wire the `CandleNerClassifier` into the PII union merge pipeline.
///
/// Only active when `classifiers.enabled = true` AND `security.pii_filter.enabled = true`.
/// Uses `classifiers.ner_model` as the NER model repo ID.
#[cfg(feature = "classifiers")]
pub(crate) fn apply_pii_ner_classifier<C: Channel>(
    agent: zeph_core::agent::Agent<C>,
    config: &Config,
) -> zeph_core::agent::Agent<C> {
    apply_pii_ner_classifier_with_cfg(
        agent,
        &config.classifiers,
        config.security.pii_filter.enabled,
    )
}

/// Wire the `CandleNerClassifier` into the PII union merge pipeline (takes `ClassifiersConfig`
/// and the `security.pii_filter.enabled` flag directly).
///
/// Only active when `classifiers.enabled = true` AND `pii_filter_enabled = true`.
/// Uses `classifiers.ner_model` as the NER model repo ID.
#[cfg(feature = "classifiers")]
pub(crate) fn apply_pii_ner_classifier_with_cfg<C: Channel>(
    agent: zeph_core::agent::Agent<C>,
    classifiers: &zeph_core::config::ClassifiersConfig,
    pii_filter_enabled: bool,
) -> zeph_core::agent::Agent<C> {
    if !classifiers.enabled || !pii_filter_enabled {
        return agent;
    }
    let mut ner_classifier = zeph_llm::classifier::ner::CandleNerClassifier::new(
        classifiers.pii_model.as_str(),
        classifiers.pii_threshold,
    );
    if let Some(token) = &classifiers.hf_token {
        ner_classifier = ner_classifier.with_hf_token(token.as_str());
    }
    let backend = std::sync::Arc::new(ner_classifier);
    tracing::info!(
        repo_id = %classifiers.pii_model,
        threshold = classifiers.pii_threshold,
        "NER PII classifier attached for union merge pipeline (model loads lazily on first use)"
    );
    agent.with_pii_ner_classifier(
        backend,
        classifiers.timeout_ms,
        classifiers.pii_ner_max_chars,
        classifiers.pii_ner_circuit_breaker,
    )
}

/// Wire `enforcement_mode` from config into the agent's injection classifier.
///
/// Must be called AFTER `apply_injection_classifier` so the sanitizer already has
/// a classifier attached. Safe to call when classifiers are disabled (no-op).
#[cfg(feature = "classifiers")]
pub(crate) fn apply_enforcement_mode<C: Channel>(
    agent: zeph_core::agent::Agent<C>,
    config: &Config,
) -> zeph_core::agent::Agent<C> {
    if !config.classifiers.enabled {
        return agent;
    }
    agent.with_enforcement_mode(config.classifiers.enforcement_mode)
}

/// Wire the three-class `AlignSentinel` refinement model into the agent's sanitizer.
///
/// Only active when `classifiers.enabled = true` and `classifiers.three_class_model` is set.
#[cfg(feature = "classifiers")]
pub(crate) fn apply_three_class_classifier<C: Channel>(
    agent: zeph_core::agent::Agent<C>,
    config: &Config,
) -> zeph_core::agent::Agent<C> {
    apply_three_class_classifier_with_cfg(agent, &config.classifiers)
}

/// Wire the three-class `AlignSentinel` refinement model into the agent's sanitizer (takes `ClassifiersConfig` directly).
#[cfg(feature = "classifiers")]
pub(crate) fn apply_three_class_classifier_with_cfg<C: Channel>(
    agent: zeph_core::agent::Agent<C>,
    classifiers: &zeph_core::config::ClassifiersConfig,
) -> zeph_core::agent::Agent<C> {
    let Some(ref repo_id) = classifiers.three_class_model else {
        return agent;
    };
    if !classifiers.enabled {
        return agent;
    }
    let mut classifier =
        zeph_llm::classifier::three_class::CandleThreeClassClassifier::new(repo_id.as_str());
    if let Some(token) = &classifiers.hf_token {
        classifier = classifier.with_hf_token(token.as_str());
    }
    if let Some(hash) = &classifiers.three_class_model_sha256 {
        classifier = classifier.with_sha256(hash.as_str());
    }
    let backend = std::sync::Arc::new(classifier);
    tracing::info!(
        repo_id = %repo_id,
        threshold = classifiers.three_class_threshold,
        "three-class AlignSentinel classifier attached (model loads lazily on first use)"
    );
    agent.with_three_class_classifier(backend, classifiers.three_class_threshold)
}

/// Wire the `TurnCausalAnalyzer` into the agent's security config.
///
/// Only active when `security.causal_ipi.enabled = true`.
/// Wire the VIGIL pre-sanitizer gate into the agent from the full config.
///
/// This must NOT be called for subagent sessions — subagent builders omit this call,
/// leaving `SecurityState::vigil = None` (the subagent exemption invariant, spec FR-009).
pub(crate) fn apply_vigil<C: Channel>(
    agent: zeph_core::agent::Agent<C>,
    vigil: &zeph_config::VigilConfig,
) -> zeph_core::agent::Agent<C> {
    if !vigil.enabled {
        return agent;
    }
    tracing::info!(
        strict_mode = vigil.strict_mode,
        extra_patterns = vigil.extra_patterns.len(),
        "VIGIL pre-sanitizer gate enabled"
    );
    agent.with_vigil_config(vigil.clone())
}

pub(crate) fn apply_causal_analyzer<C: Channel>(
    agent: zeph_core::agent::Agent<C>,
    provider: zeph_llm::any::AnyProvider,
    config: &Config,
    secret_registry: Option<&Arc<zeph_sanitizer::secret_mask::SecretMaskRegistry>>,
) -> zeph_core::agent::Agent<C> {
    let resolved = config
        .security
        .causal_ipi
        .provider
        .as_deref()
        .filter(|s| !s.is_empty())
        .and_then(
            |name| match crate::bootstrap::create_named_provider(name, config) {
                Ok(p) => {
                    tracing::info!(provider = %name, "causal IPI dedicated provider configured");
                    Some(p)
                }
                Err(e) => {
                    tracing::warn!(
                        provider = %name,
                        error = %e,
                        "causal IPI provider resolution failed, falling back to primary"
                    );
                    None
                }
            },
        );
    apply_causal_analyzer_with_cfg(
        agent,
        provider,
        resolved,
        &config.security.causal_ipi,
        secret_registry,
    )
}

/// Wire the `TurnCausalAnalyzer` into the agent's security config (takes `CausalIpiConfig` directly).
///
/// `resolved_provider` is an already-resolved named provider from `[[llm.providers]]` for probe
/// calls. When `None`, `provider` (the session's primary) is used as fallback.
///
/// `secret_registry`, when `Some`, wraps the probe provider so its outbound `.chat()` calls
/// mask registered secrets (#5437) — this analyzer is constructed and stored independently of
/// the Agent's own provider fields, so it is not covered by `Agent::with_secret_registry`'s
/// retroactive wrap and must be masked explicitly here.
pub(crate) fn apply_causal_analyzer_with_cfg<C: Channel>(
    agent: zeph_core::agent::Agent<C>,
    provider: zeph_llm::any::AnyProvider,
    resolved_provider: Option<zeph_llm::any::AnyProvider>,
    causal_config: &zeph_sanitizer::causal_ipi::CausalIpiConfig,
    secret_registry: Option<&Arc<zeph_sanitizer::secret_mask::SecretMaskRegistry>>,
) -> zeph_core::agent::Agent<C> {
    let agent = agent.with_shadow_memory_config(&causal_config.shadow_memory);
    if !causal_config.enabled {
        return agent;
    }
    let probe_provider = resolved_provider.unwrap_or(provider);
    let probe_provider = match secret_registry {
        Some(registry) => probe_provider
            .masked(Arc::clone(registry) as Arc<dyn zeph_llm::masking::OutboundMasker>),
        None => probe_provider,
    };
    let analyzer =
        zeph_sanitizer::causal_ipi::TurnCausalAnalyzer::new(probe_provider, causal_config);
    tracing::info!(
        threshold = causal_config.threshold,
        probe_timeout_ms = causal_config.probe_timeout_ms,
        "causal IPI analyzer attached"
    );
    agent.with_causal_analyzer(analyzer)
}

/// Wire the SONAR `NliSanitizer` into the agent's security config.
///
/// Resolves `security.content_isolation.nli.provider` from `[[llm.providers]]`, falling back to
/// the session's primary provider when unset or resolution fails (mirrors
/// [`apply_causal_analyzer`]).
pub(crate) fn apply_nli_sanitizer<C: Channel>(
    agent: zeph_core::agent::Agent<C>,
    provider: zeph_llm::any::AnyProvider,
    config: &Config,
    secret_registry: Option<&Arc<zeph_sanitizer::secret_mask::SecretMaskRegistry>>,
) -> zeph_core::agent::Agent<C> {
    let nli_config = &config.security.content_isolation.nli;
    let resolved = nli_config.provider.as_non_empty().and_then(|name| {
        match crate::bootstrap::create_named_provider(name, config) {
            Ok(p) => {
                tracing::info!(provider = %name, "NLI dedicated provider configured");
                Some(p)
            }
            Err(e) => {
                tracing::warn!(
                    provider = %name,
                    error = %e,
                    "NLI provider resolution failed, falling back to primary"
                );
                None
            }
        }
    });
    apply_nli_sanitizer_with_cfg(agent, provider, resolved, nli_config, secret_registry)
}

/// Wire the SONAR `NliSanitizer` into the agent's security config (takes `NliConfig` directly).
///
/// `resolved_provider` is an already-resolved named provider from `[[llm.providers]]` for NLI
/// entailment calls. When `None`, `provider` (the session's primary) is used as fallback.
/// No-op when `nli_config.enabled` is `false`.
///
/// `secret_registry`, when `Some`, wraps the NLI provider so its outbound entailment calls
/// mask registered secrets (#5437) — like the causal analyzer, this sanitizer holds its own
/// provider handle outside the Agent's provider fields and needs an explicit wrap here.
pub(crate) fn apply_nli_sanitizer_with_cfg<C: Channel>(
    agent: zeph_core::agent::Agent<C>,
    provider: zeph_llm::any::AnyProvider,
    resolved_provider: Option<zeph_llm::any::AnyProvider>,
    nli_config: &zeph_sanitizer::nli::NliConfig,
    secret_registry: Option<&Arc<zeph_sanitizer::secret_mask::SecretMaskRegistry>>,
) -> zeph_core::agent::Agent<C> {
    if !nli_config.enabled {
        return agent;
    }
    let nli_provider = resolved_provider.unwrap_or(provider);
    let nli_provider = match secret_registry {
        Some(registry) => {
            nli_provider.masked(Arc::clone(registry) as Arc<dyn zeph_llm::masking::OutboundMasker>)
        }
        None => nli_provider,
    };
    let dyn_provider: Arc<dyn zeph_llm::LlmProviderDyn> = Arc::new(nli_provider);
    let sanitizer = zeph_sanitizer::nli::NliSanitizer::new(nli_config.clone(), Some(dyn_provider));
    tracing::info!(
        threshold = nli_config.threshold,
        timeout_ms = nli_config.timeout_ms,
        "NLI sanitizer attached"
    );
    agent.with_nli_sanitizer(sanitizer)
}

/// Wire the PAAC `SecretMaskRegistry` into the agent's security config.
///
/// The registry is created once at bootstrap (gated on
/// `security.content_isolation.secret_masking.enabled`) and shared across the outbound LLM
/// masking boundary (`llm_dispatch.rs`) and the tool-dispatch unmasking boundary
/// (`tier_loop.rs`). No-op when `registry` is `None` (masking disabled).
pub(crate) fn apply_secret_masking<C: Channel>(
    agent: zeph_core::agent::Agent<C>,
    registry: Option<Arc<zeph_sanitizer::secret_mask::SecretMaskRegistry>>,
) -> zeph_core::agent::Agent<C> {
    if let Some(registry) = registry {
        tracing::info!("secret mask registry attached");
        agent.with_secret_registry(registry)
    } else {
        agent
    }
}

/// Wires a [`zeph_core::debug_dump::DebugDumper`] into `agent` for `dir`/`format`, shared by
/// every agent entry point that honors `[debug] enabled = true` (CLI, ACP, daemon, serve-sessions).
///
/// Logs and falls back to `agent` unchanged on initialization failure rather than propagating
/// the error, matching the original per-site behavior. Returns the effective dump directory
/// alongside the agent — the dumper's own per-run subdirectory on success, or `dir` unchanged on
/// failure — so callers needing the directory for further wiring (e.g. `src/runner.rs`'s
/// trace-collector setup) don't have to duplicate the `DebugDumper::new` match arm.
pub(crate) fn apply_debug_dumper<C: Channel>(
    agent: Agent<C>,
    dir: &Path,
    format: zeph_core::debug_dump::DumpFormat,
) -> (Agent<C>, PathBuf) {
    match zeph_core::debug_dump::DebugDumper::new(dir, format) {
        Ok(dumper) => {
            let session_dir = dumper.dir().to_owned();
            (agent.with_debug_dumper(dumper), session_dir)
        }
        Err(e) => {
            tracing::warn!(error = %e, "debug dump initialization failed");
            (agent, dir.to_owned())
        }
    }
}

pub(crate) async fn apply_code_indexer(
    full_config: &Config,
    qdrant_ops: Option<QdrantOps>,
    embed_provider: zeph_llm::any::AnyProvider,
    pool: zeph_db::DbPool,
    cli_mode: bool,
    status_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    supervisor: Option<zeph_common::TaskSupervisor>,
) -> CodeIndexerSetup {
    let config = &full_config.index;
    if !config.enabled {
        return (None, None);
    }

    let embedding_provider_name = config
        .embedding_provider
        .as_ref()
        .and_then(|p| p.as_non_empty())
        .map(str::to_owned)
        .unwrap_or_default();

    let init = async {
        let ops = qdrant_ops.ok_or_else(|| {
            anyhow::anyhow!("code index requires Qdrant backend (vector_backend = \"qdrant\")")
        })?;
        let store = CodeStore::with_ops(ops, pool);
        let provider_arc = std::sync::Arc::new(embed_provider);
        let base_indexer = CodeIndexer::new(
            store,
            provider_arc,
            IndexerConfig {
                concurrency: config.concurrency,
                batch_size: config.batch_size,
                memory_batch_size: config.memory_batch_size,
                max_file_bytes: config.max_file_bytes,
                embed_concurrency: config.embed_concurrency,
                embedding_provider: embedding_provider_name,
                initial_pass_batch_delay_ms: config.initial_pass_batch_delay_ms,
                ..IndexerConfig::default()
            },
        );
        // NOTE: intentionally NOT attaching the TaskSupervisor as BlockingSpawner for the
        // indexer's per-file chunk tasks. The supervisor's `spawn_blocking_named` wraps each
        // blocking task in a `tokio::spawn(async { semaphore.acquire().await; ... })` — with
        // 971+ files this queues up hundreds of async tasks competing with the agent turn
        // for tokio worker threads (#3357).
        // Plain `tokio::task::spawn_blocking` (the fallback when spawner = None) routes
        // CPU-heavy chunk work to the dedicated blocking thread pool, keeping async workers free.
        // TODO(review): re-enable via a lightweight atomic counter rather than BlockingSpawner
        // once the dependency cycle is resolved (#2961).
        let indexer = std::sync::Arc::new(base_indexer);
        anyhow::Ok(indexer)
    };

    match init.await {
        Ok(indexer) => {
            let (progress_tx, progress_rx) =
                tokio::sync::watch::channel(zeph_index::IndexProgress::default());
            let workspace_root = config.workspace_root.as_deref().map_or_else(
                || std::env::current_dir().unwrap_or_default(),
                |p| p.canonicalize().unwrap_or_else(|_| p.to_path_buf()),
            );
            if cli_mode {
                spawn_index_progress_printer(progress_tx.subscribe());
            }
            spawn_background_indexer(
                indexer.clone(),
                workspace_root.clone(),
                progress_tx,
                cli_mode,
                status_tx.clone(),
                supervisor.clone(),
            );
            tracing::info!("code indexer started");
            let watcher = start_index_watcher(
                config.watch,
                &workspace_root,
                indexer,
                status_tx,
                supervisor,
            );
            (watcher, Some(progress_rx))
        }
        Err(e) => {
            tracing::warn!("code indexer initialization failed: {e:#}");
            (None, None)
        }
    }
}

fn spawn_index_progress_printer(mut rx: tokio::sync::watch::Receiver<zeph_index::IndexProgress>) {
    tokio::spawn(async move {
        // EXEMPT(#5143): single-use CLI printer, self-terminates after one eprintln; no supervisor needed
        while rx.changed().await.is_ok() {
            let p = rx.borrow_and_update().clone();
            if p.files_total > 0 {
                eprintln!(
                    "Indexing codebase in the background ({} files) — you can start chatting now.",
                    p.files_total
                );
                break;
            }
        }
    });
}

/// Spawn the background indexing task, optionally through the workspace `TaskSupervisor`.
///
/// # Scope note (AC1 partial — #2961)
///
/// The indexer launcher (`index_project`) is registered as a single `RunOnce` supervisor task
/// named `"index_project"`. Individual per-file chunk tasks inside `CodeIndexer` are **not**
/// registered with the supervisor because `zeph-core` depends on `zeph-index` (creating a cycle
/// if `zeph-index` were to import `zeph-core`). AC1 is therefore narrowed to
/// "indexer launch is visible in the supervisor registry" rather than
/// "per-file chunk tasks are visible". A follow-up issue should track full chunk-level
/// visibility once the dependency cycle is resolved upstream.
fn spawn_background_indexer(
    indexer: std::sync::Arc<CodeIndexer>,
    root: std::path::PathBuf,
    progress_tx: tokio::sync::watch::Sender<zeph_index::IndexProgress>,
    cli_mode: bool,
    status_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    supervisor: Option<zeph_common::TaskSupervisor>,
) {
    if let Some(tx) = status_tx {
        spawn_index_progress_status_forwarder(progress_tx.subscribe(), tx, supervisor.clone());
    }

    let fut = async move {
        match indexer.index_project(&root, Some(&progress_tx)).await {
            Ok(report) => {
                tracing::info!(
                    files = report.files_indexed,
                    chunks = report.chunks_created,
                    ms = report.duration_ms,
                    "project indexed"
                );
                if cli_mode {
                    eprintln!(
                        "Codebase indexed: {} files, {} chunks ({}s) — code search is ready.",
                        report.files_indexed,
                        report.chunks_created,
                        report.duration_ms / 1000,
                    );
                }
            }
            Err(e) => tracing::warn!("background indexing failed: {e:#}"),
        }
    };
    if let Some(sup) = supervisor {
        // Wrap the one-shot future in Arc<parking_lot::Mutex<Option<_>>> so the Fn factory
        // can hand it off on the first (and only) call. RunOnce tasks are never restarted,
        // so take() will be Some exactly once.
        let fut_cell = std::sync::Arc::new(parking_lot::Mutex::new(Some(fut)));
        sup.spawn(zeph_common::TaskDescriptor {
            name: "index_project",
            restart: zeph_common::RestartPolicy::RunOnce,
            factory: move || {
                let f = fut_cell.lock().take();
                async move {
                    if let Some(f) = f {
                        f.await;
                    } else {
                        tracing::warn!(
                            "index_project RunOnce factory called after handoff — \
                             task will not restart; this indicates a policy misconfiguration"
                        );
                    }
                }
            },
        });
    } else {
        tokio::spawn(fut); // EXEMPT(#5143): no-supervisor fallback for spawn_background_indexer — None branch
    }
}

/// Forward `IndexProgress` updates to `status_tx` during the initial full-repo pass, so TUI,
/// Telegram, and Discord all see the same "Indexing repository…" signal (CLI additionally gets
/// `spawn_index_progress_printer`'s one-shot eprintln, kept separate for its own convention).
fn spawn_index_progress_status_forwarder(
    mut progress_rx: tokio::sync::watch::Receiver<zeph_index::IndexProgress>,
    status_tx: tokio::sync::mpsc::UnboundedSender<String>,
    supervisor: Option<zeph_common::TaskSupervisor>,
) {
    let fut = async move {
        while progress_rx.changed().await.is_ok() {
            let p = progress_rx.borrow_and_update().clone();
            if p.files_total == 0 {
                continue;
            }
            let _ = status_tx.send(format!(
                "Indexing repository… ({}/{} files)",
                p.files_done, p.files_total
            ));
            if p.files_done >= p.files_total {
                let _ = status_tx.send("Indexing complete".to_owned());
                break;
            }
        }
    };
    if let Some(sup) = supervisor {
        let fut_cell = std::sync::Arc::new(parking_lot::Mutex::new(Some(fut)));
        sup.spawn(zeph_common::TaskDescriptor {
            name: "index_project_progress",
            restart: zeph_common::RestartPolicy::RunOnce,
            factory: move || {
                let f = fut_cell.lock().take();
                async move {
                    if let Some(f) = f {
                        f.await;
                    }
                }
            },
        });
    } else {
        tokio::spawn(fut); // EXEMPT(#5143): no-supervisor fallback, mirrors spawn_background_indexer's None branch
    }
}

fn start_index_watcher(
    watch: bool,
    root: &std::path::Path,
    indexer: std::sync::Arc<CodeIndexer>,
    status_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    supervisor: Option<zeph_common::TaskSupervisor>,
) -> Option<IndexWatcher> {
    if !watch {
        return None;
    }
    match IndexWatcher::start(root, indexer, status_tx, supervisor) {
        Ok(w) => {
            tracing::info!("index watcher started");
            Some(w)
        }
        Err(e) => {
            tracing::warn!("index watcher failed to start: {e:#}");
            None
        }
    }
}

pub(crate) fn apply_code_retrieval<C: Channel>(agent: Agent<C>, config: &IndexConfig) -> Agent<C> {
    if !config.enabled {
        return agent;
    }

    // When mcp_enabled, skip static repo-map injection and register IndexMcpServer instead.
    if config.mcp_enabled {
        if config.repo_map_tokens > 0 {
            tracing::warn!(
                "index.repo_map_tokens is set but index.mcp_enabled=true — \
                 static repo-map injection is disabled; use IndexMcpServer tools instead"
            );
        }
        let cwd = std::env::current_dir().unwrap_or_default();
        agent.with_index_mcp_server(cwd)
    } else if config.repo_map_tokens > 0 {
        agent.with_repo_map(config.repo_map_tokens, config.repo_map_ttl_secs)
    } else {
        agent
    }
}

/// Construct a [`zeph_index::retriever::CodeRetriever`] and wire it onto the agent so
/// automatic code RAG context injection returns results on every agent turn.
///
/// Returns the agent unchanged when any of:
/// - `config.enabled = false`
/// - `config.mcp_enabled = true` (MCP pull-based mode replaces static injection)
/// - `qdrant_ops.is_none()` (no vector backend available)
/// - `config.budget_ratio <= 0.0`
pub(crate) fn apply_code_rag_retriever<C: Channel>(
    agent: zeph_core::agent::Agent<C>,
    config: &IndexConfig,
    qdrant_ops: Option<QdrantOps>,
    provider: zeph_llm::any::AnyProvider,
    pool: zeph_db::DbPool,
) -> zeph_core::agent::Agent<C> {
    if !config.enabled || config.budget_ratio <= 0.0 {
        return agent;
    }
    if config.mcp_enabled {
        tracing::debug!("code RAG retriever skipped: mcp_enabled=true, using MCP pull-based mode");
        return agent;
    }
    let Some(ops) = qdrant_ops else {
        tracing::debug!("code RAG retriever skipped: no qdrant ops");
        return agent;
    };

    let store = CodeStore::with_ops(ops, pool);
    let embedding_provider_name = config
        .embedding_provider
        .as_ref()
        .and_then(|p| p.as_non_empty())
        .map(str::to_owned)
        .unwrap_or_default();
    let retrieval_config = zeph_index::retriever::RetrievalConfig {
        max_chunks: config.max_chunks,
        score_threshold: config.score_threshold,
        budget_ratio: config.budget_ratio,
        embedding_provider: embedding_provider_name,
        ..zeph_index::retriever::RetrievalConfig::default()
    };
    let retriever = std::sync::Arc::new(zeph_index::retriever::CodeRetriever::new(
        store,
        std::sync::Arc::new(provider),
        retrieval_config,
    ));
    tracing::info!(
        max_chunks = config.max_chunks,
        score_threshold = config.score_threshold,
        budget_ratio = config.budget_ratio,
        "code RAG retriever wired"
    );
    agent.with_code_retriever(retriever)
}

pub(crate) fn build_search_code_executor(
    config: &Config,
    qdrant_ops: Option<QdrantOps>,
    provider: zeph_llm::any::AnyProvider,
    pool: zeph_db::DbPool,
    mcp_manager: Option<Arc<zeph_mcp::McpManager>>,
) -> Option<SearchCodeExecutor> {
    if !config.index.search_enabled {
        return None;
    }

    let allowed_paths = config
        .tools
        .shell
        .allowed_paths
        .iter()
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    let mut executor = SearchCodeExecutor::new(allowed_paths);

    if let Some(ops) = qdrant_ops {
        let backend = SemanticCodeSearch {
            store: CodeStore::with_ops(ops, pool),
            provider: Arc::new(provider),
            score_threshold: config.index.score_threshold,
        };
        executor = executor.with_semantic_backend(Arc::new(backend));
    }

    if let Some(manager) = mcp_manager
        && let Some(server_id) = resolve_search_lsp_server_id(config)
        && manager.is_server_connected(&server_id)
    {
        let backend = McpCodeSearch { manager, server_id };
        executor = executor.with_lsp_backend(Arc::new(backend));
    }

    Some(executor)
}

/// Builds the `DiagnosticsExecutor` for the `diagnostics` tool, sandboxed to the same
/// `tools.shell.allowed_paths` as `FileExecutor` and `SearchCodeExecutor`. Reuses
/// `tools.shell.timeout` to bound the `cargo check`/`cargo clippy` subprocess — the same
/// existing knob users already tune for long-running shell commands, rather than
/// introducing a separate `tools.diagnostics.*` config surface for one field.
pub(crate) fn build_diagnostics_executor(config: &Config) -> zeph_tools::DiagnosticsExecutor {
    let allowed_paths = config
        .tools
        .shell
        .allowed_paths
        .iter()
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    zeph_tools::DiagnosticsExecutor::new(allowed_paths)
        .with_timeout(std::time::Duration::from_secs(config.tools.shell.timeout))
}

/// Assembles the `file -> shell -> scrape -> cwd -> diagnostics` composite tool chain
/// shared by all three live entry points (CLI's [`build_tool_setup`], `acp.rs`'s
/// `spawn_acp_agent`, `daemon.rs`'s `run_daemon`) and the #5578 dispatch-reachability
/// tests. Pinning the nesting order in one function means a future reorder or drop of
/// one of these executors in production is caught by every caller — including the
/// tests — instead of a hand-copied test chain silently going stale.
///
/// Generic over the shell/scrape/file executor types so each entry point can pass in
/// its own already-customized executors (audit logging, OS sandbox, task supervisor,
/// egress config, etc. attached via their builder methods beforehand) without this
/// function needing to know about any of that; the tests pass in bare/default ones.
#[expect(
    clippy::type_complexity,
    reason = "concrete nested CompositeExecutor chain type mirrors the production wiring \
              exactly; the whole point of this helper is pinning down that exact static \
              chain shape, so hiding it behind a boxed/dyn return would defeat it"
)]
pub(crate) fn build_base_executor_chain<F, S, W>(
    file_executor: F,
    shell_executor: S,
    scrape_executor: W,
    diagnostics_executor: zeph_tools::DiagnosticsExecutor,
) -> zeph_tools::CompositeExecutor<
    F,
    zeph_tools::CompositeExecutor<
        S,
        zeph_tools::CompositeExecutor<
            W,
            zeph_tools::CompositeExecutor<
                zeph_tools::SetCwdExecutor,
                zeph_tools::DiagnosticsExecutor,
            >,
        >,
    >,
>
where
    F: zeph_tools::ToolExecutor,
    S: zeph_tools::ToolExecutor,
    W: zeph_tools::ToolExecutor,
{
    zeph_tools::CompositeExecutor::new(
        file_executor,
        zeph_tools::CompositeExecutor::new(
            shell_executor,
            zeph_tools::CompositeExecutor::new(
                scrape_executor,
                zeph_tools::CompositeExecutor::new(
                    zeph_tools::SetCwdExecutor,
                    diagnostics_executor,
                ),
            ),
        ),
    )
}

/// MCP tool-id set consumed by [`zeph_tools::TrustGateExecutor`] to recognize genuinely
/// MCP-sourced tools. Populated after MCP servers connect via [`register_mcp_tool_ids`].
pub(crate) type McpToolIdsHandle = Arc<RwLock<std::collections::HashSet<String>>>;

/// Wraps a fully-composed tool-executor tree in the single outermost `TrustGateExecutor`
/// gate shared by every agent entry point (CLI, ACP, daemon).
///
/// `inner` must already contain the ENTIRE tool-executor composition for the run — base
/// chain, MCP, search, skill loader/invoke, memory, overflow. Wrapping only a sub-tree (as
/// ACP and daemon used to do before #5611) lets tools composed outside the wrap bypass
/// Quarantine/Blocked enforcement entirely, since `TrustGateExecutor::check_trust` only runs
/// for calls that dispatch through the gate itself.
///
/// Returns the gated executor plus the MCP tool-id handle the caller must populate (via
/// [`register_mcp_tool_ids`]) once the MCP tool list is known.
pub(crate) fn apply_common_tool_gating(
    inner: zeph_tools::DynExecutor,
    permission_policy: &zeph_tools::PermissionPolicy,
) -> (zeph_tools::DynExecutor, McpToolIdsHandle) {
    let gated = zeph_tools::TrustGateExecutor::new(inner, permission_policy.clone());
    let handle = gated.mcp_tool_ids_handle();
    (zeph_tools::DynExecutor(Arc::new(gated)), handle)
}

/// Populates a `TrustGateExecutor` MCP tool-id handle from the connected MCP tool list, so
/// Quarantine denies ALL MCP-sourced tools — not just those matching `QUARANTINE_DENIED` by
/// name.
pub(crate) fn register_mcp_tool_ids(handle: &McpToolIdsHandle, mcp_tools: &[zeph_mcp::McpTool]) {
    let ids: std::collections::HashSet<String> = mcp_tools
        .iter()
        .map(zeph_mcp::McpTool::sanitized_id)
        .collect();
    *handle.write() = ids;
}

/// Builds the [`zeph_core::ProviderConfigSnapshot`] passed to `AgentBuilder::with_provider_pool`
/// by every agent entry point (CLI, ACP, daemon).
///
/// Centralizes the provider-secret and embedding-model extraction from `config.secrets` /
/// `config.timeouts` / `config.llm` so a populated snapshot reaches `resolve_background_provider`
/// from all three sites — an empty snapshot broke background-provider lookups (#5450).
pub(crate) fn build_provider_config_snapshot(config: &Config) -> zeph_core::ProviderConfigSnapshot {
    zeph_core::ProviderConfigSnapshot {
        claude_api_key: config
            .secrets
            .claude_api_key
            .as_ref()
            .map(|s| s.expose().to_owned()),
        openai_api_key: config
            .secrets
            .openai_api_key
            .as_ref()
            .map(|s| s.expose().to_owned()),
        gemini_api_key: config
            .secrets
            .gemini_api_key
            .as_ref()
            .map(|s| s.expose().to_owned()),
        compatible_api_keys: config
            .secrets
            .compatible_api_keys
            .iter()
            .map(|(k, v)| (k.clone(), v.expose().to_owned()))
            .collect(),
        llm_request_timeout_secs: config.timeouts.llm_request_timeout_secs,
        embedding_model: config.llm.embedding_model.clone(),
        gonka_private_key: config
            .secrets
            .gonka_private_key
            .as_ref()
            .map(|s| zeroize::Zeroizing::new(s.expose().to_owned())),
        gonka_address: config
            .secrets
            .gonka_address
            .as_ref()
            .map(|s| s.expose().to_owned()),
        cocoon_access_hash: config
            .secrets
            .cocoon_access_hash
            .as_ref()
            .map(|s| s.expose().to_owned()),
    }
}

fn resolve_search_lsp_server_id(config: &Config) -> Option<String> {
    config
        .mcp
        .servers
        .iter()
        .find(|server| server.id == "mcpls")
        .or_else(|| {
            config.mcp.servers.iter().find(|server| {
                server
                    .command
                    .as_deref()
                    .is_some_and(|command| command.ends_with("mcpls"))
            })
        })
        .map(|server| server.id.clone())
}

#[cfg(feature = "candle")]
pub(crate) fn apply_candle_stt<C: Channel>(
    agent: zeph_core::agent::Agent<C>,
    entry: &zeph_core::config::ProviderEntry,
    language: &str,
) -> zeph_core::agent::Agent<C> {
    let model = entry.stt_model.as_deref().unwrap_or("openai/whisper-tiny");
    match zeph_llm::candle_whisper::CandleWhisperProvider::load(
        model,
        None,
        language,
        entry.stt_model_sha256.as_deref(),
    ) {
        Ok(provider) => {
            tracing::info!("STT enabled via candle-whisper (model: {model})");
            agent.with_stt(Box::new(provider))
        }
        Err(e) => {
            tracing::error!("failed to load candle-whisper: {e}");
            agent
        }
    }
}
pub(crate) fn apply_whisper_stt<C: Channel>(
    agent: zeph_core::agent::Agent<C>,
    entry: &zeph_core::config::ProviderEntry,
    language: &str,
    api_key: String,
) -> zeph_core::agent::Agent<C> {
    let model = entry.stt_model.as_deref().unwrap_or("whisper-1");
    let base_url = entry
        .base_url
        .as_deref()
        .unwrap_or("https://api.openai.com/v1");
    let whisper = zeph_llm::whisper::WhisperProvider::new(
        zeph_core::http::default_client(),
        api_key,
        base_url,
        model,
    )
    .with_language(language);
    tracing::info!(model, base_url, "STT enabled via Whisper API");
    agent.with_stt(Box::new(whisper))
}

/// Apply Cocoon STT to the agent.
///
/// Constructs a [`CocoonClient`] from `entry`'s `cocoon_client_url` and `cocoon_access_hash`
/// fields, then wires up a [`CocoonSttProvider`] using the LLM timeout so that large audio
/// files are not cut off by a short health-check timeout.
///
/// [`CocoonClient`]: zeph_llm::cocoon::CocoonClient
/// [`CocoonSttProvider`]: zeph_llm::cocoon::CocoonSttProvider
#[cfg(feature = "cocoon")]
pub(crate) fn apply_cocoon_stt<C: Channel>(
    agent: zeph_core::agent::Agent<C>,
    entry: &zeph_core::config::ProviderEntry,
    language: &str,
    llm_timeout_secs: u64,
) -> zeph_core::agent::Agent<C> {
    let model = entry.stt_model.as_deref().unwrap_or("whisper-1");
    let base_url = entry
        .cocoon_client_url
        .as_deref()
        .unwrap_or("http://localhost:10000");
    let client = std::sync::Arc::new(zeph_llm::cocoon::CocoonClient::new(
        base_url,
        entry.cocoon_access_hash.clone(),
        std::time::Duration::from_secs(llm_timeout_secs),
    ));
    let stt = zeph_llm::cocoon::CocoonSttProvider::new(model, client).with_language(language);
    tracing::info!(model, base_url, "STT enabled via Cocoon sidecar");
    agent.with_stt(Box::new(stt))
}

/// Apply MCP tool pruning (LLM-based) configuration to the agent.
///
/// Converts `ToolPruningConfig` into `PruningParams` and optionally resolves a dedicated
/// provider for pruning LLM calls.
pub(crate) fn apply_mcp_pruning<C: Channel>(
    agent: zeph_core::agent::Agent<C>,
    config: &zeph_core::config::Config,
) -> zeph_core::agent::Agent<C> {
    let pruning = &config.mcp.pruning;
    if !pruning.enabled {
        return agent;
    }

    let params = zeph_mcp::PruningParams {
        max_tools: pruning.max_tools,
        min_tools_to_prune: pruning.min_tools_to_prune,
        always_include: pruning.always_include.clone(),
    };

    let pruning_provider = if pruning.pruning_provider.is_empty() {
        None
    } else {
        match crate::bootstrap::create_named_provider(&pruning.pruning_provider, config) {
            Ok(p) => {
                tracing::info!(
                    provider = %pruning.pruning_provider,
                    "MCP pruning provider configured"
                );
                Some(p)
            }
            Err(e) => {
                tracing::warn!(
                    provider = %pruning.pruning_provider,
                    "MCP pruning provider resolution failed, using primary: {e:#}"
                );
                None
            }
        }
    };

    agent.with_mcp_pruning(params, true, pruning_provider)
}

/// Apply embedding-based MCP tool discovery configuration to the agent (#2321).
///
/// Converts `ToolDiscoveryConfig` into `DiscoveryParams` and `ToolDiscoveryStrategy`,
/// optionally resolving a dedicated embedding provider for query embeddings.
pub(crate) fn apply_mcp_discovery<C: Channel>(
    agent: zeph_core::agent::Agent<C>,
    config: &zeph_core::config::Config,
) -> zeph_core::agent::Agent<C> {
    use zeph_core::config::ToolDiscoveryStrategyConfig;
    use zeph_mcp::ToolDiscoveryStrategy;

    let discovery = &config.mcp.tool_discovery;

    let strategy = match discovery.strategy {
        ToolDiscoveryStrategyConfig::Embedding => ToolDiscoveryStrategy::Embedding,
        ToolDiscoveryStrategyConfig::Llm => ToolDiscoveryStrategy::Llm,
        _ => ToolDiscoveryStrategy::None,
    };

    if strategy == ToolDiscoveryStrategy::Llm {
        // Llm is the default — handled by apply_mcp_pruning.
        return agent;
    }

    let params = zeph_mcp::DiscoveryParams {
        top_k: discovery.top_k,
        min_similarity: discovery.min_similarity,
        min_tools_to_filter: discovery.min_tools_to_filter,
        always_include: discovery.always_include.clone(),
        strict: discovery.strict,
    };

    let discovery_provider = if discovery.embedding_provider.is_empty() {
        None
    } else {
        match crate::bootstrap::create_named_provider(&discovery.embedding_provider, config) {
            Ok(p) => {
                tracing::info!(
                    provider = %discovery.embedding_provider,
                    "MCP tool discovery embedding provider configured"
                );
                Some(p)
            }
            Err(e) => {
                tracing::warn!(
                    provider = %discovery.embedding_provider,
                    "MCP tool discovery provider resolution failed, using primary: {e:#}"
                );
                None
            }
        }
    };

    agent.with_mcp_discovery(strategy, params, discovery_provider)
}

/// Wire a [`zeph_skills::proactive::ProactiveExplorer`] onto the agent from config.
///
/// Resolves the generation provider, builds the explorer, and calls
/// [`Agent::with_proactive_explorer`]. Returns the agent unchanged when
/// `config.skills.proactive_exploration.enabled = false`.
pub(crate) fn apply_proactive_explorer<C: zeph_core::channel::Channel>(
    agent: zeph_core::agent::Agent<C>,
    config: &zeph_core::config::Config,
    primary: &zeph_llm::any::AnyProvider,
    evaluator: Option<std::sync::Arc<zeph_skills::evaluator::SkillEvaluator>>,
    skills_paths: &[std::path::PathBuf],
) -> zeph_core::agent::Agent<C> {
    let exp_cfg = &config.skills.proactive_exploration;
    if !exp_cfg.enabled {
        return agent;
    }

    let output_dir = if let Some(ref dir) = exp_cfg.output_dir {
        std::path::PathBuf::from(dir)
    } else if let Some(first) = skills_paths.first() {
        first.join("generated")
    } else {
        crate::bootstrap::skills::managed_skills_dir().join("generated")
    };

    let provider = if exp_cfg.provider.is_empty() {
        primary.clone()
    } else {
        match crate::bootstrap::create_named_provider(&exp_cfg.provider, config) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(
                    provider = %exp_cfg.provider,
                    error = %e,
                    "proactive exploration provider resolution failed, falling back to primary"
                );
                primary.clone()
            }
        }
    };

    let generator = zeph_skills::SkillGenerator::new(provider, output_dir.clone())
        .with_generation_timeout_ms(exp_cfg.timeout_ms);
    let explorer = zeph_skills::proactive::ProactiveExplorer::new(
        generator,
        evaluator,
        output_dir,
        exp_cfg.max_chars,
        exp_cfg.timeout_ms,
        exp_cfg.excluded_domains.clone(),
    );
    tracing::info!("skills.proactive_exploration: enabled");
    agent.with_proactive_explorer(Some(std::sync::Arc::new(explorer)))
}

/// Wire a [`zeph_memory::compression::promotion::PromotionEngine`] onto the agent from config.
///
/// Resolves the output directory and skill writer, then calls
/// [`Agent::with_promotion_engine`]. Returns the agent unchanged when
/// `config.memory.compression_spectrum.enabled = false` or when no `SkillWriter`
/// could be built (missing provider or skills paths).
pub(crate) fn apply_promotion_engine<C: zeph_core::channel::Channel>(
    agent: zeph_core::agent::Agent<C>,
    config: &zeph_core::config::Config,
    primary: &zeph_llm::any::AnyProvider,
    evaluator: Option<std::sync::Arc<zeph_skills::evaluator::SkillEvaluator>>,
    eval_weights: zeph_skills::evaluator::EvaluationWeights,
    eval_threshold: f32,
    skills_paths: &[std::path::PathBuf],
) -> zeph_core::agent::Agent<C> {
    let spectrum_cfg = &config.memory.compression_spectrum;
    if !spectrum_cfg.enabled {
        return agent;
    }

    let output_dir = if let Some(ref dir) = spectrum_cfg.promotion_output_dir {
        std::path::PathBuf::from(dir)
    } else if let Some(first) = skills_paths.first() {
        first.join("promoted")
    } else {
        crate::bootstrap::skills::managed_skills_dir().join("promoted")
    };

    let Some(writer) = crate::bootstrap::skills::build_skill_writer(
        config,
        primary,
        evaluator,
        eval_weights,
        eval_threshold,
        skills_paths,
    ) else {
        return agent;
    };

    let promotion_config = zeph_memory::compression::promotion::PromotionConfig {
        min_occurrences: spectrum_cfg.min_occurrences,
        min_sessions: spectrum_cfg.min_sessions,
        cluster_threshold: spectrum_cfg.cluster_threshold,
    };
    let engine = zeph_memory::compression::promotion::PromotionEngine::new(
        writer,
        promotion_config,
        output_dir,
    );
    tracing::info!("memory.compression_spectrum: enabled");
    agent.with_promotion_engine(Some(std::sync::Arc::new(engine)))
}

/// Build a `SandboxPolicy` from the TOML `[tools.sandbox]` config section.
pub(crate) fn sandbox_policy_from_config(
    cfg: &zeph_config::tools::SandboxConfig,
) -> zeph_tools::sandbox::SandboxPolicy {
    use zeph_config::tools::SandboxProfile;
    use zeph_tools::sandbox::SandboxPolicy;
    SandboxPolicy {
        profile: cfg.profile,
        allow_read: cfg.allow_read.clone(),
        allow_write: cfg.allow_write.clone(),
        allow_network: cfg.profile == SandboxProfile::NetworkAllowAll,
        allow_exec: vec![],
        env_inherit: vec![],
        denied_domains: cfg.denied_domains.clone(),
    }
    .canonicalized()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use zeph_channels::CliChannel;
    use zeph_core::agent::Agent;
    use zeph_core::config::Config;
    use zeph_llm::any::AnyProvider;
    use zeph_llm::ollama::OllamaProvider;
    use zeph_skills::registry::SkillRegistry;
    use zeph_tools::executor::{ToolError, ToolExecutor, ToolOutput};

    use super::*;

    struct NoopExec;

    impl zeph_tools::executor::ToolExecutor for NoopExec {
        async fn execute(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
            Ok(None)
        }
    }

    fn offline_provider() -> AnyProvider {
        AnyProvider::Ollama(OllamaProvider::new(
            "http://127.0.0.1:1",
            "test".into(),
            "embed".into(),
        ))
    }

    async fn memory_pool() -> zeph_db::DbPool {
        zeph_db::DbConfig {
            url: ":memory:".to_owned(),
            ..Default::default()
        }
        .connect()
        .await
        .unwrap()
    }

    async fn file_pool(path: &Path) -> zeph_db::DbPool {
        zeph_db::DbConfig {
            url: path.display().to_string(),
            ..Default::default()
        }
        .connect()
        .await
        .unwrap()
    }

    fn make_agent() -> Agent<CliChannel> {
        let config = Config::load(Path::new("/nonexistent")).unwrap();
        let registry = SkillRegistry::load(&[] as &[std::path::PathBuf]);
        Agent::new(
            offline_provider(),
            CliChannel::new(),
            registry,
            None,
            config.skills.max_active_skills.get(),
            NoopExec,
        )
    }

    #[tokio::test]
    async fn apply_cost_tracker_disabled_returns_agent_unchanged() {
        let agent = make_agent();
        let mut config = Config::load(Path::new("/nonexistent")).unwrap();
        config.cost.enabled = false;
        let result = apply_cost_tracker(agent, &config);
        drop(result);
    }

    #[tokio::test]
    async fn apply_cost_tracker_enabled_attaches_tracker() {
        let agent = make_agent();
        let mut config = Config::load(Path::new("/nonexistent")).unwrap();
        config.cost.enabled = true;
        config.cost.max_daily_cents = 500;
        let result = apply_cost_tracker(agent, &config);
        drop(result);
    }

    #[tokio::test]
    async fn apply_cost_tracker_registers_cocoon_pricing() {
        let agent = make_agent();
        let mut config = Config::load(Path::new("/nonexistent")).unwrap();
        config.cost.enabled = true;
        config.cost.max_daily_cents = 100;
        let entry = zeph_config::ProviderEntry {
            provider_type: zeph_config::ProviderKind::Cocoon,
            model: Some("Qwen/Qwen3-0.6B".into()),
            cocoon_pricing: Some(zeph_config::CocoonPricing {
                prompt_cents_per_1k: 0.01,
                completion_cents_per_1k: 0.03,
            }),
            ..zeph_config::ProviderEntry::default()
        };
        config.llm.providers = vec![entry];
        let result = apply_cost_tracker(agent, &config);
        drop(result);
    }

    /// Regression test for #5728: `CandleNerClassifier` used to be constructed with no
    /// confidence threshold at all, letting low-confidence NER guesses (e.g. a bare Unix
    /// timestamp digit run) get promoted to a `[PII:ACCOUNTNUM]` redaction. The fix threads
    /// `classifiers.pii_threshold` into the constructor; this test locks the call site so a
    /// future revert (e.g. dropping the second constructor argument, or wiring a different
    /// config field) fails to compile or is caught here rather than silently reopening #5728.
    #[test]
    #[cfg(feature = "classifiers")]
    fn apply_pii_ner_classifier_with_cfg_wires_configured_threshold() {
        let agent = make_agent();
        let mut config = Config::load(Path::new("/nonexistent")).unwrap();
        config.classifiers.enabled = true;
        config.security.pii_filter.enabled = true;
        config.classifiers.pii_threshold = 0.42;
        let result = apply_pii_ner_classifier_with_cfg(
            agent,
            &config.classifiers,
            config.security.pii_filter.enabled,
        );
        drop(result);
    }

    #[test]
    fn build_diagnostics_executor_exposes_diagnostics_tool() {
        let config = Config::load(Path::new("/nonexistent")).unwrap();
        let executor = build_diagnostics_executor(&config);
        let defs = executor.tool_definitions();
        assert!(defs.iter().any(|d| d.id == "diagnostics"));
    }

    /// Regression test for #5433: `DiagnosticsExecutor` was fully unit-tested in
    /// `zeph-tools` but never constructed by any live entry point, so the `diagnostics`
    /// tool never appeared in the LLM's tool list. This mirrors the exact composite
    /// nesting shape used in `build_tool_setup`/`acp.rs`/`daemon.rs` and asserts the tool
    /// definition survives the merge.
    #[test]
    fn diagnostics_executor_reachable_through_composite_chain() {
        let config = Config::load(Path::new("/nonexistent")).unwrap();
        let file_executor = zeph_tools::FileExecutor::new(vec![]);
        let shell_executor = zeph_tools::ShellExecutor::new(&config.tools.shell);
        let scrape_executor = zeph_tools::WebScrapeExecutor::new(&config.tools.scrape);
        let cwd_executor = zeph_tools::SetCwdExecutor;
        let diagnostics_executor = build_diagnostics_executor(&config);
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
        let defs = base_executor.tool_definitions();
        assert!(defs.iter().any(|d| d.id == "diagnostics"));
    }

    /// Regression test for #5578: prior reachability tests for #5433 only asserted
    /// `diagnostics` appears in `tool_definitions()`, never that a `ToolCall` for it
    /// actually reaches `DiagnosticsExecutor` through the full composite chain — an
    /// earlier executor silently swallowing the call would have gone unnoticed. Uses
    /// the same `build_base_executor_chain` helper the CLI/ACP/daemon entry points
    /// call, so a future reorder or drop of `diagnostics` in production wiring is
    /// caught here too — a hand-copied chain could not. Dispatches a call with a path
    /// outside `allowed_paths` and asserts `SandboxViolation`, a signal that can only
    /// originate from `DiagnosticsExecutor`.
    #[tokio::test]
    async fn diagnostics_tool_call_dispatches_through_composite_chain() {
        let config = Config::load(Path::new("/nonexistent")).unwrap();
        let file_executor = zeph_tools::FileExecutor::new(vec![]);
        let shell_executor = zeph_tools::ShellExecutor::new(&config.tools.shell);
        let scrape_executor = zeph_tools::WebScrapeExecutor::new(&config.tools.scrape);
        let diagnostics_executor = build_diagnostics_executor(&config);
        let base_executor = build_base_executor_chain(
            file_executor,
            shell_executor,
            scrape_executor,
            diagnostics_executor,
        );

        // Must exist on disk (DiagnosticsExecutor canonicalizes before the sandbox
        // check) while staying outside `allowed_paths` (defaults to cwd). Assumes
        // `TMPDIR`/temp_dir() is not itself under the repo/cwd, true in normal
        // environments.
        let outside = std::env::temp_dir();
        let mut params = serde_json::Map::new();
        params.insert(
            "path".into(),
            serde_json::Value::String(outside.display().to_string()),
        );
        let call = zeph_tools::ToolCall {
            tool_id: "diagnostics".into(),
            params,
            caller_id: None,
            context: None,
            tool_call_id: String::new(),
            skill_name: None,
        };
        let result = base_executor.execute_tool_call(&call).await;
        assert!(
            matches!(result, Err(zeph_tools::ToolError::SandboxViolation { .. })),
            "expected SandboxViolation from DiagnosticsExecutor, got {result:?}"
        );
    }

    #[tokio::test]
    async fn build_search_code_executor_exposes_search_code_tool() {
        let pool = memory_pool().await;
        let config = Config::load(Path::new("/nonexistent")).unwrap();
        assert!(config.index.search_enabled, "expected default to be on");
        let executor =
            build_search_code_executor(&config, None, offline_provider(), pool, None).unwrap();
        let defs = executor.tool_definitions();
        assert!(defs.iter().any(|d| d.id == "search_code"));
    }

    /// Regression test for #5579: `search_code` was wired into `agent_setup.rs`'s
    /// CLI/TUI tool chain and `acp.rs`, but never into `daemon.rs`'s composite chain,
    /// so the `search_code` tool silently never appeared for the `zeph serve --a2a`
    /// entry point. Mirrors the exact nesting shape used in `daemon.rs::run_daemon`
    /// (`CompositeExecutor::new(DynExecutor(base), search_executor)`) and asserts the
    /// tool definition survives the merge.
    #[tokio::test]
    async fn search_code_executor_reachable_through_daemon_composite_chain() {
        let pool = memory_pool().await;
        let config = Config::load(Path::new("/nonexistent")).unwrap();
        let base: std::sync::Arc<dyn zeph_tools::ErasedToolExecutor> =
            std::sync::Arc::new(NoopExec);
        let search_executor =
            build_search_code_executor(&config, None, offline_provider(), pool, None).unwrap();
        let composite =
            zeph_tools::CompositeExecutor::new(zeph_tools::DynExecutor(base), search_executor);
        let defs = composite.tool_definitions();
        assert!(defs.iter().any(|d| d.id == "search_code"));
    }

    /// Regression test confirming `PolicyGateExecutor` composes correctly around a nested
    /// composite executor: `PolicyGateExecutor` used to be constructed only in `runner.rs`
    /// (CLI path) — `acp.rs` and `daemon.rs` built their composite tool chain with no
    /// declarative policy gate at all, so `[tools.policy]` deny rules were silently
    /// unenforced for ACP and daemon (A2A) sessions regardless of config. Mirrors the
    /// wrapping position both entry points now use (the gate wraps the full assembled
    /// composite chain) and asserts a deny rule still blocks the call when the inner
    /// executor is itself a nested composite.
    #[tokio::test]
    async fn policy_gate_executor_reachable_through_composite_chain() {
        let inner = zeph_tools::CompositeExecutor::new(NoopExec, NoopExec);
        let policy_config = zeph_tools::PolicyConfig {
            enabled: true,
            default_effect: zeph_tools::DefaultEffect::Allow,
            rules: vec![zeph_tools::PolicyRuleConfig {
                effect: zeph_tools::PolicyEffect::Deny,
                tool: "shell".into(),
                paths: vec![],
                env: vec![],
                trust_level: None,
                args_match: None,
                capabilities: vec![],
            }],
            ..Default::default()
        };
        let enforcer =
            std::sync::Arc::new(zeph_tools::PolicyEnforcer::compile(&policy_config).unwrap());
        let context = std::sync::Arc::new(RwLock::new(zeph_tools::PolicyContext {
            trust_level: zeph_common::SkillTrustLevel::Trusted,
            env: std::collections::HashMap::new(),
        }));
        let gate = zeph_tools::PolicyGateExecutor::new(inner, enforcer, context);
        let call = zeph_tools::ToolCall {
            tool_id: "shell".into(),
            params: serde_json::Map::new(),
            caller_id: None,
            context: None,
            tool_call_id: String::new(),
            skill_name: None,
        };
        let result = gate.execute_tool_call(&call).await;
        assert!(
            matches!(result, Err(zeph_tools::ToolError::Blocked { .. })),
            "expected PolicyGateExecutor to block a denied tool call even when the inner \
             executor is a nested composite (mirrors acp.rs/daemon.rs wiring), got {result:?}"
        );
    }

    /// Regression test confirming `AdversarialPolicyGateExecutor` composes correctly around
    /// a nested composite executor: this gate used to be constructed only in `runner.rs`
    /// (CLI path) — `acp.rs` and `daemon.rs` never wired the LLM-based
    /// `[tools.adversarial_policy]` gate at all. Mirrors the same nested-composite
    /// wrapping shape as the declarative gate test above and asserts a "DENY" verdict
    /// from the policy LLM still blocks the call.
    #[tokio::test]
    async fn adversarial_policy_gate_executor_reachable_through_composite_chain() {
        struct DenyLlm;
        impl zeph_tools::PolicyLlmClient for DenyLlm {
            fn chat<'a>(
                &'a self,
                _messages: &'a [zeph_tools::PolicyMessage],
            ) -> Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'a>>
            {
                Box::pin(async move { Ok("DENY: blocked by test policy".to_owned()) })
            }
        }

        let inner = zeph_tools::CompositeExecutor::new(NoopExec, NoopExec);
        let validator = std::sync::Arc::new(zeph_tools::PolicyValidator::new(
            vec!["test policy".to_owned()],
            std::time::Duration::from_millis(500),
            false,
            Vec::new(),
        ));
        let llm: std::sync::Arc<dyn zeph_tools::PolicyLlmClient> = std::sync::Arc::new(DenyLlm);
        let gate = zeph_tools::AdversarialPolicyGateExecutor::new(inner, validator, llm);
        let call = zeph_tools::ToolCall {
            tool_id: "shell".into(),
            params: serde_json::Map::new(),
            caller_id: None,
            context: None,
            tool_call_id: String::new(),
            skill_name: None,
        };
        let result = gate.execute_tool_call(&call).await;
        assert!(
            matches!(result, Err(zeph_tools::ToolError::Blocked { .. })),
            "expected AdversarialPolicyGateExecutor to block a DENY verdict even when the \
             inner executor is a nested composite (mirrors acp.rs/daemon.rs wiring), got \
             {result:?}"
        );
    }

    #[tokio::test]
    async fn apply_summary_provider_none_returns_agent_unchanged() {
        let agent = make_agent();
        let result = apply_summary_provider(agent, None);
        drop(result);
    }

    #[tokio::test]
    async fn apply_summary_provider_some_attaches_provider() {
        let agent = make_agent();
        let sp = offline_provider();
        let result = apply_summary_provider(agent, Some(sp));
        drop(result);
    }

    #[tokio::test]
    async fn apply_response_cache_disabled_returns_agent_unchanged() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let pool = file_pool(tmp.path()).await;
        let agent = make_agent();
        let cancel = tokio_util::sync::CancellationToken::new();
        let (result, handle) =
            apply_response_cache(agent, false, pool, 300, false, "embed-model".into(), cancel);
        assert!(
            handle.is_none(),
            "disabled cache must not spawn a background task"
        );
        drop(result);
    }

    #[tokio::test]
    async fn apply_response_cache_enabled_attaches_cache() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let pool = file_pool(tmp.path()).await;
        let agent = make_agent();
        let cancel = tokio_util::sync::CancellationToken::new();
        let (result, handle) =
            apply_response_cache(agent, true, pool, 300, false, "embed-model".into(), cancel);
        assert!(handle.is_some(), "enabled cache must return a JoinHandle");
        drop(result);
        drop(handle);
    }

    #[tokio::test]
    async fn apply_response_cache_cleanup_spawns_without_panic() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let pool = file_pool(tmp.path()).await;
        let agent = make_agent();
        let cancel = tokio_util::sync::CancellationToken::new();
        let child = cancel.child_token();

        let (_, cleanup_handle) =
            apply_response_cache(agent, true, pool, 300, false, "embed-model".into(), child);
        let cleanup_handle = cleanup_handle.expect("enabled cache must return a JoinHandle");

        cancel.cancel();

        tokio::time::timeout(std::time::Duration::from_secs(1), cleanup_handle)
            .await
            .expect("cleanup loop did not exit within 1 s after cancellation")
            .expect("cleanup loop panicked");
    }

    #[tokio::test]
    async fn apply_code_indexer_disabled_returns_no_runtime() {
        let full_config = Config {
            index: IndexConfig {
                enabled: false,
                ..IndexConfig::default()
            },
            ..Config::default()
        };
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let pool = file_pool(tmp.path()).await;

        let (watcher, progress_rx) = apply_code_indexer(
            &full_config,
            None,
            offline_provider(),
            pool,
            false,
            None,
            None,
        )
        .await;
        assert!(watcher.is_none());
        assert!(progress_rx.is_none());
    }

    #[tokio::test]
    async fn apply_code_indexer_enabled_returns_runtime_without_watcher_when_disabled() {
        let full_config = Config {
            index: IndexConfig {
                enabled: true,
                watch: false,
                ..IndexConfig::default()
            },
            ..Config::default()
        };
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let pool = file_pool(tmp.path()).await;
        let qdrant = QdrantOps::new("http://127.0.0.1:1", None).unwrap();

        let (watcher, _progress_rx) = apply_code_indexer(
            &full_config,
            Some(qdrant),
            offline_provider(),
            pool,
            false,
            None,
            None,
        )
        .await;
        assert!(watcher.is_none());
    }

    #[tokio::test]
    async fn apply_code_indexer_workspace_root_none_uses_current_dir() {
        let full_config = Config {
            index: IndexConfig {
                enabled: false,
                workspace_root: None,
                ..IndexConfig::default()
            },
            ..Config::default()
        };
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let pool = file_pool(tmp.path()).await;

        let (watcher, _) = apply_code_indexer(
            &full_config,
            None,
            offline_provider(),
            pool,
            false,
            None,
            None,
        )
        .await;
        assert!(watcher.is_none());
    }

    #[tokio::test]
    async fn apply_code_indexer_workspace_root_some_path() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let full_config = Config {
            index: IndexConfig {
                enabled: true,
                watch: false,
                workspace_root: Some(tmp_dir.path().to_path_buf()),
                ..IndexConfig::default()
            },
            ..Config::default()
        };
        let tmp_db = tempfile::NamedTempFile::new().unwrap();
        let pool = file_pool(tmp_db.path()).await;
        let qdrant = QdrantOps::new("http://127.0.0.1:1", None).unwrap();

        let (watcher, _) = apply_code_indexer(
            &full_config,
            Some(qdrant),
            offline_provider(),
            pool,
            false,
            None,
            None,
        )
        .await;
        assert!(watcher.is_none()); // watch = false
    }

    /// `spawn_index_progress_status_forwarder` must forward each `IndexProgress` update as a
    /// human-readable status string, then send a completion message once `files_done` reaches
    /// `files_total` and stop — the next real status message (e.g. from the file watcher)
    /// overwrites it naturally, matching `IndexWatcher`'s own convention. This is the mechanism
    /// that closes the "no status visibility during the initial index pass" gap (#5720 item D)
    /// for TUI/Telegram/Discord — previously only the CLI got a one-shot `eprintln!`.
    #[tokio::test]
    async fn spawn_index_progress_status_forwarder_forwards_progress_and_completes() {
        let (progress_tx, _keepalive) =
            tokio::sync::watch::channel(zeph_index::IndexProgress::default());
        let (status_tx, mut status_rx) = tokio::sync::mpsc::unbounded_channel::<String>();

        spawn_index_progress_status_forwarder(progress_tx.subscribe(), status_tx, None);

        progress_tx
            .send(zeph_index::IndexProgress {
                files_done: 1,
                files_total: 2,
                chunks_created: 3,
            })
            .unwrap();
        assert_eq!(
            status_rx.recv().await.unwrap(),
            "Indexing repository… (1/2 files)"
        );

        progress_tx
            .send(zeph_index::IndexProgress {
                files_done: 2,
                files_total: 2,
                chunks_created: 5,
            })
            .unwrap();
        assert_eq!(
            status_rx.recv().await.unwrap(),
            "Indexing repository… (2/2 files)"
        );
        assert_eq!(status_rx.recv().await.unwrap(), "Indexing complete");

        // The forwarder stops after sending the completion message — no further sends,
        // and the channel closes once the forwarder task exits.
        assert!(status_rx.recv().await.is_none());
    }

    /// A zero-file project (`files_total == 0`) never sends any `IndexProgress` update from
    /// `index_project`'s batch loop (the `entries.chunks(..)` iterator is empty). In
    /// `spawn_background_indexer`, `progress_tx` is owned by the `fut` async block and is
    /// dropped once `index_project` returns and that block's scope ends — even having sent
    /// zero updates. This test reproduces that by dropping `progress_tx` directly: the
    /// forwarder's `progress_rx.changed()` must observe the closed channel and exit its loop
    /// cleanly, without hanging forever and without emitting a spurious completion/clear
    /// message for a project that had nothing to index.
    #[tokio::test]
    async fn spawn_index_progress_status_forwarder_exits_cleanly_for_zero_file_project() {
        let (progress_tx, _keepalive) =
            tokio::sync::watch::channel(zeph_index::IndexProgress::default());
        let (status_tx, mut status_rx) = tokio::sync::mpsc::unbounded_channel::<String>();

        spawn_index_progress_status_forwarder(progress_tx.subscribe(), status_tx, None);
        drop(progress_tx); // closes the watch channel, as if index_project finished with no updates

        assert!(
            status_rx.recv().await.is_none(),
            "with zero updates ever sent, the forwarder must exit on channel closure without \
             emitting any status message"
        );
    }

    #[tokio::test]
    async fn apply_code_retrieval_with_disabled_index_returns_agent() {
        let agent = make_agent();
        let config = IndexConfig {
            enabled: false,
            ..IndexConfig::default()
        };
        let result = apply_code_retrieval(agent, &config);
        drop(result);
    }

    #[tokio::test]
    async fn apply_code_rag_retriever_disabled_is_noop() {
        let pool = memory_pool().await;
        let agent = make_agent();
        let config = IndexConfig {
            enabled: false,
            ..IndexConfig::default()
        };
        let result = apply_code_rag_retriever(agent, &config, None, offline_provider(), pool);
        assert!(
            !result.has_code_retriever(),
            "disabled index must leave retriever None"
        );
    }

    #[tokio::test]
    async fn apply_code_rag_retriever_no_qdrant_is_noop() {
        let pool = memory_pool().await;
        let agent = make_agent();
        let config = IndexConfig {
            enabled: true,
            budget_ratio: 0.4,
            ..IndexConfig::default()
        };
        let result = apply_code_rag_retriever(agent, &config, None, offline_provider(), pool);
        assert!(
            !result.has_code_retriever(),
            "missing qdrant ops must leave retriever None"
        );
    }

    #[tokio::test]
    async fn apply_code_rag_retriever_mcp_enabled_is_noop() {
        let pool = memory_pool().await;
        let agent = make_agent();
        let config = IndexConfig {
            enabled: true,
            mcp_enabled: true,
            budget_ratio: 0.4,
            ..IndexConfig::default()
        };
        let qdrant = QdrantOps::new("http://127.0.0.1:1", None).unwrap();
        let result =
            apply_code_rag_retriever(agent, &config, Some(qdrant), offline_provider(), pool);
        assert!(
            !result.has_code_retriever(),
            "mcp_enabled must leave retriever None"
        );
    }

    // --- apply_nli_sanitizer_with_cfg (#5438) ---

    #[tokio::test]
    async fn apply_nli_sanitizer_disabled_does_not_set_metrics_flag() {
        let (tx, rx) = tokio::sync::watch::channel(zeph_core::metrics::MetricsSnapshot::default());
        let agent = make_agent().with_metrics(tx);
        let nli_config = zeph_sanitizer::nli::NliConfig {
            enabled: false,
            ..zeph_sanitizer::nli::NliConfig::default()
        };
        let result =
            apply_nli_sanitizer_with_cfg(agent, offline_provider(), None, &nli_config, None);
        drop(result);
        assert!(
            !rx.borrow().nli_enabled,
            "disabled config must not attach the NLI sanitizer"
        );
    }

    #[tokio::test]
    async fn apply_nli_sanitizer_enabled_attaches_and_sets_metrics_flag() {
        let (tx, rx) = tokio::sync::watch::channel(zeph_core::metrics::MetricsSnapshot::default());
        let agent = make_agent().with_metrics(tx);
        let nli_config = zeph_sanitizer::nli::NliConfig {
            enabled: true,
            ..zeph_sanitizer::nli::NliConfig::default()
        };
        let result =
            apply_nli_sanitizer_with_cfg(agent, offline_provider(), None, &nli_config, None);
        drop(result);
        assert!(
            rx.borrow().nli_enabled,
            "enabled config must attach the NLI sanitizer and flip the metrics flag"
        );
    }

    // --- apply_secret_masking (#5437) ---

    #[tokio::test]
    async fn apply_secret_masking_none_does_not_set_metrics_flag() {
        let (tx, rx) = tokio::sync::watch::channel(zeph_core::metrics::MetricsSnapshot::default());
        let agent = make_agent().with_metrics(tx);
        let result = apply_secret_masking(agent, None);
        drop(result);
        assert!(
            !rx.borrow().secret_masking_enabled,
            "None registry must not attach secret masking"
        );
    }

    #[tokio::test]
    async fn apply_secret_masking_some_attaches_and_sets_metrics_flag() {
        let (tx, rx) = tokio::sync::watch::channel(zeph_core::metrics::MetricsSnapshot::default());
        let agent = make_agent().with_metrics(tx);
        let registry = std::sync::Arc::new(zeph_sanitizer::secret_mask::SecretMaskRegistry::new());
        let result = apply_secret_masking(agent, Some(registry));
        drop(result);
        assert!(
            rx.borrow().secret_masking_enabled,
            "Some(registry) must attach secret masking and flip the metrics flag"
        );
    }

    // --- apply_common_tool_gating / register_mcp_tool_ids (#5611) ---

    /// Mock executor that only handles calls matching its own `tool_id`, mirroring
    /// `CompositeExecutor`'s first-match-wins dispatch (`Ok(None)` = "not mine, try next").
    #[derive(Debug)]
    struct TaggedMock(String);

    impl ToolExecutor for TaggedMock {
        async fn execute(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
            Ok(None)
        }

        async fn execute_tool_call(
            &self,
            call: &zeph_tools::ToolCall,
        ) -> Result<Option<ToolOutput>, ToolError> {
            if call.tool_id != self.0 {
                return Ok(None);
            }
            Ok(Some(ToolOutput {
                tool_name: call.tool_id.clone(),
                summary: "ok".into(),
                blocks_executed: 1,
                filter_stats: None,
                diff: None,
                streamed: false,
                terminal_id: None,
                locations: None,
                raw_response: None,
                claim_source: None,
            }))
        }
    }

    fn make_tool_call(tool_id: &str) -> zeph_tools::ToolCall {
        zeph_tools::ToolCall {
            tool_id: tool_id.into(),
            params: serde_json::Map::new(),
            caller_id: None,
            context: None,
            tool_call_id: String::new(),
            skill_name: None,
        }
    }

    fn make_test_mcp_tool(server_id: &str, name: &str) -> zeph_mcp::McpTool {
        zeph_mcp::McpTool {
            server_id: server_id.to_owned(),
            name: name.to_owned(),
            description: String::new(),
            input_schema: serde_json::Value::Null,
            output_schema: None,
            security_meta: zeph_mcp::tool::ToolSecurityMeta::default(),
        }
    }

    /// Regression test for #5611: the gate must wrap the ENTIRE composed tree — including
    /// tools that used to be composed outside it in ACP/daemon (memory, MCP) — not just the
    /// base chain. Mirrors the production shape built by `spawn_acp_agent`/`run_daemon`:
    /// a "memory_save"-like tool and an MCP-sourced tool sit alongside a readonly tool in one
    /// `CompositeExecutor` tree, gated by a single outermost `apply_common_tool_gating` call.
    #[tokio::test]
    async fn quarantine_blocks_memory_and_mcp_tools_reached_through_composed_tree() {
        let mcp_tool = make_test_mcp_tool("fs", "write_file");
        let mcp_tool_id = mcp_tool.sanitized_id();

        let inner: Arc<dyn zeph_tools::ErasedToolExecutor> =
            Arc::new(zeph_tools::CompositeExecutor::new(
                TaggedMock("memory_save".to_owned()),
                zeph_tools::CompositeExecutor::new(
                    TaggedMock(mcp_tool_id.clone()),
                    TaggedMock("read".to_owned()),
                ),
            ));
        let (gated, mcp_handle) = apply_common_tool_gating(
            zeph_tools::DynExecutor(inner),
            &zeph_tools::PermissionPolicy::default(),
        );
        register_mcp_tool_ids(&mcp_handle, std::slice::from_ref(&mcp_tool));
        gated.set_effective_trust(zeph_common::SkillTrustLevel::Quarantined);

        let memory_result = gated
            .execute_tool_call(&make_tool_call("memory_save"))
            .await;
        assert!(
            matches!(memory_result, Err(zeph_tools::ToolError::Blocked { .. })),
            "memory_save must be denied under Quarantine even when composed outside the \
             former gate boundary, got {memory_result:?}"
        );

        let mcp_result = gated.execute_tool_call(&make_tool_call(&mcp_tool_id)).await;
        assert!(
            matches!(mcp_result, Err(zeph_tools::ToolError::Blocked { .. })),
            "MCP-sourced tool must be denied under Quarantine once its id is registered, \
             got {mcp_result:?}"
        );

        let read_result = gated.execute_tool_call(&make_tool_call("read")).await;
        assert!(
            read_result.is_ok(),
            "readonly native tool must remain reachable under Quarantine, got {read_result:?}"
        );
    }

    /// Companion to the above: under `Trusted`, none of the gate's Quarantine-specific
    /// denials apply, so all three tools in the same composed tree must dispatch normally.
    #[tokio::test]
    async fn trusted_allows_memory_and_mcp_tools_through_composed_tree() {
        let mcp_tool = make_test_mcp_tool("fs", "write_file");
        let mcp_tool_id = mcp_tool.sanitized_id();

        let inner: Arc<dyn zeph_tools::ErasedToolExecutor> =
            Arc::new(zeph_tools::CompositeExecutor::new(
                TaggedMock("memory_save".to_owned()),
                zeph_tools::CompositeExecutor::new(
                    TaggedMock(mcp_tool_id.clone()),
                    TaggedMock("read".to_owned()),
                ),
            ));
        let policy =
            zeph_tools::PermissionPolicy::default().with_autonomy(zeph_tools::AutonomyLevel::Full);
        let (gated, mcp_handle) = apply_common_tool_gating(zeph_tools::DynExecutor(inner), &policy);
        register_mcp_tool_ids(&mcp_handle, std::slice::from_ref(&mcp_tool));
        gated.set_effective_trust(zeph_common::SkillTrustLevel::Trusted);

        for tool_id in ["memory_save", mcp_tool_id.as_str(), "read"] {
            let result = gated.execute_tool_call(&make_tool_call(tool_id)).await;
            assert!(
                result.is_ok(),
                "{tool_id} must dispatch normally under Trusted/Full autonomy, got {result:?}"
            );
        }
    }
}
