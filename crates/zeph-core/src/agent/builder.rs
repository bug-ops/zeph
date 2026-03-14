// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::{Notify, mpsc, watch};
use zeph_llm::any::AnyProvider;
use zeph_llm::provider::LlmProvider;

use super::Agent;
use crate::channel::Channel;
use crate::config::{
    CompressionConfig, LearningConfig, RoutingConfig, SecurityConfig, TimeoutConfig,
};
use crate::config_watcher::ConfigEvent;
use crate::context::ContextBudget;
use crate::cost::CostTracker;
use crate::instructions::{InstructionEvent, InstructionReloadState};
use crate::metrics::MetricsSnapshot;
use zeph_memory::semantic::SemanticMemory;
use zeph_skills::watcher::SkillEvent;

impl<C: Channel> Agent<C> {
    #[must_use]
    pub fn with_autosave_config(mut self, autosave_assistant: bool, min_length: usize) -> Self {
        self.memory_state.autosave_assistant = autosave_assistant;
        self.memory_state.autosave_min_length = min_length;
        self
    }

    #[must_use]
    pub fn with_tool_call_cutoff(mut self, cutoff: usize) -> Self {
        self.memory_state.tool_call_cutoff = cutoff;
        self
    }

    #[must_use]
    pub fn with_response_cache(
        mut self,
        cache: std::sync::Arc<zeph_memory::ResponseCache>,
    ) -> Self {
        self.response_cache = Some(cache);
        self
    }

    /// Set the parent tool call ID for subagent sessions.
    ///
    /// When set, every `LoopbackEvent::ToolStart` and `LoopbackEvent::ToolOutput` emitted
    /// by this agent will carry the `parent_tool_use_id` so the IDE can build a subagent
    /// hierarchy tree.
    #[must_use]
    pub fn with_parent_tool_use_id(mut self, id: impl Into<String>) -> Self {
        self.parent_tool_use_id = Some(id.into());
        self
    }

    #[must_use]
    pub fn with_stt(mut self, stt: Box<dyn zeph_llm::stt::SpeechToText>) -> Self {
        self.providers.stt = Some(stt);
        self
    }

    /// Enable debug dump mode, writing LLM requests/responses and raw tool output to `dumper`.
    #[must_use]
    pub fn with_debug_dumper(mut self, dumper: crate::debug_dump::DebugDumper) -> Self {
        self.debug_state.debug_dumper = Some(dumper);
        self
    }

    /// Enable `OTel` trace collection. The collector writes `trace.json` at session end.
    #[must_use]
    pub fn with_trace_collector(
        mut self,
        collector: crate::debug_dump::trace::TracingCollector,
    ) -> Self {
        self.debug_state.trace_collector = Some(collector);
        self
    }

    /// Store trace config so `/dump-format trace` can create a `TracingCollector` at runtime (CR-04).
    #[must_use]
    pub fn with_trace_config(
        mut self,
        dump_dir: std::path::PathBuf,
        service_name: impl Into<String>,
        redact: bool,
    ) -> Self {
        self.debug_state.dump_dir = Some(dump_dir);
        self.debug_state.trace_service_name = service_name.into();
        self.debug_state.trace_redact = redact;
        self
    }

    /// Enable LSP context injection hooks (diagnostics-on-save, hover-on-read).
    #[cfg(feature = "lsp-context")]
    #[must_use]
    pub fn with_lsp_hooks(mut self, runner: crate::lsp_hooks::LspHookRunner) -> Self {
        self.lsp_hooks = Some(runner);
        self
    }

    #[must_use]
    pub fn with_update_notifications(mut self, rx: mpsc::Receiver<String>) -> Self {
        self.lifecycle.update_notify_rx = Some(rx);
        self
    }

    #[must_use]
    pub fn with_custom_task_rx(mut self, rx: mpsc::Receiver<String>) -> Self {
        self.lifecycle.custom_task_rx = Some(rx);
        self
    }

    /// Wrap the current tool executor with an additional executor via `CompositeExecutor`.
    #[must_use]
    pub fn add_tool_executor(
        mut self,
        extra: impl zeph_tools::executor::ToolExecutor + 'static,
    ) -> Self {
        let existing = Arc::clone(&self.tool_executor);
        let combined = zeph_tools::CompositeExecutor::new(zeph_tools::DynExecutor(existing), extra);
        self.tool_executor = Arc::new(combined);
        self
    }

