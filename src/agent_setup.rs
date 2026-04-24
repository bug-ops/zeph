// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use parking_lot::RwLock;

use zeph_core::RuntimeContext;
use zeph_core::channel::Channel;
use zeph_core::config::Config;
use zeph_tools::{
    LspSearchBackend, SearchCodeExecutor, SearchCodeHit, SearchCodeSource, SemanticSearchBackend,
};

type ToolExecutor = zeph_tools::CompositeExecutor<
    zeph_tools::CompositeExecutor<
        zeph_tools::FileExecutor,
        zeph_tools::CompositeExecutor<
            zeph_tools::ShellExecutor,
            zeph_tools::CompositeExecutor<
                zeph_tools::WebScrapeExecutor,
                zeph_tools::SetCwdExecutor,
            >,
        >,
    >,
    zeph_mcp::McpToolExecutor,
>;

pub(crate) struct ToolSetup {
    pub(crate) executor: ToolExecutor,
    pub(crate) mcp_tools: Vec<zeph_mcp::McpTool>,
    pub(crate) mcp_outcomes: Vec<zeph_mcp::ServerConnectOutcome>,
    pub(crate) mcp_manager: Arc<zeph_mcp::McpManager>,
    pub(crate) mcp_shared_tools: Arc<RwLock<Vec<zeph_mcp::McpTool>>>,
    pub(crate) tool_event_rx: Option<tokio::sync::mpsc::UnboundedReceiver<zeph_tools::ToolEvent>>,
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
            let vector = self.provider.embed(query).await.map_err(|e| {
                zeph_tools::ToolError::Execution(std::io::Error::other(e.to_string()))
            })?;
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
        .find_map(|content| match &content.raw {
            rmcp::model::RawContent::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
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
    age_vault: Option<&Arc<tokio::sync::RwLock<zeph_core::vault::AgeVaultProvider>>>,
    status_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    pool: Option<&zeph_db::DbPool>,
    provider: &zeph_llm::any::AnyProvider,
) -> ToolSetup {
    let filter_registry = if config.tools.filters.enabled {
        zeph_tools::OutputFilterRegistry::default_filters(&config.tools.filters)
    } else {
        zeph_tools::OutputFilterRegistry::new(false)
    };
    let mut shell_executor = zeph_tools::ShellExecutor::new(&config.tools.shell)
        .with_permissions(permission_policy)
        .with_output_filters(filter_registry);
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
        let (tool_tx, tool_rx) = tokio::sync::mpsc::unbounded_channel::<zeph_tools::ToolEvent>();
        shell_executor = shell_executor.with_tool_event_tx(tool_tx);
        Some(tool_rx)
    } else {
        None
    };
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
        tokio::spawn(drain_embedding_guard_events(rx));
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
        mcp_manager.spawn_refresh_task();
    }

    let mcp_shared_tools = Arc::new(RwLock::new(mcp_tools.clone()));
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
    let executor = zeph_tools::CompositeExecutor::new(base_executor, mcp_executor);

    ToolSetup {
        executor,
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
    }
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
) {
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
}

pub(crate) fn apply_response_cache<C: Channel>(
    agent: Agent<C>,
    enabled: bool,
    pool: zeph_db::DbPool,
    ttl_secs: u64,
    semantic_cache_enabled: bool,
    embed_model: String,
) -> Agent<C> {
    if !enabled {
        if semantic_cache_enabled {
            tracing::warn!("semantic_cache_enabled has no effect without response_cache_enabled");
        }
        return agent;
    }
    let cache = std::sync::Arc::new(zeph_memory::ResponseCache::new(pool, ttl_secs));
    let cache_clone = std::sync::Arc::clone(&cache);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_hours(1));
        interval.tick().await; // skip immediate first tick
        loop {
            interval.tick().await;
            match cache_clone.cleanup(&embed_model).await {
                Ok(n) if n > 0 => tracing::debug!("cleaned up {n} cache entries"),
                Ok(_) => {}
                Err(e) => tracing::warn!("response cache cleanup failed: {e:#}"),
            }
        }
    });
    agent.with_response_cache(cache)
}

