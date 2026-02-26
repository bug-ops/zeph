// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::path::PathBuf;
use std::sync::Arc;

use zeph_core::channel::Channel;
use zeph_core::config::Config;

type ToolExecutor = zeph_tools::CompositeExecutor<
    zeph_tools::CompositeExecutor<
        zeph_tools::FileExecutor,
        zeph_tools::CompositeExecutor<zeph_tools::ShellExecutor, zeph_tools::WebScrapeExecutor>,
    >,
    zeph_mcp::McpToolExecutor,
>;

pub(crate) struct ToolSetup {
    pub(crate) executor: ToolExecutor,
    pub(crate) mcp_tools: Vec<zeph_mcp::McpTool>,
    pub(crate) mcp_manager: Arc<zeph_mcp::McpManager>,
    pub(crate) mcp_shared_tools: Arc<std::sync::RwLock<Vec<zeph_mcp::McpTool>>>,
    pub(crate) tool_event_rx: Option<tokio::sync::mpsc::UnboundedReceiver<zeph_tools::ToolEvent>>,
}

pub(crate) async fn build_tool_setup(
    config: &Config,
    permission_policy: zeph_tools::PermissionPolicy,
    with_tool_events: bool,
) -> ToolSetup {
    let filter_registry = if config.tools.filters.enabled {
        zeph_tools::OutputFilterRegistry::default_filters(&config.tools.filters)
    } else {
        zeph_tools::OutputFilterRegistry::new(false)
    };
    let mut shell_executor = zeph_tools::ShellExecutor::new(&config.tools.shell)
        .with_permissions(permission_policy)
        .with_output_filters(filter_registry);
    if config.tools.audit.enabled
        && let Ok(logger) = zeph_tools::AuditLogger::from_config(&config.tools.audit).await
    {
        shell_executor = shell_executor.with_audit(logger);
    }

    let tool_event_rx = if with_tool_events {
        let (tool_tx, tool_rx) = tokio::sync::mpsc::unbounded_channel::<zeph_tools::ToolEvent>();
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

    let mcp_manager = Arc::new(zeph_core::bootstrap::create_mcp_manager(config));
    let mcp_tools = mcp_manager.connect_all().await;
    tracing::info!("discovered {} MCP tool(s)", mcp_tools.len());

    let mcp_shared_tools = Arc::new(std::sync::RwLock::new(mcp_tools.clone()));
    let mcp_executor =
        zeph_mcp::McpToolExecutor::new(mcp_manager.clone(), mcp_shared_tools.clone());
    let base_executor = zeph_tools::CompositeExecutor::new(
        file_executor,
        zeph_tools::CompositeExecutor::new(shell_executor, scrape_executor),
    );
    let executor = zeph_tools::CompositeExecutor::new(base_executor, mcp_executor);

    ToolSetup {
        executor,
        mcp_tools,
        mcp_manager,
        mcp_shared_tools,
        tool_event_rx,
    }
}

use zeph_core::agent::Agent;
#[cfg(feature = "index")]
use zeph_core::config::IndexConfig;
use zeph_core::cost::CostTracker;
#[cfg(feature = "index")]
use zeph_index::{
    indexer::{CodeIndexer, IndexerConfig},
    retriever::{CodeRetriever, RetrievalConfig},
    store::CodeStore,
    watcher::IndexWatcher,
};

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
    pool: sqlx::SqlitePool,
    ttl_secs: u64,
) -> Agent<C> {
    if !enabled {
        return agent;
    }
    let cache = std::sync::Arc::new(zeph_memory::ResponseCache::new(pool, ttl_secs));
    let cache_clone = std::sync::Arc::clone(&cache);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(3600));
        interval.tick().await; // skip immediate first tick
        loop {
            interval.tick().await;
            match cache_clone.cleanup_expired().await {
                Ok(n) if n > 0 => tracing::debug!("cleaned up {n} expired cache entries"),
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

#[cfg(feature = "index")]
pub(crate) async fn apply_code_index<C: Channel>(
    agent: Agent<C>,
    config: &IndexConfig,
    qdrant_url: &str,
    provider: zeph_llm::any::AnyProvider,
    pool: sqlx::SqlitePool,
    provider_has_tools: bool,
) -> (Agent<C>, Option<IndexWatcher>) {
    if !config.enabled || provider_has_tools {
        if config.enabled && provider_has_tools {
            tracing::info!("code index skipped: provider supports native tool_use");
        }
        return (agent, None);
    }

    let init = async {
        let store = CodeStore::new(qdrant_url, pool)?;
        let provider_arc = std::sync::Arc::new(provider);
        let retrieval_config = RetrievalConfig {
            max_chunks: config.max_chunks,
            score_threshold: config.score_threshold,
            budget_ratio: config.budget_ratio,
        };
        let retriever = CodeRetriever::new(store.clone(), provider_arc.clone(), retrieval_config);
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
            let watcher = if config.watch {
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
            let agent = agent.with_code_retriever(
                std::sync::Arc::new(retriever),
                config.repo_map_tokens,
                config.repo_map_ttl_secs,
            );
            (agent, watcher)
        }
        Err(e) => {
            tracing::warn!("code index initialization failed: {e:#}");
            (agent, None)
        }
    }
}

#[cfg(feature = "candle")]
pub(crate) fn apply_candle_stt<C: Channel>(
    agent: zeph_core::agent::Agent<C>,
    stt_config: Option<&zeph_core::config::SttConfig>,
) -> zeph_core::agent::Agent<C> {
    if !stt_config.is_some_and(|s| s.provider == "candle-whisper") {
        return agent;
    }
    let model = stt_config.map_or("openai/whisper-tiny", |s| s.model.as_str());
    let language = stt_config.map_or("auto", |s| s.language.as_str());
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

#[cfg(feature = "stt")]
pub(crate) fn apply_whisper_stt<C: Channel>(
    agent: zeph_core::agent::Agent<C>,
    stt_config: Option<&zeph_core::config::SttConfig>,
    openai_base_url: &str,
    api_key: String,
) -> zeph_core::agent::Agent<C> {
    let Some(stt_cfg) = stt_config else {
        return agent;
    };
    if stt_cfg.provider == "candle-whisper" {
        return agent;
    }
    let base_url = stt_cfg.base_url.as_deref().unwrap_or(openai_base_url);
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

    #[test]
    fn apply_summary_provider_none_returns_agent_unchanged() {
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
        let pool = sqlx::SqlitePool::connect(&db_url).await.unwrap();
        let agent = make_agent();
        let result = apply_response_cache(agent, false, pool, 300);
        drop(result);
    }

    #[tokio::test]
    async fn apply_response_cache_enabled_attaches_cache() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let db_url = format!("sqlite:{}", tmp.path().display());
        let pool = sqlx::SqlitePool::connect(&db_url).await.unwrap();
        let agent = make_agent();
        let result = apply_response_cache(agent, true, pool, 300);
        drop(result);
    }
}
