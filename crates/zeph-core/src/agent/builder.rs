// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::{Notify, mpsc, watch};
use zeph_llm::any::AnyProvider;
use zeph_llm::provider::LlmProvider;

use super::Agent;
use super::session_config::{AgentSessionConfig, CONTEXT_BUDGET_RESERVE_RATIO};
use crate::agent::state::ProviderConfigSnapshot;
use crate::channel::Channel;
use crate::config::{
    CompressionConfig, LearningConfig, ProviderEntry, RoutingConfig, SecurityConfig, TimeoutConfig,
};
use crate::config_watcher::ConfigEvent;
use crate::context::ContextBudget;
use crate::cost::CostTracker;
use crate::instructions::{InstructionEvent, InstructionReloadState};
use crate::metrics::MetricsSnapshot;
use zeph_memory::semantic::SemanticMemory;
use zeph_skills::watcher::SkillEvent;

impl<C: Channel> Agent<C> {
    /// Attach a status channel for spinner/status messages sent to TUI or stderr.
    /// The sender must be cloned from the provider's `StatusTx` before
    /// `provider.set_status_tx()` consumes it.
    #[must_use]
    pub fn with_status_tx(mut self, tx: tokio::sync::mpsc::UnboundedSender<String>) -> Self {
        self.session.status_tx = Some(tx);
        self
    }

    /// Store a snapshot of the policy config for `/policy` command inspection.
    #[cfg(feature = "policy-enforcer")]
    #[must_use]
    pub fn with_policy_config(mut self, config: zeph_tools::PolicyConfig) -> Self {
        self.session.policy_config = Some(config);
        self
    }

    #[must_use]
    pub fn with_structured_summaries(mut self, enabled: bool) -> Self {
        self.memory_state.structured_summaries = enabled;
        self
    }

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
    pub fn with_shutdown_summary_config(
        mut self,
        enabled: bool,
        min_messages: usize,
        max_messages: usize,
        timeout_secs: u64,
    ) -> Self {
        self.memory_state.shutdown_summary = enabled;
        self.memory_state.shutdown_summary_min_messages = min_messages;
        self.memory_state.shutdown_summary_max_messages = max_messages;
        self.memory_state.shutdown_summary_timeout_secs = timeout_secs;
        self
    }

    #[must_use]
    pub fn with_response_cache(
        mut self,
        cache: std::sync::Arc<zeph_memory::ResponseCache>,
    ) -> Self {
        self.session.response_cache = Some(cache);
        self
    }

    /// Set the parent tool call ID for subagent sessions.
    ///
    /// When set, every `LoopbackEvent::ToolStart` and `LoopbackEvent::ToolOutput` emitted
    /// by this agent will carry the `parent_tool_use_id` so the IDE can build a subagent
    /// hierarchy tree.
    #[must_use]
    pub fn with_parent_tool_use_id(mut self, id: impl Into<String>) -> Self {
        self.session.parent_tool_use_id = Some(id.into());
        self
    }

    #[must_use]
    pub fn with_stt(mut self, stt: Box<dyn zeph_llm::stt::SpeechToText>) -> Self {
        self.providers.stt = Some(stt);
        self
    }

    /// Set the dedicated embedding provider (resolved once at bootstrap, never changed by
    /// `/provider switch`). When not called, defaults to the primary provider clone set in
    /// `Agent::new`.
    #[must_use]
    pub fn with_embedding_provider(mut self, provider: AnyProvider) -> Self {
        self.embedding_provider = provider;
        self
    }