pub(crate) fn apply_cost_tracker<C: Channel>(
    agent: Agent<C>,
    enabled: bool,
    max_daily_cents: u32,
) -> Agent<C> {
    if !enabled {
        return agent;
    }
    agent.with_cost_tracker(CostTracker::new(true, f64::from(max_daily_cents)))
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
    if !config.classifiers.enabled || !config.security.pii_filter.enabled {
        return agent;
    }
    let mut ner_classifier =
        zeph_llm::classifier::ner::CandleNerClassifier::new(config.classifiers.pii_model.as_str());
    if let Some(token) = &config.classifiers.hf_token {
        ner_classifier = ner_classifier.with_hf_token(token.as_str());
    }
    let backend = std::sync::Arc::new(ner_classifier);
    tracing::info!(
        repo_id = %config.classifiers.pii_model,
        "NER PII classifier attached for union merge pipeline (model loads lazily on first use)"
    );
    agent.with_pii_ner_classifier(
        backend,
        config.classifiers.timeout_ms,
        config.classifiers.pii_ner_max_chars,
        config.classifiers.pii_ner_circuit_breaker,
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
) -> zeph_core::agent::Agent<C> {
    apply_causal_analyzer_with_cfg(agent, provider, &config.security.causal_ipi)
}

/// Wire the `TurnCausalAnalyzer` into the agent's security config (takes `CausalIpiConfig` directly).
pub(crate) fn apply_causal_analyzer_with_cfg<C: Channel>(
    agent: zeph_core::agent::Agent<C>,
    provider: zeph_llm::any::AnyProvider,
    causal_config: &zeph_sanitizer::causal_ipi::CausalIpiConfig,
) -> zeph_core::agent::Agent<C> {
    if !causal_config.enabled {
        return agent;
    }
    let analyzer = zeph_sanitizer::causal_ipi::TurnCausalAnalyzer::new(provider, causal_config);
    tracing::info!(
        threshold = causal_config.threshold,
        probe_timeout_ms = causal_config.probe_timeout_ms,
        "causal IPI analyzer attached"
    );
    agent.with_causal_analyzer(analyzer)
}

pub(crate) async fn apply_code_indexer(
    config: &IndexConfig,
    qdrant_ops: Option<QdrantOps>,
    provider: zeph_llm::any::AnyProvider,
    pool: zeph_db::DbPool,
    cli_mode: bool,
    status_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    supervisor: Option<zeph_core::TaskSupervisor>,
) -> CodeIndexerSetup {
    if !config.enabled {
        return (None, None);
    }

    let init = async {
        let ops = qdrant_ops.ok_or_else(|| {
            anyhow::anyhow!("code index requires Qdrant backend (vector_backend = \"qdrant\")")
        })?;
        let store = CodeStore::with_ops(ops, pool);
        let provider_arc = std::sync::Arc::new(provider);
        let base_indexer = CodeIndexer::new(
            store,
            provider_arc,
            IndexerConfig {
                concurrency: config.concurrency,
                batch_size: config.batch_size,
                memory_batch_size: config.memory_batch_size,
                max_file_bytes: config.max_file_bytes,
                embed_concurrency: config.embed_concurrency,
                ..IndexerConfig::default()
            },
        );
        let base_indexer = if let Some(ref sup) = supervisor {
            base_indexer.with_spawner(std::sync::Arc::new(sup.clone())
                as std::sync::Arc<dyn zeph_common::BlockingSpawner>)
        } else {
            base_indexer
        };
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
                supervisor,
            );
            tracing::info!("code indexer started");
            let watcher = start_index_watcher(config.watch, &workspace_root, indexer, status_tx);
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
    supervisor: Option<zeph_core::TaskSupervisor>,
) {
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
        sup.spawn(zeph_core::TaskDescriptor {
            name: "index_project",
            restart: zeph_core::RestartPolicy::RunOnce,
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
        tokio::spawn(fut);
    }
}

fn start_index_watcher(
    watch: bool,
    root: &std::path::Path,
    indexer: std::sync::Arc<CodeIndexer>,
    status_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
) -> Option<IndexWatcher> {
    if !watch {
        return None;
    }
    match IndexWatcher::start(root, indexer, status_tx) {
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
    let retrieval_config = zeph_index::retriever::RetrievalConfig {
        max_chunks: config.max_chunks,
        score_threshold: config.score_threshold,
        budget_ratio: config.budget_ratio,
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
        ToolDiscoveryStrategyConfig::None => ToolDiscoveryStrategy::None,
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

/// Build a `SandboxPolicy` from the TOML `[tools.sandbox]` config section.
pub(crate) fn sandbox_policy_from_config(
    cfg: &zeph_tools::config::SandboxConfig,
) -> zeph_tools::sandbox::SandboxPolicy {
    use zeph_tools::sandbox::SandboxPolicy;
    SandboxPolicy {
        profile: cfg.profile,
        allow_read: cfg.allow_read.clone(),
        allow_write: cfg.allow_write.clone(),
        allow_network: cfg.profile == zeph_tools::sandbox::SandboxProfile::NetworkAllowAll,
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
    use zeph_tools::executor::{ToolError, ToolOutput};

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

    fn make_agent() -> Agent<CliChannel> {
        let config = Config::load(Path::new("/nonexistent")).unwrap();
        let registry = SkillRegistry::load(&[] as &[std::path::PathBuf]);
        Agent::new(
            offline_provider(),
            CliChannel::new(),
            registry,
            None,
            config.skills.max_active_skills,
            NoopExec,
        )
    }

    #[test]
    fn apply_cost_tracker_disabled_returns_agent_unchanged() {
        let agent = make_agent();
        let result = apply_cost_tracker(agent, false, 100);
        drop(result);
    }

    #[test]
    fn apply_cost_tracker_enabled_attaches_tracker() {
        let agent = make_agent();
        let result = apply_cost_tracker(agent, true, 500);
        drop(result);
    }

    #[tokio::test]
    async fn apply_summary_provider_none_returns_agent_unchanged() {
        let agent = make_agent();
        let result = apply_summary_provider(agent, None);
        drop(result);
    }

    #[test]
    fn apply_summary_provider_some_attaches_provider() {
        let agent = make_agent();
        let sp = offline_provider();
        let result = apply_summary_provider(agent, Some(sp));
        drop(result);
    }

    #[tokio::test]
    async fn apply_response_cache_disabled_returns_agent_unchanged() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let db_url = format!("sqlite:{}", tmp.path().display());
        let pool = zeph_db::sqlx::SqlitePool::connect(&db_url).await.unwrap();
        let agent = make_agent();
        let result = apply_response_cache(agent, false, pool, 300, false, "embed-model".into());
        drop(result);
    }

    #[tokio::test]
    async fn apply_response_cache_enabled_attaches_cache() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let db_url = format!("sqlite:{}", tmp.path().display());
        let pool = zeph_db::sqlx::SqlitePool::connect(&db_url).await.unwrap();
        let agent = make_agent();
        let result = apply_response_cache(agent, true, pool, 300, false, "embed-model".into());
        drop(result);
    }

    #[tokio::test]
    async fn apply_code_indexer_disabled_returns_no_runtime() {
        let config = IndexConfig {
            enabled: false,
            ..IndexConfig::default()
        };
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let db_url = format!("sqlite:{}", tmp.path().display());
        let pool = zeph_db::sqlx::SqlitePool::connect(&db_url).await.unwrap();

        let (watcher, progress_rx) =
            apply_code_indexer(&config, None, offline_provider(), pool, false, None, None).await;
        assert!(watcher.is_none());
        assert!(progress_rx.is_none());
    }

    #[tokio::test]
    async fn apply_code_indexer_enabled_returns_runtime_without_watcher_when_disabled() {
        let config = IndexConfig {
            enabled: true,
            watch: false,
            ..IndexConfig::default()
        };
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let db_url = format!("sqlite:{}", tmp.path().display());
        let pool = zeph_db::sqlx::SqlitePool::connect(&db_url).await.unwrap();
        let qdrant = QdrantOps::new("http://127.0.0.1:1").unwrap();

        let (watcher, _progress_rx) = apply_code_indexer(
            &config,
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
        let config = IndexConfig {
            enabled: false,
            workspace_root: None,
            ..IndexConfig::default()
        };
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let db_url = format!("sqlite:{}", tmp.path().display());
        let pool = zeph_db::sqlx::SqlitePool::connect(&db_url).await.unwrap();

        let (watcher, _) =
            apply_code_indexer(&config, None, offline_provider(), pool, false, None, None).await;
        assert!(watcher.is_none());
    }

    #[tokio::test]
    async fn apply_code_indexer_workspace_root_some_path() {
        let tmp_dir = tempfile::tempdir().unwrap();
        let config = IndexConfig {
            enabled: true,
            watch: false,
            workspace_root: Some(tmp_dir.path().to_path_buf()),
            ..IndexConfig::default()
        };
        let tmp_db = tempfile::NamedTempFile::new().unwrap();
        let db_url = format!("sqlite:{}", tmp_db.path().display());
        let pool = zeph_db::sqlx::SqlitePool::connect(&db_url).await.unwrap();
        let qdrant = QdrantOps::new("http://127.0.0.1:1").unwrap();

        let (watcher, _) = apply_code_indexer(
            &config,
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

    #[test]
    fn apply_code_retrieval_with_disabled_index_returns_agent() {
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
        let pool = zeph_db::sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .unwrap();
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
        let pool = zeph_db::sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .unwrap();
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
        let pool = zeph_db::sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .unwrap();
        let agent = make_agent();
        let config = IndexConfig {
            enabled: true,
            mcp_enabled: true,
            budget_ratio: 0.4,
            ..IndexConfig::default()
        };
        let qdrant = QdrantOps::new("http://127.0.0.1:1").unwrap();
        let result =
            apply_code_rag_retriever(agent, &config, Some(qdrant), offline_provider(), pool);
        assert!(
            !result.has_code_retriever(),
            "mcp_enabled must leave retriever None"
        );
    }
}