    #[must_use]
    pub fn with_max_tool_iterations(mut self, max: usize) -> Self {
        self.tool_orchestrator.max_iterations = max;
        self
    }

    /// Set the maximum number of retry attempts for transient tool errors (0 = disabled, max 5).
    #[must_use]
    pub fn with_max_tool_retries(mut self, max: usize) -> Self {
        self.tool_orchestrator.max_tool_retries = max.min(5);
        self
    }

    /// Set the maximum wall-clock budget (seconds) for retries per tool call (0 = unlimited).
    #[must_use]
    pub fn with_max_retry_duration_secs(mut self, secs: u64) -> Self {
        self.tool_orchestrator.max_retry_duration_secs = secs;
        self
    }

    /// Set the repeat-detection threshold (0 = disabled).
    /// Window size is `2 * threshold`.
    #[must_use]
    pub fn with_tool_repeat_threshold(mut self, threshold: usize) -> Self {
        self.tool_orchestrator.repeat_threshold = threshold;
        self.tool_orchestrator.recent_tool_calls = VecDeque::with_capacity(2 * threshold.max(1));
        self
    }

    #[must_use]
    pub fn with_memory(
        mut self,
        memory: Arc<SemanticMemory>,
        conversation_id: zeph_memory::ConversationId,
        history_limit: u32,
        recall_limit: usize,
        summarization_threshold: usize,
    ) -> Self {
        self.memory_state.memory = Some(memory);
        self.memory_state.conversation_id = Some(conversation_id);
        self.memory_state.history_limit = history_limit;
        self.memory_state.recall_limit = recall_limit;
        self.memory_state.summarization_threshold = summarization_threshold;
        self.update_metrics(|m| {
            m.qdrant_available = false;
            m.sqlite_conversation_id = Some(conversation_id);
        });
        self
    }

    #[must_use]
    pub fn with_embedding_model(mut self, model: String) -> Self {
        self.skill_state.embedding_model = model;
        self
    }

    #[must_use]
    pub fn with_disambiguation_threshold(mut self, threshold: f32) -> Self {
        self.skill_state.disambiguation_threshold = threshold;
        self
    }

    #[must_use]
    pub fn with_skill_prompt_mode(mut self, mode: crate::config::SkillPromptMode) -> Self {
        self.skill_state.prompt_mode = mode;
        self
    }

    #[must_use]
    pub fn with_document_config(mut self, config: crate::config::DocumentConfig) -> Self {
        self.memory_state.document_config = config;
        self
    }

    #[must_use]
    pub fn with_graph_config(mut self, config: crate::config::GraphConfig) -> Self {
        // R-IMP-03: graph extraction writes raw entity names/relations extracted by the LLM.
        // No PII redaction is applied on the graph write path (pre-1.0 MVP limitation).
        if config.enabled {
            tracing::warn!(
                "graph-memory is enabled: extracted entities are stored without PII redaction. \
                 Do not use with sensitive personal data until redaction is implemented."
            );
        }
        self.memory_state.graph_config = config;
        self
    }

    #[must_use]
    pub fn with_anomaly_detector(mut self, detector: zeph_tools::AnomalyDetector) -> Self {
        self.debug_state.anomaly_detector = Some(detector);
        self
    }

    #[must_use]
    pub fn with_instruction_blocks(
        mut self,
        blocks: Vec<crate::instructions::InstructionBlock>,
    ) -> Self {
        self.instruction_blocks = blocks;
        self
    }

    #[must_use]
    pub fn with_instruction_reload(
        mut self,
        rx: mpsc::Receiver<InstructionEvent>,
        state: InstructionReloadState,
    ) -> Self {
        self.instruction_reload_rx = Some(rx);
        self.instruction_reload_state = Some(state);
        self
    }

    #[must_use]
    pub fn with_shutdown(mut self, rx: watch::Receiver<bool>) -> Self {
        self.lifecycle.shutdown = rx;
        self
    }

    #[must_use]
    pub fn with_skill_reload(
        mut self,
        paths: Vec<PathBuf>,
        rx: mpsc::Receiver<SkillEvent>,
    ) -> Self {
        self.skill_state.skill_paths = paths;
        self.skill_state.skill_reload_rx = Some(rx);
        self
    }

    #[must_use]
    pub fn with_managed_skills_dir(mut self, dir: PathBuf) -> Self {
        self.skill_state.managed_dir = Some(dir);
        self
    }

