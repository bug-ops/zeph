// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Sub-struct definitions for the `Agent` struct.
//!
//! Each struct groups a related cluster of `Agent` fields.
//! All types are `pub(super)` — visible only within the `agent` module.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Instant;

use tokio::sync::{Notify, mpsc, watch};
use tokio_util::sync::CancellationToken;
use zeph_llm::any::AnyProvider;
use zeph_llm::provider::Message;
use zeph_llm::stt::SpeechToText;

use crate::config::{ProviderEntry, SecurityConfig, SkillPromptMode, TimeoutConfig};
use crate::config_watcher::ConfigEvent;
use crate::context::EnvironmentContext;
use crate::cost::CostTracker;
use crate::instructions::{InstructionBlock, InstructionEvent, InstructionReloadState};
use crate::metrics::MetricsSnapshot;
use crate::vault::Secret;
use zeph_memory::TokenCounter;
use zeph_memory::semantic::SemanticMemory;
use zeph_sanitizer::ContentSanitizer;
use zeph_sanitizer::quarantine::QuarantinedSummarizer;
use zeph_skills::matcher::SkillMatcherBackend;
use zeph_skills::registry::SkillRegistry;
use zeph_skills::watcher::SkillEvent;

use super::message_queue::QueuedMessage;

pub(crate) struct MemoryState {
    pub(crate) memory: Option<Arc<SemanticMemory>>,
    pub(crate) conversation_id: Option<zeph_memory::ConversationId>,
    pub(crate) history_limit: u32,
    pub(crate) recall_limit: usize,
    pub(crate) summarization_threshold: usize,
    pub(crate) cross_session_score_threshold: f32,
    pub(crate) autosave_assistant: bool,
    pub(crate) autosave_min_length: usize,
    pub(crate) tool_call_cutoff: usize,
    pub(crate) unsummarized_count: usize,
    pub(crate) document_config: crate::config::DocumentConfig,
    pub(crate) graph_config: crate::config::GraphConfig,
    pub(crate) compression_guidelines_config: zeph_memory::CompressionGuidelinesConfig,
    pub(crate) shutdown_summary: bool,
    pub(crate) shutdown_summary_min_messages: usize,
    pub(crate) shutdown_summary_max_messages: usize,
    pub(crate) shutdown_summary_timeout_secs: u64,
    /// When `true`, hard compaction uses `AnchoredSummary` (structured JSON) instead of
    /// free-form prose. Falls back to prose on any LLM or validation failure.
    pub(crate) structured_summaries: bool,
    /// Top-1 semantic recall score from the most recent `prepare_context` cycle.
    /// Used by MAR (Memory-Augmented Routing) to bias the bandit toward cheap providers
    /// when memory confidence is high. Reset to `None` at the start of each turn.
    pub(crate) last_recall_confidence: Option<f32>,
    /// Session digest configuration (#2289).
    pub(crate) digest_config: crate::config::DigestConfig,
    /// Cached session digest text and its token count, loaded at session start.
    pub(crate) cached_session_digest: Option<(String, usize)>,
    /// Context assembly strategy (#2288).
    pub(crate) context_strategy: crate::config::ContextStrategy,
    /// Turn threshold for `Adaptive` strategy crossover (#2288).
    pub(crate) crossover_turn_threshold: u32,
    /// D-MEM RPE router. `Some` when `graph_config.rpe.enabled = true`.
    /// Protected by `std::sync::Mutex` for non-async access from `maybe_spawn_graph_extraction`.
    pub(crate) rpe_router: Option<std::sync::Mutex<zeph_memory::RpeRouter>>,
}