    /// Store the provider pool and config snapshot for runtime `/provider` switching.
    #[must_use]
    pub fn with_provider_pool(
        mut self,
        pool: Vec<ProviderEntry>,
        snapshot: ProviderConfigSnapshot,
    ) -> Self {
        self.providers.provider_pool = pool;
        self.providers.provider_config_snapshot = Some(snapshot);
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
        self.session.lsp_hooks = Some(runner);
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

    /// Set the provider name for LLM-based parameter reformatting (empty = disabled).
    #[must_use]
    pub fn with_parameter_reformat_provider(mut self, provider: impl Into<String>) -> Self {
        self.tool_orchestrator.parameter_reformat_provider = provider.into();
        self
    }

    /// Set the exponential backoff parameters for tool retries.
    #[must_use]
    pub fn with_retry_backoff(mut self, base_ms: u64, max_ms: u64) -> Self {
        self.tool_orchestrator.retry_base_ms = base_ms;
        self.tool_orchestrator.retry_max_ms = max_ms;
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
    pub fn with_compression_guidelines_config(
        mut self,
        config: zeph_memory::CompressionGuidelinesConfig,
    ) -> Self {
        self.memory_state.compression_guidelines_config = config;
        self
    }

    #[must_use]
    pub fn with_digest_config(mut self, config: crate::config::DigestConfig) -> Self {
        self.memory_state.digest_config = config;
        self
    }

    #[must_use]
    pub fn with_context_strategy(
        mut self,
        strategy: crate::config::ContextStrategy,
        crossover_turn_threshold: u32,
    ) -> Self {
        self.memory_state.context_strategy = strategy;
        self.memory_state.crossover_turn_threshold = crossover_turn_threshold;
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
        self.instructions.blocks = blocks;
        self
    }

    #[must_use]
    pub fn with_instruction_reload(
        mut self,
        rx: mpsc::Receiver<InstructionEvent>,
        state: InstructionReloadState,
    ) -> Self {
        self.instructions.reload_rx = Some(rx);
        self.instructions.reload_state = Some(state);
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
            self.feedback.detector = super::feedback_detector::FeedbackDetector::new(
                config.correction_confidence_threshold,
            );
            if config.detector_mode == crate::config::DetectorMode::Judge {
                self.feedback.judge = Some(super::feedback_detector::JudgeDetector::new(
                    config.judge_adaptive_low,
                    config.judge_adaptive_high,
                ));
            }
        }
        self.learning_engine.config = Some(config);
        self
    }

    /// Attach an `LlmClassifier` for `detector_mode = "model"` feedback detection.
    ///
    /// When attached, the model-based path is used instead of `JudgeDetector`.
    /// The classifier resolves the provider at construction time — if the provider
    /// is unavailable, do not call this method (fallback to regex-only).
    #[must_use]
    pub fn with_llm_classifier(
        mut self,
        classifier: zeph_llm::classifier::llm::LlmClassifier,
    ) -> Self {
        // If classifier_metrics is already set, wire it into the LlmClassifier for Feedback recording.
        #[cfg(feature = "classifiers")]
        let classifier = if let Some(ref m) = self.metrics.classifier_metrics {
            classifier.with_metrics(std::sync::Arc::clone(m))
        } else {
            classifier
        };
        self.feedback.llm_classifier = Some(classifier);
        self
    }

    #[must_use]
    pub fn with_judge_provider(mut self, provider: AnyProvider) -> Self {
        self.providers.judge_provider = Some(provider);
        self
    }

    #[must_use]
    pub fn with_probe_provider(mut self, provider: AnyProvider) -> Self {
        self.providers.probe_provider = Some(provider);
        self
    }

    /// Set a dedicated provider for `compress_context` LLM calls (#2356).
    ///
    /// When not set, `handle_compress_context` falls back to the primary provider.
    #[must_use]
    #[cfg(feature = "context-compression")]
    pub fn with_compress_provider(mut self, provider: AnyProvider) -> Self {
        self.providers.compress_provider = Some(provider);
        self
    }

    #[must_use]
    pub fn with_planner_provider(mut self, provider: AnyProvider) -> Self {
        self.orchestration.planner_provider = Some(provider);
        self
    }

    /// Set a dedicated provider for `PlanVerifier` LLM calls.
    ///
    /// When not set, verification falls back to the primary provider.
    #[must_use]
    pub fn with_verify_provider(mut self, provider: AnyProvider) -> Self {
        self.orchestration.verify_provider = Some(provider);
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
    pub fn with_mcp_server_outcomes(
        mut self,
        outcomes: Vec<zeph_mcp::ServerConnectOutcome>,
    ) -> Self {
        self.mcp.server_outcomes = outcomes;
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

    /// Configure MCP tool pruning (#2298).
    ///
    /// Sets the pruning params derived from `ToolPruningConfig` and optionally a dedicated
    /// provider for pruning LLM calls.  `pruning_provider = None` means fall back to the
    /// primary provider.
    #[must_use]
    pub fn with_mcp_pruning(
        mut self,
        params: zeph_mcp::PruningParams,
        enabled: bool,
        pruning_provider: Option<zeph_llm::any::AnyProvider>,
    ) -> Self {
        self.mcp.pruning_params = params;
        self.mcp.pruning_enabled = enabled;
        self.mcp.pruning_provider = pruning_provider;
        self
    }

    /// Configure embedding-based MCP tool discovery (#2321).
    ///
    /// Sets the discovery strategy, parameters, and optionally a dedicated embedding provider.
    /// `discovery_provider = None` means fall back to the agent's primary embedding provider.
    #[must_use]
    pub fn with_mcp_discovery(
        mut self,
        strategy: zeph_mcp::ToolDiscoveryStrategy,
        params: zeph_mcp::DiscoveryParams,
        discovery_provider: Option<zeph_llm::any::AnyProvider>,
    ) -> Self {
        self.mcp.discovery_strategy = strategy;
        self.mcp.discovery_params = params;
        self.mcp.discovery_provider = discovery_provider;
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
            zeph_sanitizer::ContentSanitizer::new(&security.content_isolation);
        self.security.exfiltration_guard = zeph_sanitizer::exfiltration::ExfiltrationGuard::new(
            security.exfiltration_guard.clone(),
        );
        self.security.pii_filter = zeph_sanitizer::pii::PiiFilter::new(security.pii_filter.clone());
        self.security.memory_validator =
            zeph_sanitizer::memory_validation::MemoryWriteValidator::new(
                security.memory_validation.clone(),
            );
        self.runtime.rate_limiter =
            crate::agent::rate_limiter::ToolRateLimiter::new(security.rate_limit.clone());

        // Build pre-execution verifiers from config.
        // Stored on ToolOrchestrator (not SecurityState) — verifiers inspect tool arguments
        // at dispatch time, consistent with repeat-detection and rate-limiting which also
        // live on ToolOrchestrator. SecurityState hosts zeph-core::sanitizer types only.
        let mut verifiers: Vec<Box<dyn zeph_tools::PreExecutionVerifier>> = Vec::new();
        if security.pre_execution_verify.enabled {
            let dcfg = &security.pre_execution_verify.destructive_commands;
            if dcfg.enabled {
                verifiers.push(Box::new(zeph_tools::DestructiveCommandVerifier::new(dcfg)));
            }
            let icfg = &security.pre_execution_verify.injection_patterns;
            if icfg.enabled {
                verifiers.push(Box::new(zeph_tools::InjectionPatternVerifier::new(icfg)));
            }
            let ucfg = &security.pre_execution_verify.url_grounding;
            if ucfg.enabled {
                verifiers.push(Box::new(zeph_tools::UrlGroundingVerifier::new(
                    ucfg,
                    std::sync::Arc::clone(&self.security.user_provided_urls),
                )));
            }
            let fcfg = &security.pre_execution_verify.firewall;
            if fcfg.enabled {
                verifiers.push(Box::new(zeph_tools::FirewallVerifier::new(fcfg)));
            }
        }
        self.tool_orchestrator.pre_execution_verifiers = verifiers;

        self.security.response_verifier = zeph_sanitizer::response_verifier::ResponseVerifier::new(
            security.response_verification.clone(),
        );

        self.runtime.security = security;
        self.runtime.timeouts = timeouts;
        self
    }

    /// Attach an audit logger for pre-execution verifier blocks.
    #[must_use]
    pub fn with_audit_logger(mut self, logger: std::sync::Arc<zeph_tools::AuditLogger>) -> Self {
        self.tool_orchestrator.audit_logger = Some(logger);
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

    /// Configure Think-Augmented Function Calling (TAFC).
    ///
    /// `complexity_threshold` is clamped to [0.0, 1.0]; NaN / Inf are reset to 0.6.
    #[must_use]
    pub fn with_tafc_config(mut self, config: zeph_tools::TafcConfig) -> Self {
        self.tool_orchestrator.tafc = config.validated();
        self
    }

    #[must_use]
    pub fn with_result_cache_config(mut self, config: &zeph_tools::ResultCacheConfig) -> Self {
        self.tool_orchestrator.set_cache_config(config);
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
        qs: zeph_sanitizer::quarantine::QuarantinedSummarizer,
    ) -> Self {
        self.security.quarantine_summarizer = Some(qs);
        self
    }

    /// Attach an ML classifier backend to the sanitizer for injection detection.
    ///
    /// When attached, `classify_injection()` is called on each incoming user message when
    /// `classifiers.enabled = true`. On error or timeout it falls back to regex detection.
    #[cfg(feature = "classifiers")]
    #[must_use]
    pub fn with_injection_classifier(
        mut self,
        backend: std::sync::Arc<dyn zeph_llm::classifier::ClassifierBackend>,
        timeout_ms: u64,
        threshold: f32,
        threshold_soft: f32,
    ) -> Self {
        // Replace sanitizer in-place: move out, attach classifier, move back.
        let old = std::mem::replace(
            &mut self.security.sanitizer,
            zeph_sanitizer::ContentSanitizer::new(
                &zeph_sanitizer::ContentIsolationConfig::default(),
            ),
        );
        self.security.sanitizer = old
            .with_classifier(backend, timeout_ms, threshold)
            .with_injection_threshold_soft(threshold_soft);
        self
    }

    /// Set the enforcement mode for the injection classifier.
    ///
    /// `Warn` (default): scores above the hard threshold emit WARN + metric but do NOT block.
    /// `Block`: scores above the hard threshold block content.
    #[cfg(feature = "classifiers")]
    #[must_use]
    pub fn with_enforcement_mode(mut self, mode: zeph_config::InjectionEnforcementMode) -> Self {
        let old = std::mem::replace(
            &mut self.security.sanitizer,
            zeph_sanitizer::ContentSanitizer::new(
                &zeph_sanitizer::ContentIsolationConfig::default(),
            ),
        );
        self.security.sanitizer = old.with_enforcement_mode(mode);
        self
    }

    /// Attach a three-class classifier backend for `AlignSentinel` injection refinement.
    #[cfg(feature = "classifiers")]
    #[must_use]
    pub fn with_three_class_classifier(
        mut self,
        backend: std::sync::Arc<dyn zeph_llm::classifier::ClassifierBackend>,
        threshold: f32,
    ) -> Self {
        let old = std::mem::replace(
            &mut self.security.sanitizer,
            zeph_sanitizer::ContentSanitizer::new(
                &zeph_sanitizer::ContentIsolationConfig::default(),
            ),
        );
        self.security.sanitizer = old.with_three_class_backend(backend, threshold);
        self
    }

    /// Attach a temporal causal IPI analyzer.
    ///
    /// When `Some`, the native tool dispatch loop runs pre/post behavioral probes.
    #[must_use]
    pub fn with_causal_analyzer(
        mut self,
        analyzer: zeph_sanitizer::causal_ipi::TurnCausalAnalyzer,
    ) -> Self {
        self.security.causal_analyzer = Some(analyzer);
        self
    }

    /// Configure whether the ML classifier runs on direct user chat messages.
    ///
    /// Default `false`. See `ClassifiersConfig::scan_user_input` for rationale.
    #[cfg(feature = "classifiers")]
    #[must_use]
    pub fn with_scan_user_input(mut self, value: bool) -> Self {
        let old = std::mem::replace(
            &mut self.security.sanitizer,
            zeph_sanitizer::ContentSanitizer::new(
                &zeph_sanitizer::ContentIsolationConfig::default(),
            ),
        );
        self.security.sanitizer = old.with_scan_user_input(value);
        self
    }

    /// Attach a PII detector backend to the sanitizer.
    ///
    /// When attached, `detect_pii()` is called on outgoing assistant responses when
    /// `classifiers.pii_enabled = true`. On error it falls back to returning no spans.
    #[cfg(feature = "classifiers")]
    #[must_use]
    pub fn with_pii_detector(
        mut self,
        detector: std::sync::Arc<dyn zeph_llm::classifier::PiiDetector>,
        threshold: f32,
    ) -> Self {
        let old = std::mem::replace(
            &mut self.security.sanitizer,
            zeph_sanitizer::ContentSanitizer::new(
                &zeph_sanitizer::ContentIsolationConfig::default(),
            ),
        );
        self.security.sanitizer = old.with_pii_detector(detector, threshold);
        self
    }

    /// Attach a NER classifier backend for PII detection in the union merge pipeline.
    ///
    /// When attached, `sanitize_tool_output()` runs both regex and NER, merges spans, and
    /// redacts from the merged list in a single pass. References `classifiers.ner_model`.
    #[cfg(feature = "classifiers")]
    #[must_use]
    pub fn with_pii_ner_classifier(
        mut self,
        backend: std::sync::Arc<dyn zeph_llm::classifier::ClassifierBackend>,
        timeout_ms: u64,
    ) -> Self {
        self.security.pii_ner_backend = Some(backend);
        self.security.pii_ner_timeout_ms = timeout_ms;
        self
    }

    /// Attach a [`ClassifierMetrics`] instance to record injection, PII, and feedback latencies.
    ///
    /// The same `Arc` is shared with `ContentSanitizer` (injection + PII) and `LlmClassifier`
    /// (feedback) so all three tasks write into the same ring buffers. Call this before
    /// `with_injection_classifier`, `with_pii_detector`, and `with_llm_classifier`.
    #[cfg(feature = "classifiers")]
    #[must_use]
    pub fn with_classifier_metrics(
        mut self,
        metrics: std::sync::Arc<zeph_llm::ClassifierMetrics>,
    ) -> Self {
        // Wire into sanitizer for injection + PII recording.
        let old = std::mem::replace(
            &mut self.security.sanitizer,
            zeph_sanitizer::ContentSanitizer::new(
                &zeph_sanitizer::ContentIsolationConfig::default(),
            ),
        );
        self.security.sanitizer = old.with_classifier_metrics(std::sync::Arc::clone(&metrics));
        // Store Arc for snapshot push and LlmClassifier wiring.
        self.metrics.classifier_metrics = Some(metrics);
        self
    }

    #[cfg(feature = "guardrail")]
    #[must_use]
    pub fn with_guardrail(mut self, filter: zeph_sanitizer::guardrail::GuardrailFilter) -> Self {
        use zeph_sanitizer::guardrail::GuardrailAction;
        let warn_mode = filter.action() == GuardrailAction::Warn;
        self.security.guardrail = Some(filter);
        self.update_metrics(|m| {
            m.guardrail_enabled = true;
            m.guardrail_warn_mode = warn_mode;
        });
        self
    }

    pub(super) fn summary_or_primary_provider(&self) -> &AnyProvider {
        self.providers
            .summary_provider
            .as_ref()
            .unwrap_or(&self.provider)
    }

    pub(super) fn probe_or_summary_provider(&self) -> &AnyProvider {
        self.providers
            .probe_provider
            .as_ref()
            .or(self.providers.summary_provider.as_ref())
            .unwrap_or(&self.provider)
    }

    /// Extract the last assistant message, truncated to 500 chars, for the judge prompt.
    pub(super) fn last_assistant_response(&self) -> String {
        self.msg
            .messages
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
        if budget_tokens == 0 {
            tracing::warn!("context budget is 0 — agent will have no token tracking");
        }
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

    /// Configure Focus-based active context compression (#1850).
    #[must_use]
    pub fn with_focus_config(mut self, config: crate::config::FocusConfig) -> Self {
        self.focus = super::focus::FocusState::new(config);
        self
    }

    /// Configure `SideQuest` LLM-driven tool output eviction (#1885).
    #[must_use]
    pub fn with_sidequest_config(mut self, config: crate::config::SidequestConfig) -> Self {
        self.sidequest = super::sidequest::SidequestState::new(config);
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

    /// Set the configured provider name (from `[[llm.providers]]` `name` field).
    ///
    /// Used by the TUI metrics panel and `/provider status` to display the logical name
    /// instead of the provider type string returned by `LlmProvider::name()`.
    #[must_use]
    pub fn with_active_provider_name(mut self, name: impl Into<String>) -> Self {
        self.runtime.active_provider_name = name.into();
        self
    }

    #[must_use]
    pub fn with_working_dir(mut self, path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        self.session.env_context =
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
        let provider_name = if self.runtime.active_provider_name.is_empty() {
            self.provider.name().to_owned()
        } else {
            self.runtime.active_provider_name.clone()
        };
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
            .msg
            .messages
            .first()
            .map_or(0, |m| u64::try_from(m.content.len()).unwrap_or(0) / 4);
        let mcp_tool_count = self.mcp.tools.len();
        let mcp_server_count = if self.mcp.server_outcomes.is_empty() {
            // Fallback: count unique server IDs from connected tools
            self.mcp
                .tools
                .iter()
                .map(|t| &t.server_id)
                .collect::<std::collections::HashSet<_>>()
                .len()
        } else {
            self.mcp.server_outcomes.len()
        };
        let mcp_connected_count = if self.mcp.server_outcomes.is_empty() {
            mcp_server_count
        } else {
            self.mcp
                .server_outcomes
                .iter()
                .filter(|o| o.connected)
                .count()
        };
        let mcp_servers: Vec<crate::metrics::McpServerStatus> = self
            .mcp
            .server_outcomes
            .iter()
            .map(|o| crate::metrics::McpServerStatus {
                id: o.id.clone(),
                status: if o.connected {
                    crate::metrics::McpServerConnectionStatus::Connected
                } else {
                    crate::metrics::McpServerConnectionStatus::Failed
                },
                tool_count: o.tool_count,
                error: o.error.clone(),
            })
            .collect();
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
            m.mcp_connected_count = mcp_connected_count;
            m.mcp_servers = mcp_servers;
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
        self.experiments.config = config;
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
        self.experiments.baseline = baseline;
        self
    }

    /// Set a dedicated judge provider for experiment evaluation.
    ///
    /// When set, the evaluator uses this provider instead of the agent's primary provider,
    /// eliminating self-judge bias. Corresponds to `experiments.eval_model` in config.
    #[cfg(feature = "experiments")]
    #[must_use]
    pub fn with_eval_provider(mut self, provider: AnyProvider) -> Self {
        self.experiments.eval_provider = Some(provider);
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

    /// Set the dynamic tool schema filter (pre-computed tool embeddings).
    #[must_use]
    pub fn with_tool_schema_filter(mut self, filter: zeph_tools::ToolSchemaFilter) -> Self {
        self.tool_schema_filter = Some(filter);
        self
    }

    /// Set dependency config parameters (boost values) used per-turn.
    #[must_use]
    pub fn with_dependency_config(mut self, config: zeph_tools::DependencyConfig) -> Self {
        self.runtime.dependency_config = config;
        self
    }

    /// Attach a tool dependency graph for sequential tool availability (issue #2024).
    ///
    /// When set, hard gates (`requires`) are applied after schema filtering, and soft boosts
    /// (`prefers`) are added to similarity scores. Always-on tool IDs bypass hard gates.
    #[must_use]
    pub fn with_tool_dependency_graph(
        mut self,
        graph: zeph_tools::ToolDependencyGraph,
        always_on: std::collections::HashSet<String>,
    ) -> Self {
        self.dependency_graph = Some(graph);
        self.dependency_always_on = always_on;
        self
    }

    /// Initialize and attach the tool schema filter if enabled in config.
    ///
    /// Embeds all filterable tool descriptions at startup and caches the embeddings.
    /// Gracefully degrades: returns `self` unchanged if embedding is unsupported or fails.
    pub async fn maybe_init_tool_schema_filter(
        mut self,
        config: &crate::config::ToolFilterConfig,
        provider: &zeph_llm::any::AnyProvider,
    ) -> Self {
        use zeph_llm::provider::LlmProvider;

        if !config.enabled {
            return self;
        }

        let always_on_set: std::collections::HashSet<&str> =
            config.always_on.iter().map(String::as_str).collect();
        let defs = self.tool_executor.tool_definitions_erased();
        let filterable: Vec<&zeph_tools::registry::ToolDef> = defs
            .iter()
            .filter(|d| !always_on_set.contains(d.id.as_ref()))
            .collect();

        if filterable.is_empty() {
            tracing::info!("tool schema filter: all tools are always-on, nothing to filter");
            return self;
        }

        let mut embeddings = Vec::with_capacity(filterable.len());
        for def in &filterable {
            let text = format!("{}: {}", def.id, def.description);
            match provider.embed(&text).await {
                Ok(emb) => {
                    embeddings.push(zeph_tools::ToolEmbedding {
                        tool_id: def.id.to_string(),
                        embedding: emb,
                    });
                }
                Err(e) => {
                    tracing::info!(
                        provider = provider.name(),
                        "tool schema filter disabled: embedding not supported \
                        by provider ({e:#})"
                    );
                    return self;
                }
            }
        }

        tracing::info!(
            tool_count = embeddings.len(),
            always_on = config.always_on.len(),
            top_k = config.top_k,
            "tool schema filter initialized"
        );

        let filter = zeph_tools::ToolSchemaFilter::new(
            config.always_on.clone(),
            config.top_k,
            config.min_description_words,
            embeddings,
        );
        self.tool_schema_filter = Some(filter);
        self
    }

    /// Apply all config-derived settings from [`AgentSessionConfig`] in a single call.
    ///
    /// Takes `cfg` by value and destructures it so the compiler emits an unused-variable warning
    /// for any field that is added to [`AgentSessionConfig`] but not consumed here (S4).
    ///
    /// Per-session wiring (`cancel_signal`, `provider_override`, `memory`, `debug_dumper`, etc.)
    /// must still be applied separately after this call, since those depend on runtime state.
    #[must_use]
    pub fn apply_session_config(mut self, cfg: AgentSessionConfig) -> Self {
        let AgentSessionConfig {
            max_tool_iterations,
            max_tool_retries,
            max_retry_duration_secs,
            retry_base_ms,
            retry_max_ms,
            parameter_reformat_provider,
            tool_repeat_threshold,
            tool_summarization,
            tool_call_cutoff,
            overflow_config,
            permission_policy,
            model_name,
            embed_model,
            semantic_cache_enabled,
            semantic_cache_threshold,
            semantic_cache_max_candidates,
            budget_tokens,
            soft_compaction_threshold,
            hard_compaction_threshold,
            compaction_preserve_tail,
            compaction_cooldown_turns,
            prune_protect_tokens,
            redact_credentials,
            security,
            timeouts,
            learning,
            document_config,
            graph_config,
            anomaly_config,
            result_cache_config,
            orchestration_config,
            // Not applied here: caller clones this before `apply_session_config` and applies
            // it per-session (e.g. `spawn_acp_agent` passes it to `with_debug_config`).
            debug_config: _debug_config,
            server_compaction,
            secrets,
        } = cfg;

        self = self
            .with_max_tool_iterations(max_tool_iterations)
            .with_max_tool_retries(max_tool_retries)
            .with_max_retry_duration_secs(max_retry_duration_secs)
            .with_retry_backoff(retry_base_ms, retry_max_ms)
            .with_parameter_reformat_provider(parameter_reformat_provider)
            .with_tool_repeat_threshold(tool_repeat_threshold)
            .with_model_name(model_name)
            .with_embedding_model(embed_model)
            .with_context_budget(
                budget_tokens,
                CONTEXT_BUDGET_RESERVE_RATIO,
                hard_compaction_threshold,
                compaction_preserve_tail,
                prune_protect_tokens,
            )
            .with_soft_compaction_threshold(soft_compaction_threshold)
            .with_compaction_cooldown(compaction_cooldown_turns)
            .with_security(security, timeouts)
            .with_redact_credentials(redact_credentials)
            .with_tool_summarization(tool_summarization)
            .with_overflow_config(overflow_config)
            .with_permission_policy(permission_policy)
            .with_learning(learning)
            .with_tool_call_cutoff(tool_call_cutoff)
            .with_available_secrets(
                secrets
                    .iter()
                    .map(|(k, v)| (k.clone(), crate::vault::Secret::new(v.expose().to_owned()))),
            )
            .with_server_compaction(server_compaction)
            .with_document_config(document_config)
            .with_graph_config(graph_config)
            .with_orchestration_config(orchestration_config);

        self.debug_state.reasoning_model_warning = anomaly_config.reasoning_model_warning;
        if anomaly_config.enabled {
            self = self.with_anomaly_detector(zeph_tools::AnomalyDetector::new(
                anomaly_config.window_size,
                anomaly_config.error_threshold,
                anomaly_config.critical_threshold,
            ));
        }

        self.runtime.semantic_cache_enabled = semantic_cache_enabled;
        self.runtime.semantic_cache_threshold = semantic_cache_threshold;
        self.runtime.semantic_cache_max_candidates = semantic_cache_max_candidates;
        self = self.with_result_cache_config(&result_cache_config);

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
            pruning_strategy: crate::config::PruningStrategy::default(),
            probe: zeph_memory::CompactionProbeConfig::default(),
            compress_provider: String::new(),
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

    /// Verify that `apply_session_config` wires graph memory, orchestration, and anomaly
    /// detector configs into the agent in a single call — the acceptance criterion for issue #1812.
    ///
    /// This exercises the full path: `AgentSessionConfig::from_config` → `apply_session_config` →
    /// agent internal state, confirming that all three feature configs are propagated correctly.
    #[test]
    fn apply_session_config_wires_graph_orchestration_anomaly() {
        use crate::config::Config;

        let mut config = Config::default();
        config.memory.graph.enabled = true;
        config.orchestration.enabled = true;
        config.orchestration.max_tasks = 42;
        config.tools.anomaly.enabled = true;
        config.tools.anomaly.window_size = 7;

        let session_cfg = AgentSessionConfig::from_config(&config, 100_000);

        // Precondition: from_config captured the values.
        assert!(session_cfg.graph_config.enabled);
        assert!(session_cfg.orchestration_config.enabled);
        assert_eq!(session_cfg.orchestration_config.max_tasks, 42);
        assert!(session_cfg.anomaly_config.enabled);
        assert_eq!(session_cfg.anomaly_config.window_size, 7);

        let agent = make_agent().apply_session_config(session_cfg);

        // Graph config must be set on memory_state.
        assert!(
            agent.memory_state.graph_config.enabled,
            "apply_session_config must wire graph_config into agent"
        );

        // Orchestration config must be propagated.
        assert!(
            agent.orchestration.orchestration_config.enabled,
            "apply_session_config must wire orchestration_config into agent"
        );
        assert_eq!(
            agent.orchestration.orchestration_config.max_tasks, 42,
            "orchestration max_tasks must match config"
        );

        // Anomaly detector must be created when anomaly_config.enabled = true.
        assert!(
            agent.debug_state.anomaly_detector.is_some(),
            "apply_session_config must create anomaly_detector when enabled"
        );
    }

    #[test]
    fn with_focus_config_propagates_to_focus_state() {
        let cfg = crate::config::FocusConfig {
            enabled: true,
            compression_interval: 7,
            ..Default::default()
        };
        let agent = make_agent().with_focus_config(cfg.clone());
        assert!(
            agent.focus.config.enabled,
            "with_focus_config must set enabled"
        );
        assert_eq!(
            agent.focus.config.compression_interval, 7,
            "with_focus_config must propagate compression_interval"
        );
    }

    #[test]
    fn with_sidequest_config_propagates_to_sidequest_state() {
        let cfg = crate::config::SidequestConfig {
            enabled: true,
            interval_turns: 3,
            ..Default::default()
        };
        let agent = make_agent().with_sidequest_config(cfg.clone());
        assert!(
            agent.sidequest.config.enabled,
            "with_sidequest_config must set enabled"
        );
        assert_eq!(
            agent.sidequest.config.interval_turns, 3,
            "with_sidequest_config must propagate interval_turns"
        );
    }

    /// Verify that `apply_session_config` does NOT create an anomaly detector when disabled.
    #[test]
    fn apply_session_config_skips_anomaly_detector_when_disabled() {
        use crate::config::Config;

        let mut config = Config::default();
        config.tools.anomaly.enabled = false; // explicitly disable to test the disabled path
        let session_cfg = AgentSessionConfig::from_config(&config, 100_000);
        assert!(!session_cfg.anomaly_config.enabled);

        let agent = make_agent().apply_session_config(session_cfg);
        assert!(
            agent.debug_state.anomaly_detector.is_none(),
            "apply_session_config must not create anomaly_detector when disabled"
        );
    }
}