    #[must_use]
    pub fn with_trust_config(mut self, config: crate::config::TrustConfig) -> Self {
        self.skill_state.trust_config = config;
        self
    }

    #[must_use]
    pub fn with_config_reload(mut self, path: PathBuf, rx: mpsc::Receiver<ConfigEvent>) -> Self {
        self.lifecycle.config_path = Some(path);
        self.lifecycle.config_reload_rx = Some(rx);
        self
    }

    #[must_use]
    pub fn with_logging_config(mut self, logging: crate::config::LoggingConfig) -> Self {
        self.debug_state.logging_config = logging;
        self
    }

    #[must_use]
    pub fn with_available_secrets(
        mut self,
        secrets: impl IntoIterator<Item = (String, crate::vault::Secret)>,
    ) -> Self {
        self.skill_state.available_custom_secrets = secrets.into_iter().collect();
        self
    }

    /// # Panics
    ///
    /// Panics if the registry `RwLock` is poisoned.
    #[must_use]
    pub fn with_hybrid_search(mut self, enabled: bool) -> Self {
        self.skill_state.hybrid_search = enabled;
        if enabled {
            let reg = self
                .skill_state
                .registry
                .read()
                .expect("registry read lock");
            let all_meta = reg.all_meta();
            let descs: Vec<&str> = all_meta.iter().map(|m| m.description.as_str()).collect();
            self.skill_state.bm25_index = Some(zeph_skills::bm25::Bm25Index::build(&descs));
        }
        self
    }

    #[must_use]
    pub fn with_learning(mut self, config: LearningConfig) -> Self {
        if config.correction_detection {
            self.feedback_detector = super::feedback_detector::FeedbackDetector::new(
                config.correction_confidence_threshold,
            );
            if config.detector_mode == crate::config::DetectorMode::Judge {
                self.judge_detector = Some(super::feedback_detector::JudgeDetector::new(
                    config.judge_adaptive_low,
                    config.judge_adaptive_high,
                ));
            }
        }
        self.learning_engine.config = Some(config);
        self
    }

    #[must_use]
    pub fn with_judge_provider(mut self, provider: AnyProvider) -> Self {
        self.providers.judge_provider = Some(provider);
        self
    }

    /// Enable server-side compaction mode (Claude compact-2026-01-12 beta).
    ///
    /// When active, client-side reactive and proactive compaction are skipped.
    #[must_use]
    pub fn with_server_compaction(mut self, enabled: bool) -> Self {
        self.providers.server_compaction_active = enabled;
        self
    }

    #[must_use]
    pub fn with_mcp(
        mut self,
        tools: Vec<zeph_mcp::McpTool>,
        registry: Option<zeph_mcp::McpToolRegistry>,
        manager: Option<std::sync::Arc<zeph_mcp::McpManager>>,
        mcp_config: &crate::config::McpConfig,
    ) -> Self {
        self.mcp.tools = tools;
        self.mcp.registry = registry;
        self.mcp.manager = manager;
        self.mcp
            .allowed_commands
            .clone_from(&mcp_config.allowed_commands);
        self.mcp.max_dynamic = mcp_config.max_dynamic_servers;
        self
    }

    #[must_use]
    pub fn with_mcp_shared_tools(
        mut self,
        shared: std::sync::Arc<std::sync::RwLock<Vec<zeph_mcp::McpTool>>>,
    ) -> Self {
        self.mcp.shared_tools = Some(shared);
        self
    }

    /// Set the watch receiver for MCP tool list updates from `tools/list_changed` notifications.
    ///
    /// The agent polls this receiver at the start of each turn to pick up refreshed tool lists.
    #[must_use]
    pub fn with_mcp_tool_rx(
        mut self,
        rx: tokio::sync::watch::Receiver<Vec<zeph_mcp::McpTool>>,
    ) -> Self {
        self.mcp.tool_rx = Some(rx);
        self
    }

    #[must_use]
    pub fn with_security(mut self, security: SecurityConfig, timeouts: TimeoutConfig) -> Self {
        self.security.sanitizer =
            crate::sanitizer::ContentSanitizer::new(&security.content_isolation);
        self.security.exfiltration_guard = crate::sanitizer::exfiltration::ExfiltrationGuard::new(
            security.exfiltration_guard.clone(),
        );
        self.security.pii_filter =
            crate::sanitizer::pii::PiiFilter::new(security.pii_filter.clone());
        self.security.memory_validator =
            crate::sanitizer::memory_validation::MemoryWriteValidator::new(
                security.memory_validation.clone(),
            );
        self.rate_limiter =
            crate::agent::rate_limiter::ToolRateLimiter::new(security.rate_limit.clone());
        self.runtime.security = security;
        self.runtime.timeouts = timeouts;
        self
    }