pub(crate) struct SkillState {
    pub(crate) registry: std::sync::Arc<std::sync::RwLock<SkillRegistry>>,
    pub(crate) skill_paths: Vec<PathBuf>,
    pub(crate) managed_dir: Option<PathBuf>,
    pub(crate) trust_config: crate::config::TrustConfig,
    pub(crate) matcher: Option<SkillMatcherBackend>,
    pub(crate) max_active_skills: usize,
    pub(crate) disambiguation_threshold: f32,
    pub(crate) embedding_model: String,
    pub(crate) skill_reload_rx: Option<mpsc::Receiver<SkillEvent>>,
    pub(crate) active_skill_names: Vec<String>,
    pub(crate) last_skills_prompt: String,
    pub(crate) prompt_mode: SkillPromptMode,
    /// Custom secrets available at runtime: key=hyphenated name, value=secret.
    pub(crate) available_custom_secrets: HashMap<String, Secret>,
    pub(crate) cosine_weight: f32,
    pub(crate) hybrid_search: bool,
    pub(crate) bm25_index: Option<zeph_skills::bm25::Bm25Index>,
    pub(crate) two_stage_matching: bool,
    /// Threshold for confusability warnings (0.0 = disabled).
    pub(crate) confusability_threshold: f32,
}

pub(crate) struct McpState {
    pub(crate) tools: Vec<zeph_mcp::McpTool>,
    pub(crate) registry: Option<zeph_mcp::McpToolRegistry>,
    pub(crate) manager: Option<std::sync::Arc<zeph_mcp::McpManager>>,
    pub(crate) allowed_commands: Vec<String>,
    pub(crate) max_dynamic: usize,
    /// Shared with `McpToolExecutor` so native `tool_use` sees the current tool list.
    ///
    /// Two methods write to this `RwLock` — ordering matters:
    /// - `sync_mcp_executor_tools()`: writes the **full** `self.mcp.tools` set.
    /// - `apply_pruned_mcp_tools()`: writes the **pruned** subset (used after pruning).
    ///
    /// Within a turn, `sync_mcp_executor_tools` must always run **before**
    /// `apply_pruned_mcp_tools`.  The normal call order guarantees this: tool-list
    /// change events call `sync_mcp_executor_tools` (inside `check_tool_refresh`,
    /// `handle_mcp_add`, `handle_mcp_remove`), and pruning runs later inside
    /// `rebuild_system_prompt`.  See also: `apply_pruned_mcp_tools`.
    pub(crate) shared_tools: Option<std::sync::Arc<std::sync::RwLock<Vec<zeph_mcp::McpTool>>>>,
    /// Receives full flattened tool list after any `tools/list_changed` notification.
    pub(crate) tool_rx: Option<tokio::sync::watch::Receiver<Vec<zeph_mcp::McpTool>>>,
    /// Per-server connection outcomes from the initial `connect_all()` call.
    pub(crate) server_outcomes: Vec<zeph_mcp::ServerConnectOutcome>,
    /// Per-message cache for MCP tool pruning results (#2298).
    ///
    /// Reset at the start of each user turn and whenever the MCP tool list
    /// changes (via `tools/list_changed`, `/mcp add`, or `/mcp remove`).
    pub(crate) pruning_cache: zeph_mcp::PruningCache,
    /// Dedicated provider for MCP tool pruning LLM calls.
    ///
    /// `None` means fall back to the agent's primary provider.
    /// Resolved from `[[llm.providers]]` at build time using `pruning_provider`
    /// from `ToolPruningConfig`.
    pub(crate) pruning_provider: Option<zeph_llm::any::AnyProvider>,
    /// Whether MCP tool pruning is enabled.  Mirrors `ToolPruningConfig::enabled`.
    pub(crate) pruning_enabled: bool,
    /// Pruning parameters snapshot.  Derived from `ToolPruningConfig` at build time.
    pub(crate) pruning_params: zeph_mcp::PruningParams,
    /// Pre-computed semantic tool index for embedding-based discovery (#2321).
    ///
    /// Built at connect time via `rebuild_semantic_index()`, rebuilt on tool list change.
    /// `None` when strategy is not `Embedding` or when build failed (fallback to all tools).
    pub(crate) semantic_index: Option<zeph_mcp::SemanticToolIndex>,
    /// Active discovery strategy and parameters.  Derived from `ToolDiscoveryConfig`.
    pub(crate) discovery_strategy: zeph_mcp::ToolDiscoveryStrategy,
    /// Discovery parameters snapshot.  Derived from `ToolDiscoveryConfig` at build time.
    pub(crate) discovery_params: zeph_mcp::DiscoveryParams,
    /// Dedicated embedding provider for tool discovery.  `None` = fall back to the
    /// agent's primary embedding provider.
    pub(crate) discovery_provider: Option<zeph_llm::any::AnyProvider>,
}