    #[must_use]
    pub fn with_redact_credentials(mut self, enabled: bool) -> Self {
        self.runtime.redact_credentials = enabled;
        self
    }

    #[must_use]
    pub fn with_tool_summarization(mut self, enabled: bool) -> Self {
        self.tool_orchestrator.summarize_tool_output_enabled = enabled;
        self
    }

    #[must_use]
    pub fn with_overflow_config(mut self, config: zeph_tools::OverflowConfig) -> Self {
        self.tool_orchestrator.overflow_config = config;
        self
    }

    #[must_use]
    pub fn with_summary_provider(mut self, provider: AnyProvider) -> Self {
        self.providers.summary_provider = Some(provider);
        self
    }

    #[must_use]
    pub fn with_quarantine_summarizer(
        mut self,
        qs: crate::sanitizer::quarantine::QuarantinedSummarizer,
    ) -> Self {
        self.security.quarantine_summarizer = Some(qs);
        self
    }

    pub(super) fn summary_or_primary_provider(&self) -> &AnyProvider {
        self.providers
            .summary_provider
            .as_ref()
            .unwrap_or(&self.provider)
    }

    /// Extract the last assistant message, truncated to 500 chars, for the judge prompt.
    pub(super) fn last_assistant_response(&self) -> String {
        self.messages
            .iter()
            .rev()
            .find(|m| m.role == zeph_llm::provider::Role::Assistant)
            .map(|m| super::context::truncate_chars(&m.content, 500))
            .unwrap_or_default()
    }

    #[must_use]
    pub fn with_permission_policy(mut self, policy: zeph_tools::PermissionPolicy) -> Self {
        self.runtime.permission_policy = policy;
        self
    }

    #[must_use]
    pub fn with_context_budget(
        mut self,
        budget_tokens: usize,
        reserve_ratio: f32,
        hard_compaction_threshold: f32,
        compaction_preserve_tail: usize,
        prune_protect_tokens: usize,
    ) -> Self {
        if budget_tokens > 0 {
            self.context_manager.budget = Some(ContextBudget::new(budget_tokens, reserve_ratio));
        }
        self.context_manager.hard_compaction_threshold = hard_compaction_threshold;
        self.context_manager.compaction_preserve_tail = compaction_preserve_tail;
        self.context_manager.prune_protect_tokens = prune_protect_tokens;
        self
    }

    #[must_use]
    pub fn with_soft_compaction_threshold(mut self, threshold: f32) -> Self {
        self.context_manager.soft_compaction_threshold = threshold;
        self
    }

    /// Sets the number of turns to skip compaction after a successful compaction.
    ///
    /// Prevents the compaction loop from re-triggering immediately when the
    /// summary itself is large. A value of `0` disables the cooldown.
    #[must_use]
    pub fn with_compaction_cooldown(mut self, cooldown_turns: u8) -> Self {
        self.context_manager.compaction_cooldown_turns = cooldown_turns;
        self
    }

    #[must_use]
    pub fn with_compression(mut self, compression: CompressionConfig) -> Self {
        self.context_manager.compression = compression;
        self
    }

    #[must_use]
    pub fn with_routing(mut self, routing: RoutingConfig) -> Self {
        self.context_manager.routing = routing;
        self
    }

    #[must_use]
    pub fn with_model_name(mut self, name: impl Into<String>) -> Self {
        self.runtime.model_name = name.into();
        self
    }

    #[must_use]
    pub fn with_working_dir(mut self, path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        self.env_context =
            crate::context::EnvironmentContext::gather_for_dir(&self.runtime.model_name, &path);
        self
    }

    #[must_use]
    pub fn with_warmup_ready(mut self, rx: watch::Receiver<bool>) -> Self {
        self.lifecycle.warmup_ready = Some(rx);
        self
    }

    #[must_use]
    pub fn with_cost_tracker(mut self, tracker: CostTracker) -> Self {
        self.metrics.cost_tracker = Some(tracker);
        self
    }

    #[must_use]
    pub fn with_extended_context(mut self, enabled: bool) -> Self {
        self.metrics.extended_context = enabled;
        self
    }