pub(crate) struct IndexState {
    pub(crate) retriever: Option<std::sync::Arc<zeph_index::retriever::CodeRetriever>>,
    pub(crate) repo_map_tokens: usize,
    pub(crate) cached_repo_map: Option<(String, std::time::Instant)>,
    pub(crate) repo_map_ttl: std::time::Duration,
}

pub(crate) struct RuntimeConfig {
    pub(crate) security: SecurityConfig,
    pub(crate) timeouts: TimeoutConfig,
    pub(crate) model_name: String,
    /// Configured name from `[[llm.providers]]` (the `name` field), set at startup and on
    /// `/provider` switch. Falls back to the provider type string when empty.
    pub(crate) active_provider_name: String,
    pub(crate) permission_policy: zeph_tools::PermissionPolicy,
    pub(crate) redact_credentials: bool,
    pub(crate) rate_limiter: super::rate_limiter::ToolRateLimiter,
    pub(crate) semantic_cache_enabled: bool,
    pub(crate) semantic_cache_threshold: f32,
    pub(crate) semantic_cache_max_candidates: u32,
    /// Dependency config snapshot stored for per-turn boost parameters.
    pub(crate) dependency_config: zeph_tools::DependencyConfig,
}

/// Groups feedback detection subsystems: correction detector, judge detector, and LLM classifier.
pub(crate) struct FeedbackState {
    pub(crate) detector: super::feedback_detector::FeedbackDetector,
    pub(crate) judge: Option<super::feedback_detector::JudgeDetector>,
    /// LLM-backed zero-shot classifier for `DetectorMode::Model`.
    /// When `Some`, `spawn_judge_correction_check` uses this instead of `JudgeDetector`.
    pub(crate) llm_classifier: Option<zeph_llm::classifier::llm::LlmClassifier>,
}

/// Groups security-related subsystems (sanitizer, quarantine, exfiltration guard).
pub(crate) struct SecurityState {
    pub(crate) sanitizer: ContentSanitizer,
    pub(crate) quarantine_summarizer: Option<QuarantinedSummarizer>,
    /// Whether this agent session is serving an ACP client.
    /// When `true` and `mcp_to_acp_boundary` is enabled, MCP tool results
    /// receive unconditional quarantine and cross-boundary audit logging.
    pub(crate) is_acp_session: bool,
    pub(crate) exfiltration_guard: zeph_sanitizer::exfiltration::ExfiltrationGuard,
    pub(crate) flagged_urls: HashSet<String>,
    /// URLs explicitly provided by the user across all turns in this session.
    /// Populated from raw user message text; cleared on `/clear`.
    /// Shared with `UrlGroundingVerifier` to check `fetch`/`web_scrape` calls at dispatch time.
    pub(crate) user_provided_urls: Arc<RwLock<HashSet<String>>>,
    pub(crate) pii_filter: zeph_sanitizer::pii::PiiFilter,
    /// NER classifier for PII detection (`classifiers.ner_model`). When `Some`, the PII path
    /// runs both regex (`pii_filter`) and NER, then merges spans before redaction.
    /// `None` when `classifiers` feature is disabled or `classifiers.enabled = false`.
    #[cfg(feature = "classifiers")]
    pub(crate) pii_ner_backend: Option<std::sync::Arc<dyn zeph_llm::classifier::ClassifierBackend>>,
    /// Per-call timeout for the NER PII classifier in milliseconds.
    #[cfg(feature = "classifiers")]
    pub(crate) pii_ner_timeout_ms: u64,
    pub(crate) memory_validator: zeph_sanitizer::memory_validation::MemoryWriteValidator,
    /// LLM-based prompt injection pre-screener (opt-in).
    #[cfg(feature = "guardrail")]
    pub(crate) guardrail: Option<zeph_sanitizer::guardrail::GuardrailFilter>,
    /// Post-LLM response verification layer.
    pub(crate) response_verifier: zeph_sanitizer::response_verifier::ResponseVerifier,
    /// Temporal causal IPI analyzer (opt-in, disabled when `None`).
    pub(crate) causal_analyzer: Option<zeph_sanitizer::causal_ipi::TurnCausalAnalyzer>,
}

/// Groups debug/diagnostics subsystems (dumper, trace collector, anomaly detector, logging config).
pub(crate) struct DebugState {
    pub(crate) debug_dumper: Option<crate::debug_dump::DebugDumper>,
    pub(crate) dump_format: crate::debug_dump::DumpFormat,
    pub(crate) trace_collector: Option<crate::debug_dump::trace::TracingCollector>,
    /// Monotonically increasing counter for `process_user_message` calls.
    /// Used to key spans in `trace_collector.active_iterations`.
    pub(crate) iteration_counter: usize,
    pub(crate) anomaly_detector: Option<zeph_tools::AnomalyDetector>,
    /// Whether to emit `reasoning_amplification` warnings for quality failures from reasoning
    /// models. Mirrors `AnomalyConfig::reasoning_model_warning`. Default: `true`.
    pub(crate) reasoning_model_warning: bool,
    pub(crate) logging_config: crate::config::LoggingConfig,
    /// Base dump directory — stored so `/dump-format trace` can create a `TracingCollector` (CR-04).
    pub(crate) dump_dir: Option<PathBuf>,
    /// Service name for `TracingCollector` created via runtime format switch (CR-04).
    pub(crate) trace_service_name: String,
    /// Whether to redact in `TracingCollector` created via runtime format switch (CR-04).
    pub(crate) trace_redact: bool,
    /// Span ID of the currently executing iteration — used by LLM/tool span wiring (CR-01).
    /// Set to `Some` at the start of `process_user_message`, cleared at end.
    pub(crate) current_iteration_span_id: Option<[u8; 8]>,
}

/// Groups agent lifecycle state: shutdown signaling, timing, and I/O notification channels.
pub(crate) struct LifecycleState {
    pub(crate) shutdown: watch::Receiver<bool>,
    pub(crate) start_time: Instant,
    pub(crate) cancel_signal: Arc<Notify>,
    pub(crate) cancel_token: CancellationToken,
    pub(crate) config_path: Option<PathBuf>,
    pub(crate) config_reload_rx: Option<mpsc::Receiver<ConfigEvent>>,
    pub(crate) warmup_ready: Option<watch::Receiver<bool>>,
    pub(crate) update_notify_rx: Option<mpsc::Receiver<String>>,
    pub(crate) custom_task_rx: Option<mpsc::Receiver<String>>,
}

/// Minimal config snapshot needed to reconstruct a provider at runtime via `/provider <name>`.
///
/// Secrets are stored as plain strings because [`Secret`] intentionally does not implement
/// `Clone`. They are re-wrapped in `Secret` when passed to `build_provider_for_switch`.
pub struct ProviderConfigSnapshot {
    pub claude_api_key: Option<String>,
    pub openai_api_key: Option<String>,
    pub gemini_api_key: Option<String>,
    pub compatible_api_keys: std::collections::HashMap<String, String>,
    pub llm_request_timeout_secs: u64,
    pub embedding_model: String,
}

/// Groups provider-related state: alternate providers, runtime switching, and compaction flags.
pub(crate) struct ProviderState {
    pub(crate) summary_provider: Option<AnyProvider>,
    /// Shared slot for runtime model switching; set by external caller (e.g. ACP).
    pub(crate) provider_override: Option<Arc<std::sync::RwLock<Option<AnyProvider>>>>,
    pub(crate) judge_provider: Option<AnyProvider>,
    /// Dedicated provider for compaction probe LLM calls. Falls back to `summary_provider`
    /// (or primary) when `None`.
    pub(crate) probe_provider: Option<AnyProvider>,
    /// Dedicated provider for `compress_context` LLM calls (#2356).
    /// Falls back to the primary provider when `None`.
    #[cfg(feature = "context-compression")]
    pub(crate) compress_provider: Option<AnyProvider>,
    pub(crate) cached_prompt_tokens: u64,
    /// Whether the active provider has server-side compaction enabled (Claude compact-2026-01-12).
    /// When true, client-side compaction is skipped.
    pub(crate) server_compaction_active: bool,
    pub(crate) stt: Option<Box<dyn SpeechToText>>,
    /// Snapshot of `[[llm.providers]]` entries for runtime `/provider` switching.
    pub(crate) provider_pool: Vec<ProviderEntry>,
    /// Resolved secrets and timeout settings needed to reconstruct providers at runtime.
    pub(crate) provider_config_snapshot: Option<ProviderConfigSnapshot>,
}