    #[must_use]
    pub fn with_repo_map(mut self, token_budget: usize, ttl_secs: u64) -> Self {
        self.index.repo_map_tokens = token_budget;
        self.index.repo_map_ttl = std::time::Duration::from_secs(ttl_secs);
        self
    }

    #[must_use]
    pub fn with_code_retriever(
        mut self,
        retriever: std::sync::Arc<zeph_index::retriever::CodeRetriever>,
    ) -> Self {
        self.index.retriever = Some(retriever);
        self
    }

    /// # Panics
    ///
    /// Panics if the registry `RwLock` is poisoned.
    #[must_use]
    pub fn with_metrics(mut self, tx: watch::Sender<MetricsSnapshot>) -> Self {
        let provider_name = self.provider.name().to_string();
        let model_name = self.runtime.model_name.clone();
        let total_skills = self
            .skill_state
            .registry
            .read()
            .expect("registry read lock")
            .all_meta()
            .len();
        let qdrant_available = false;
        let conversation_id = self.memory_state.conversation_id;
        let prompt_estimate = self
            .messages
            .first()
            .map_or(0, |m| u64::try_from(m.content.len()).unwrap_or(0) / 4);
        let mcp_tool_count = self.mcp.tools.len();
        let mcp_server_count = self
            .mcp
            .tools
            .iter()
            .map(|t| &t.server_id)
            .collect::<std::collections::HashSet<_>>()
            .len();
        let extended_context = self.metrics.extended_context;
        tx.send_modify(|m| {
            m.provider_name = provider_name;
            m.model_name = model_name;
            m.total_skills = total_skills;
            m.qdrant_available = qdrant_available;
            m.sqlite_conversation_id = conversation_id;
            m.context_tokens = prompt_estimate;
            m.prompt_tokens = prompt_estimate;
            m.total_tokens = prompt_estimate;
            m.mcp_tool_count = mcp_tool_count;
            m.mcp_server_count = mcp_server_count;
            m.extended_context = extended_context;
        });
        self.metrics.metrics_tx = Some(tx);
        self
    }

    /// Returns a handle that can cancel the current in-flight operation.
    /// The returned `Notify` is stable across messages — callers invoke
    /// `notify_waiters()` to cancel whatever operation is running.
    #[must_use]
    pub fn cancel_signal(&self) -> Arc<Notify> {
        Arc::clone(&self.lifecycle.cancel_signal)
    }

    /// Inject a shared cancel signal so an external caller (e.g. ACP session) can
    /// interrupt the agent loop by calling `notify_one()`.
    #[must_use]
    pub fn with_cancel_signal(mut self, signal: Arc<Notify>) -> Self {
        self.lifecycle.cancel_signal = signal;
        self
    }

    #[must_use]
    pub fn with_subagent_manager(mut self, manager: crate::subagent::SubAgentManager) -> Self {
        self.orchestration.subagent_manager = Some(manager);
        self
    }

    #[must_use]
    pub fn with_subagent_config(mut self, config: crate::config::SubAgentConfig) -> Self {
        self.orchestration.subagent_config = config;
        self
    }

    #[must_use]
    pub fn with_orchestration_config(mut self, config: crate::config::OrchestrationConfig) -> Self {
        self.orchestration.orchestration_config = config;
        self
    }

    /// Set the experiment configuration for the `/experiment` slash command.
    #[cfg(feature = "experiments")]
    #[must_use]
    pub fn with_experiment_config(mut self, config: crate::config::ExperimentConfig) -> Self {
        self.experiment_config = config;
        self
    }

    /// Set the baseline config snapshot used when the agent runs an experiment.
    ///
    /// Call this alongside `with_experiment_config()` so the experiment engine uses
    /// actual runtime config values (temperature, memory params, etc.) rather than
    /// hardcoded defaults. Typically built via `ConfigSnapshot::from_config(&config)`.
    #[cfg(feature = "experiments")]
    #[must_use]
    pub fn with_experiment_baseline(
        mut self,
        baseline: crate::experiments::ConfigSnapshot,
    ) -> Self {
        self.experiment_baseline = baseline;
        self
    }