/// Groups metrics and cost tracking state.
pub(crate) struct MetricsState {
    pub(crate) metrics_tx: Option<watch::Sender<MetricsSnapshot>>,
    pub(crate) cost_tracker: Option<CostTracker>,
    pub(crate) token_counter: Arc<TokenCounter>,
    /// Set to `true` when Claude extended context (`enable_extended_context = true`) is active.
    /// Read from config at build time, not derived from provider internals.
    pub(crate) extended_context: bool,
    /// Shared classifier latency ring buffer. Populated by `ContentSanitizer` (injection, PII)
    /// and `LlmClassifier` (feedback). `None` when classifiers are not configured.
    pub(crate) classifier_metrics: Option<Arc<zeph_llm::ClassifierMetrics>>,
}

/// Groups task orchestration and subagent state.
pub(crate) struct OrchestrationState {
    /// On `OrchestrationState` (not `ProviderState`) because this provider is used exclusively
    /// by `LlmPlanner` during orchestration, not shared across subsystems.
    pub(crate) planner_provider: Option<AnyProvider>,
    /// Provider for `PlanVerifier` LLM calls. `None` falls back to the primary provider.
    /// On `OrchestrationState` for the same reason as `planner_provider`.
    pub(crate) verify_provider: Option<AnyProvider>,
    /// Graph waiting for `/plan confirm` before execution starts.
    pub(crate) pending_graph: Option<crate::orchestration::TaskGraph>,
    /// Cancellation token for the currently executing plan. `None` when no plan is running.
    /// Created fresh in `handle_plan_confirm()`, cancelled in `handle_plan_cancel()`.
    ///
    /// # Known limitation
    ///
    /// Token plumbing is ready; the delivery path requires the agent message loop to be
    /// restructured so `/plan cancel` can be received while `run_scheduler_loop` holds
    /// `&mut self`. See follow-up issue #1603 (SEC-M34-002).
    pub(crate) plan_cancel_token: Option<CancellationToken>,
    /// Manages spawned sub-agents.
    pub(crate) subagent_manager: Option<crate::subagent::SubAgentManager>,
    pub(crate) subagent_config: crate::config::SubAgentConfig,
    pub(crate) orchestration_config: crate::config::OrchestrationConfig,
    /// Lazily initialized plan template cache. `None` until first use or when
    /// memory (`SQLite`) is unavailable.
    pub(crate) plan_cache: Option<crate::orchestration::PlanCache>,
    /// Goal embedding from the most recent `plan_with_cache()` call. Consumed by
    /// `finalize_plan_execution()` to cache the completed plan template.
    pub(crate) pending_goal_embedding: Option<Vec<f32>>,
}

/// Groups instruction hot-reload state.
pub(crate) struct InstructionState {
    pub(crate) blocks: Vec<InstructionBlock>,
    pub(crate) reload_rx: Option<mpsc::Receiver<InstructionEvent>>,
    pub(crate) reload_state: Option<InstructionReloadState>,
}

/// Groups experiment feature state (gated behind `experiments` feature flag).
pub(crate) struct ExperimentState {
    #[cfg(feature = "experiments")]
    pub(crate) config: crate::config::ExperimentConfig,
    /// Cancellation token for a running experiment session. `Some` means an experiment is active.
    #[cfg(feature = "experiments")]
    pub(crate) cancel: Option<tokio_util::sync::CancellationToken>,
    /// Pre-built config snapshot used as the experiment baseline (agent path).
    #[cfg(feature = "experiments")]
    pub(crate) baseline: crate::experiments::ConfigSnapshot,
    /// Dedicated judge provider for evaluation. When `Some`, the evaluator uses this provider
    /// instead of the agent's primary provider, eliminating self-judge bias.
    #[cfg(feature = "experiments")]
    pub(crate) eval_provider: Option<AnyProvider>,
    /// Receives completion/error messages from the background experiment engine task.
    /// Always present so the select! branch compiles unconditionally.
    pub(crate) notify_rx: Option<tokio::sync::mpsc::Receiver<String>>,
    /// Sender end paired with `experiment_notify_rx`. Cloned into the background task.
    #[cfg(feature = "experiments")]
    pub(crate) notify_tx: tokio::sync::mpsc::Sender<String>,
}

/// Output of a background subgoal extraction LLM call.
#[cfg(feature = "context-compression")]
pub(crate) struct SubgoalExtractionResult {
    /// Current subgoal the agent is working toward.
    pub(crate) current: String,
    /// Just-completed subgoal, if the LLM detected a transition (`COMPLETED:` non-NONE).
    pub(crate) completed: Option<String>,
}

/// Groups context-compression feature state (gated behind `context-compression` feature flag).
#[cfg(feature = "context-compression")]
pub(crate) struct CompressionState {
    /// Cached task goal for TaskAware/MIG pruning. Set by `maybe_compact()`,
    /// invalidated when the last user message hash changes.
    pub(crate) current_task_goal: Option<String>,
    /// Hash of the last user message when `current_task_goal` was populated.
    pub(crate) task_goal_user_msg_hash: Option<u64>,
    /// Pending background task for goal extraction. Spawned fire-and-forget when the user message
    /// hash changes; result applied at the start of the next Soft compaction (#1909).
    pub(crate) pending_task_goal: Option<tokio::task::JoinHandle<Option<String>>>,
    /// Pending `SideQuest` eviction result from the background LLM call spawned last turn.
    /// Applied at the START of the next turn before compaction (PERF-1 fix).
    pub(crate) pending_sidequest_result: Option<tokio::task::JoinHandle<Option<Vec<usize>>>>,
    /// In-memory subgoal registry for `Subgoal`/`SubgoalMig` pruning strategies (#2022).
    pub(crate) subgoal_registry: crate::agent::compaction_strategy::SubgoalRegistry,
    /// Pending background subgoal extraction task.
    pub(crate) pending_subgoal: Option<tokio::task::JoinHandle<Option<SubgoalExtractionResult>>>,
    /// Hash of the last user message when subgoal extraction was scheduled.
    pub(crate) subgoal_user_msg_hash: Option<u64>,
}

/// Groups per-session I/O and policy state.
pub(crate) struct SessionState {
    pub(crate) env_context: EnvironmentContext,
    pub(crate) response_cache: Option<std::sync::Arc<zeph_memory::ResponseCache>>,
    /// Parent tool call ID when this agent runs as a subagent inside another agent session.
    /// Propagated into every `LoopbackEvent::ToolStart` / `ToolOutput` so the IDE can build
    /// a subagent hierarchy.
    pub(crate) parent_tool_use_id: Option<String>,
    /// Optional status channel for sending spinner/status messages to TUI or stderr.
    pub(crate) status_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    /// LSP context injection hooks. Fires after native tool execution, injects
    /// diagnostics/hover notes as `Role::System` messages before the next LLM call.
    #[cfg(feature = "lsp-context")]
    pub(crate) lsp_hooks: Option<crate::lsp_hooks::LspHookRunner>,
    /// Snapshot of the policy config for `/policy` command inspection.
    #[cfg(feature = "policy-enforcer")]
    pub(crate) policy_config: Option<zeph_tools::PolicyConfig>,
}

// Groups message buffering and image staging state.
pub(crate) struct MessageState {
    pub(crate) messages: Vec<Message>,
    // QueuedMessage is pub(super) in message_queue — same visibility as this struct; lint suppressed.
    #[allow(private_interfaces)]
    pub(crate) message_queue: VecDeque<QueuedMessage>,
    /// Image parts staged by `/image` commands, attached to the next user message.
    pub(crate) pending_image_parts: Vec<zeph_llm::provider::MessagePart>,
}

#[cfg(test)]
mod tests;