    /// Inject a shared provider override slot for runtime model switching (e.g. via ACP
    /// `set_session_config_option`). The agent checks and swaps the provider before each turn.
    #[must_use]
    pub fn with_provider_override(
        mut self,
        slot: Arc<std::sync::RwLock<Option<AnyProvider>>>,
    ) -> Self {
        self.providers.provider_override = Some(slot);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::super::agent_tests::{
        MockChannel, MockToolExecutor, create_test_registry, mock_provider,
    };
    use super::*;
    use crate::config::{CompressionStrategy, RoutingStrategy};

    fn make_agent() -> Agent<MockChannel> {
        Agent::new(
            mock_provider(vec![]),
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        )
    }

    #[test]
    fn with_compression_sets_proactive_strategy() {
        let compression = CompressionConfig {
            strategy: CompressionStrategy::Proactive {
                threshold_tokens: 50_000,
                max_summary_tokens: 2_000,
            },
            model: String::new(),
        };
        let agent = make_agent().with_compression(compression);
        assert!(
            matches!(
                agent.context_manager.compression.strategy,
                CompressionStrategy::Proactive {
                    threshold_tokens: 50_000,
                    max_summary_tokens: 2_000,
                }
            ),
            "expected Proactive strategy after with_compression"
        );
    }

    #[test]
    fn with_routing_sets_routing_config() {
        let routing = RoutingConfig {
            strategy: RoutingStrategy::Heuristic,
        };
        let agent = make_agent().with_routing(routing);
        assert_eq!(
            agent.context_manager.routing.strategy,
            RoutingStrategy::Heuristic,
            "routing strategy must be set by with_routing"
        );
    }

    #[test]
    fn default_compression_is_reactive() {
        let agent = make_agent();
        assert_eq!(
            agent.context_manager.compression.strategy,
            CompressionStrategy::Reactive,
            "default compression strategy must be Reactive"
        );
    }

    #[test]
    fn default_routing_is_heuristic() {
        let agent = make_agent();
        assert_eq!(
            agent.context_manager.routing.strategy,
            RoutingStrategy::Heuristic,
            "default routing strategy must be Heuristic"
        );
    }

    #[test]
    fn with_cancel_signal_replaces_internal_signal() {
        let agent = Agent::new(
            mock_provider(vec![]),
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        );

        let shared = Arc::new(Notify::new());
        let agent = agent.with_cancel_signal(Arc::clone(&shared));

        // The injected signal and the agent's internal signal must be the same Arc.
        assert!(Arc::ptr_eq(&shared, &agent.cancel_signal()));
    }

    /// Verify that `with_managed_skills_dir` enables the install/remove commands.
    /// Without a managed dir, `/skill install` sends a "not configured" message.
    /// With a managed dir configured, it proceeds past that guard (and may fail
    /// for other reasons such as the source not existing).
    #[tokio::test]
    async fn with_managed_skills_dir_enables_install_command() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let managed = tempfile::tempdir().unwrap();

        let mut agent_no_dir = Agent::new(
            mock_provider(vec![]),
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        );
        agent_no_dir
            .handle_skill_command("install /some/path")
            .await
            .unwrap();
        let sent_no_dir = agent_no_dir.channel.sent_messages();
        assert!(
            sent_no_dir.iter().any(|s| s.contains("not configured")),
            "without managed dir: {sent_no_dir:?}"
        );

        let _ = (provider, channel, registry, executor);
        let mut agent_with_dir = Agent::new(
            mock_provider(vec![]),
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        )
        .with_managed_skills_dir(managed.path().to_path_buf());

        agent_with_dir
            .handle_skill_command("install /nonexistent/path")
            .await
            .unwrap();
        let sent_with_dir = agent_with_dir.channel.sent_messages();
        assert!(
            !sent_with_dir.iter().any(|s| s.contains("not configured")),
            "with managed dir should not say not configured: {sent_with_dir:?}"
        );
        assert!(
            sent_with_dir.iter().any(|s| s.contains("Install failed")),
            "with managed dir should fail due to bad path: {sent_with_dir:?}"
        );
    }

    #[test]
    fn default_graph_config_is_disabled() {
        let agent = make_agent();
        assert!(
            !agent.memory_state.graph_config.enabled,
            "graph_config must default to disabled"
        );
    }

    #[test]
    fn with_graph_config_enabled_sets_flag() {
        let cfg = crate::config::GraphConfig {
            enabled: true,
            ..Default::default()
        };
        let agent = make_agent().with_graph_config(cfg);
        assert!(
            agent.memory_state.graph_config.enabled,
            "with_graph_config must set enabled flag"
        );
    }
}
