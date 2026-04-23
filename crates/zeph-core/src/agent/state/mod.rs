// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Sub-struct definitions for the `Agent` struct.
//!
//! Each struct groups a related cluster of `Agent` fields.
//! All types are `pub(crate)` — visible only within the `zeph-core` crate.
//!
//! `MemoryState` is decomposed into four concern-separated sub-structs, each in its own file:
//!
//! - [`MemoryPersistenceState`] — `SQLite` handles, conversation IDs, recall budgets, autosave
//! - [`MemoryCompactionState`] — summarization thresholds, shutdown summary, digest, strategy
//! - [`MemoryExtractionState`] — graph config, RPE router, document config, semantic labels
//! - [`MemorySubsystemState`] — `TiMem`, `autoDream`, `MagicDocs`, microcompact

pub(crate) mod compaction;
pub(crate) mod extraction;
pub(crate) mod persistence;
pub(crate) mod subsystems;

pub(crate) use self::compaction::MemoryCompactionState;
pub(crate) use self::extraction::MemoryExtractionState;
pub(crate) use self::persistence::MemoryPersistenceState;
pub(crate) use self::subsystems::MemorySubsystemState;

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::RwLock;
use std::time::Instant;

use tokio::sync::{Notify, mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::Interval;
use tokio_util::sync::CancellationToken;
use zeph_llm::any::AnyProvider;
use zeph_llm::provider::Message;
use zeph_llm::stt::SpeechToText;

use crate::config::{ProviderEntry, SecurityConfig, SkillPromptMode, TimeoutConfig};
use crate::config_watcher::ConfigEvent;
use crate::context::EnvironmentContext;
use crate::cost::CostTracker;
use crate::file_watcher::FileChangedEvent;
use crate::instructions::{InstructionBlock, InstructionEvent, InstructionReloadState};
use crate::metrics::MetricsSnapshot;
use crate::vault::Secret;
use zeph_config;
use zeph_memory::TokenCounter;
use zeph_sanitizer::ContentSanitizer;
use zeph_sanitizer::quarantine::QuarantinedSummarizer;
use zeph_skills::matcher::SkillMatcherBackend;
use zeph_skills::registry::SkillRegistry;
use zeph_skills::watcher::SkillEvent;

use super::message_queue::QueuedMessage;

/// Coordinator struct holding four concern-separated sub-structs for memory management.
///
/// Each sub-struct groups fields by a single concern:
/// - [`persistence`](MemoryPersistenceState) — `SQLite` handles, conversation IDs, recall budgets
/// - [`compaction`](MemoryCompactionState) — summarization thresholds, shutdown summary, digest
/// - [`extraction`](MemoryExtractionState) — graph config, RPE router, semantic labels
/// - [`subsystems`](MemorySubsystemState) — `TiMem`, `autoDream`, `MagicDocs`, microcompact
#[derive(Default)]
pub(crate) struct MemoryState {
    /// `SQLite` handles, conversation IDs, recall budgets, and autosave policy.
    pub(crate) persistence: MemoryPersistenceState,
    /// Summarization thresholds, shutdown summary, digest config, and context strategy.
    pub(crate) compaction: MemoryCompactionState,
    /// Graph extraction config, RPE router, document config, and semantic label configs.
    pub(crate) extraction: MemoryExtractionState,
    /// `TiMem`, `autoDream`, `MagicDocs`, and microcompact subsystem state.
    pub(crate) subsystems: MemorySubsystemState,
}

pub(crate) struct SkillState {
    pub(crate) registry: Arc<RwLock<SkillRegistry>>,
    /// Per-turn trust snapshot written by `prepare_context` after `build_skill_trust_map`.
    /// Shared with `SkillInvokeExecutor` so it can resolve trust without hitting `SQLite`
    /// on every tool call. Refreshed once per turn — stale by at most one turn.
    pub(crate) trust_snapshot: Arc<RwLock<HashMap<String, zeph_common::SkillTrustLevel>>>,
    pub(crate) skill_paths: Vec<PathBuf>,
    pub(crate) managed_dir: Option<PathBuf>,
    pub(crate) trust_config: crate::config::TrustConfig,
    pub(crate) matcher: Option<SkillMatcherBackend>,
    pub(crate) max_active_skills: usize,
    pub(crate) disambiguation_threshold: f32,
    pub(crate) min_injection_score: f32,
    pub(crate) embedding_model: String,
    pub(crate) skill_reload_rx: Option<mpsc::Receiver<SkillEvent>>,
    /// Resolves the current set of per-plugin skill dirs at reload time.
    ///
    /// Called inside `reload_skills()` so that plugins installed via `/plugins add` after
    /// startup are discovered on the next watcher event without restarting the agent.
    pub(crate) plugin_dirs_supplier: Option<Arc<dyn Fn() -> Vec<PathBuf> + Send + Sync>>,
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
    /// `SkillOrchestra` RL routing head. `Some` when `rl_routing_enabled = true` and
    /// weights are loaded or initialized. `None` when RL routing is disabled.
    pub(crate) rl_head: Option<zeph_skills::rl_head::RoutingHead>,
    /// Blend weight for RL routing: `final = (1-rl_weight)*cosine + rl_weight*rl_score`.
    pub(crate) rl_weight: f32,
    /// Skip RL blending for the first N updates (cold-start warmup).
    pub(crate) rl_warmup_updates: u32,
    /// Directory where `/skill create` writes generated skills.
    /// Defaults to `managed_dir` if `None`.
    pub(crate) generation_output_dir: Option<std::path::PathBuf>,
    /// Provider name for `/skill create` generation. Empty = primary.
    pub(crate) generation_provider_name: String,
}

pub(crate) struct McpState {
    pub(crate) tools: Vec<zeph_mcp::McpTool>,
    pub(crate) registry: Option<zeph_mcp::McpToolRegistry>,
    pub(crate) manager: Option<std::sync::Arc<zeph_mcp::McpManager>>,
    pub(crate) allowed_commands: Vec<String>,
    pub(crate) max_dynamic: usize,
    /// Receives elicitation requests from MCP server handlers during tool execution.
    /// When `Some`, the agent loop must process these concurrently with tool result awaiting
    /// to avoid deadlock (tool result waits for elicitation, elicitation waits for agent loop).
    pub(crate) elicitation_rx: Option<tokio::sync::mpsc::Receiver<zeph_mcp::ElicitationEvent>>,
    /// Shared with `McpToolExecutor` so native `tool_use` sees the current tool list.
    ///
    /// Two methods write to this `RwLock` — ordering matters:
    /// - `sync_executor_tools()`: writes the **full** `self.tools` set.
    /// - `apply_pruned_tools()`: writes the **pruned** subset (used after pruning).
    ///
    /// Within a turn, `sync_executor_tools` must always run **before**
    /// `apply_pruned_tools`.  The normal call order guarantees this: tool-list
    /// change events call `sync_executor_tools` (inside `check_tool_refresh`,
    /// `handle_mcp_add`, `handle_mcp_remove`), and pruning runs later inside
    /// `rebuild_system_prompt`.  See also: `apply_pruned_tools`.
    pub(crate) shared_tools: Option<Arc<RwLock<Vec<zeph_mcp::McpTool>>>>,
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
    /// When `true`, show a security warning before prompting for fields whose names
    /// match sensitive patterns (password, token, secret, key, credential, etc.).
    pub(crate) elicitation_warn_sensitive_fields: bool,
    /// When `true`, semantic index and registry need to be rebuilt at the next opportunity.
    ///
    /// Set after `/mcp add` or `/mcp remove` when called via `AgentAccess::handle_mcp`,
    /// which cannot call `rebuild_semantic_index` and `sync_mcp_registry` directly because
    /// those are `async fn(&mut self)` and their futures are `!Send` (they hold `&mut Agent<C>`
    /// across `.await`). The rebuild is deferred to `check_tool_refresh`, which runs at the
    /// start of each turn without the `Box<dyn Future + Send>` constraint.
    pub(crate) pending_semantic_rebuild: bool,
}

pub(crate) struct IndexState {
    pub(crate) retriever: Option<std::sync::Arc<zeph_index::retriever::CodeRetriever>>,
    pub(crate) repo_map_tokens: usize,
    pub(crate) cached_repo_map: Option<(String, std::time::Instant)>,
    pub(crate) repo_map_ttl: std::time::Duration,
}

/// Snapshot of adversarial policy gate configuration for status display.
#[derive(Debug, Clone)]
pub struct AdversarialPolicyInfo {
    pub provider: String,
    pub policy_count: usize,
    pub fail_open: bool,
}

#[allow(clippy::struct_excessive_bools)]
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
    /// Adversarial policy gate runtime info for /status display.
    pub(crate) adversarial_policy_info: Option<AdversarialPolicyInfo>,
    /// Current spawn depth of this agent instance (0 = top-level, 1 = first sub-agent, etc.).
    /// Used by `build_spawn_context()` to propagate depth to children.
    pub(crate) spawn_depth: u32,
    /// Inject `<budget>` XML into the volatile system prompt section (#2267).
    pub(crate) budget_hint_enabled: bool,
    /// Per-channel skill allowlist. Skills not matching the allowlist are excluded from the
    /// prompt. An empty `allowed` list means all skills are permitted (default).
    pub(crate) channel_skills: zeph_config::ChannelSkillsConfig,
    /// Minimum allowed interval for `/loop` ticks (seconds). Sourced from `[cli.loop] min_interval_secs`.
    pub(crate) loop_min_interval_secs: u64,
    /// Runtime middleware layers for LLM calls and tool dispatch (#2286).
    ///
    /// Default: empty vec (zero-cost — loops never iterate).
    pub(crate) layers: Vec<std::sync::Arc<dyn crate::runtime_layer::RuntimeLayer>>,
    /// Background supervisor config snapshot for turn-boundary abort logic.
    pub(crate) supervisor_config: crate::config::TaskSupervisorConfig,
    /// Session recap config (#3064).
    pub(crate) recap_config: zeph_config::RecapConfig,
    /// ACP server configuration snapshot for `/acp` slash-command display.
    pub(crate) acp_config: zeph_config::AcpConfig,
    /// Set to `true` after the auto-recap is emitted at session resume (#3144).
    ///
    /// Used by `/recap` to skip a redundant LLM call when no new messages have
    /// been added since the auto-recap was shown.
    pub(crate) auto_recap_shown: bool,
    /// Number of non-system messages present when the session was resumed (#3144).
    ///
    /// Combined with `auto_recap_shown` to detect whether the user has added new
    /// messages after the auto-recap was shown.
    pub(crate) msg_count_at_resume: usize,
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
    /// Maximum number of bytes passed to the NER PII classifier per call.
    ///
    /// Large tool outputs (e.g. `search_code`) can produce 150+ `DeBERTa` chunks and exceed
    /// the per-call timeout. Input is truncated at a valid UTF-8 boundary before classification.
    #[cfg(feature = "classifiers")]
    pub(crate) pii_ner_max_chars: usize,
    /// Circuit-breaker threshold: number of consecutive timeouts before NER is disabled.
    /// `0` means the circuit breaker is disabled (NER is always attempted).
    #[cfg(feature = "classifiers")]
    pub(crate) pii_ner_circuit_breaker_threshold: u32,
    /// Number of consecutive NER timeouts observed since the last successful call.
    #[cfg(feature = "classifiers")]
    pub(crate) pii_ner_consecutive_timeouts: u32,
    /// Set to `true` when the circuit breaker trips. NER is skipped for the rest of the session.
    #[cfg(feature = "classifiers")]
    pub(crate) pii_ner_tripped: bool,
    pub(crate) memory_validator: zeph_sanitizer::memory_validation::MemoryWriteValidator,
    /// LLM-based prompt injection pre-screener (opt-in).
    pub(crate) guardrail: Option<zeph_sanitizer::guardrail::GuardrailFilter>,
    /// Post-LLM response verification layer.
    pub(crate) response_verifier: zeph_sanitizer::response_verifier::ResponseVerifier,
    /// Temporal causal IPI analyzer (opt-in, disabled when `None`).
    pub(crate) causal_analyzer: Option<zeph_sanitizer::causal_ipi::TurnCausalAnalyzer>,
    /// VIGIL pre-sanitizer gate. `None` for subagent sessions (subagents are exempt).
    /// Set at agent build time for top-level agents; skipped for subagents (high FP rate).
    pub(crate) vigil: Option<crate::agent::vigil::VigilGate>,
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

/// Snapshot of the shell-level overlay baked in at startup.
///
/// Used in `reload_config` to detect when a hot-reload would produce a different shell
/// restriction set than the one baked into the live `ShellExecutor` (M4 warn-on-divergence).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShellOverlaySnapshot {
    /// Sorted `blocked_commands` contributed by plugins.
    pub blocked: Vec<String>,
    /// Sorted `allowed_commands` after plugin intersection (empty if base was empty).
    pub allowed: Vec<String>,
}

/// Runtime state for an active `/loop` session.
///
/// At most one loop is active at a time; `LifecycleState::user_loop` holds `Some` while
/// the loop is running and `None` otherwise.
pub(crate) struct LoopState {
    /// The prompt text injected on each tick.
    pub(crate) prompt: String,
    /// Number of ticks fired so far.
    pub(crate) iteration: u64,
    /// Tick interval. `MissedTickBehavior::Skip` prevents burst catch-up.
    pub(crate) interval: Interval,
    /// Cancel handle. Dropped (and token cancelled) when loop is stopped.
    pub(crate) cancel_tx: CancellationToken,
}

/// Groups agent lifecycle state: shutdown signaling, timing, and I/O notification channels.
pub(crate) struct LifecycleState {
    pub(crate) shutdown: watch::Receiver<bool>,
    pub(crate) start_time: Instant,
    pub(crate) cancel_signal: Arc<Notify>,
    pub(crate) cancel_token: CancellationToken,
    /// Handle to the cancel bridge task spawned each turn. Aborted before a new one is created
    /// to prevent unbounded task accumulation across turns.
    pub(crate) cancel_bridge_handle: Option<JoinHandle<()>>,
    pub(crate) config_path: Option<PathBuf>,
    pub(crate) config_reload_rx: Option<mpsc::Receiver<ConfigEvent>>,
    /// Path to the plugins directory; used to re-apply overlays on hot-reload.
    pub(crate) plugins_dir: PathBuf,
    /// Shell overlay snapshot baked in at startup. Used to detect divergence on hot-reload.
    pub(crate) startup_shell_overlay: ShellOverlaySnapshot,
    /// Handle for live-rebuilding the `ShellExecutor`'s `blocked_commands` policy on hot-reload.
    /// `None` when no `ShellExecutor` is in the executor chain (test harnesses, daemon-only modes).
    pub(crate) shell_policy_handle: Option<zeph_tools::ShellPolicyHandle>,
    pub(crate) warmup_ready: Option<watch::Receiver<bool>>,
    pub(crate) update_notify_rx: Option<mpsc::Receiver<String>>,
    pub(crate) custom_task_rx: Option<mpsc::Receiver<String>>,
    /// Active `/loop` state. `None` when no loop is running.
    pub(crate) user_loop: Option<LoopState>,
    /// Last known process cwd. Compared after each tool call to detect changes.
    pub(crate) last_known_cwd: PathBuf,
    /// Receiver for file-change events from `FileChangeWatcher`. `None` when no paths configured.
    pub(crate) file_changed_rx: Option<mpsc::Receiver<FileChangedEvent>>,
    /// Keeps the `FileChangeWatcher` alive for the agent's lifetime. Dropping it aborts the watcher task.
    pub(crate) file_watcher: Option<crate::file_watcher::FileChangeWatcher>,
    /// Supervised background task manager. Owned by the agent; call `reap()` between turns
    /// and `abort_all()` on shutdown.
    pub(crate) supervisor: super::agent_supervisor::BackgroundSupervisor,
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
    pub(crate) provider_override: Option<Arc<RwLock<Option<AnyProvider>>>>,
    pub(crate) judge_provider: Option<AnyProvider>,
    /// Dedicated provider for compaction probe LLM calls. Falls back to `summary_provider`
    /// (or primary) when `None`.
    pub(crate) probe_provider: Option<AnyProvider>,
    /// Dedicated provider for `compress_context` LLM calls (#2356).
    /// Falls back to the primary provider when `None`.
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
    /// Rolling window of per-turn latency samples (last 10 turns).
    pub(crate) timing_window: std::collections::VecDeque<crate::metrics::TurnTimings>,
    /// Accumulator for the current turn's timings. Flushed at turn end via `flush_turn_timings`.
    pub(crate) pending_timings: crate::metrics::TurnTimings,
    /// Optional histogram recorder for per-event Prometheus observations.
    /// `None` when the `prometheus` feature is disabled or metrics are not enabled.
    pub(crate) histogram_recorder: Option<std::sync::Arc<dyn crate::metrics::HistogramRecorder>>,
}

/// Groups task orchestration and subagent state.
#[derive(Default)]
pub(crate) struct OrchestrationState {
    /// On `OrchestrationState` (not `ProviderState`) because this provider is used exclusively
    /// by `LlmPlanner` during orchestration, not shared across subsystems.
    pub(crate) planner_provider: Option<AnyProvider>,
    /// Provider for `PlanVerifier` LLM calls. `None` falls back to the primary provider.
    /// On `OrchestrationState` for the same reason as `planner_provider`.
    pub(crate) verify_provider: Option<AnyProvider>,
    /// Graph waiting for `/plan confirm` before execution starts.
    pub(crate) pending_graph: Option<zeph_orchestration::TaskGraph>,
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
    pub(crate) subagent_manager: Option<zeph_subagent::SubAgentManager>,
    pub(crate) subagent_config: crate::config::SubAgentConfig,
    pub(crate) orchestration_config: crate::config::OrchestrationConfig,
    /// Lazily initialized plan template cache. `None` until first use or when
    /// memory (`SQLite`) is unavailable.
    #[allow(dead_code)]
    pub(crate) plan_cache: Option<zeph_orchestration::PlanCache>,
    /// Goal embedding from the most recent `plan_with_cache()` call. Consumed by
    /// `finalize_plan_execution()` to cache the completed plan template.
    pub(crate) pending_goal_embedding: Option<Vec<f32>>,
    /// `AdaptOrch` topology advisor — `None` when `[orchestration.adaptorch]` is disabled.
    pub(crate) topology_advisor: Option<std::sync::Arc<zeph_orchestration::TopologyAdvisor>>,
    /// Last `AdaptOrch` verdict; carried from `handle_plan_goal_as_string` to scheduler loop
    /// for `record_outcome`.
    #[allow(dead_code)] // read via .take() in plan.rs; clippy false positive
    pub(crate) last_advisor_verdict: Option<zeph_orchestration::AdvisorVerdict>,
    /// Task graph persistence handle. `None` when no `SemanticMemory` was
    /// attached via `with_memory`, or when
    /// `OrchestrationConfig::persistence_enabled` is `false`. When `Some`, the
    /// scheduler loop snapshots the graph once per tick and `/plan resume <id>`
    /// rehydrates from disk.
    pub(crate) graph_persistence: Option<
        zeph_orchestration::GraphPersistence<zeph_memory::store::graph_store::TaskGraphStore>,
    >,
}

/// Groups instruction hot-reload state.
#[derive(Default)]
pub(crate) struct InstructionState {
    pub(crate) blocks: Vec<InstructionBlock>,
    pub(crate) reload_rx: Option<mpsc::Receiver<InstructionEvent>>,
    pub(crate) reload_state: Option<InstructionReloadState>,
}

/// Groups experiment feature state (gated behind `experiments` feature flag).
pub(crate) struct ExperimentState {
    pub(crate) config: crate::config::ExperimentConfig,
    /// Cancellation token for a running experiment session. `Some` means an experiment is active.
    pub(crate) cancel: Option<tokio_util::sync::CancellationToken>,
    /// Pre-built config snapshot used as the experiment baseline (agent path).
    pub(crate) baseline: zeph_experiments::ConfigSnapshot,
    /// Dedicated judge provider for evaluation. When `Some`, the evaluator uses this provider
    /// instead of the agent's primary provider, eliminating self-judge bias.
    pub(crate) eval_provider: Option<AnyProvider>,
    /// Receives completion/error messages from the background experiment engine task.
    /// Always present so the select! branch compiles unconditionally.
    pub(crate) notify_rx: Option<tokio::sync::mpsc::Receiver<String>>,
    /// Sender end paired with `experiment_notify_rx`. Cloned into the background task.
    pub(crate) notify_tx: tokio::sync::mpsc::Sender<String>,
}

/// Output of a background subgoal extraction LLM call.
pub(crate) struct SubgoalExtractionResult {
    /// Current subgoal the agent is working toward.
    pub(crate) current: String,
    /// Just-completed subgoal, if the LLM detected a transition (`COMPLETED:` non-NONE).
    pub(crate) completed: Option<String>,
}

/// Groups context-compression feature state (gated behind `context-compression` feature flag).
#[derive(Default)]
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

/// Groups runtime tool filtering, dependency tracking, and iteration bookkeeping.
#[derive(Default)]
pub(crate) struct ToolState {
    /// Dynamic tool schema filter: pre-computed tool embeddings for per-turn filtering (#2020).
    pub(crate) tool_schema_filter: Option<zeph_tools::ToolSchemaFilter>,
    /// Cached filtered tool IDs for the current user turn.
    pub(crate) cached_filtered_tool_ids: Option<HashSet<String>>,
    /// Tool dependency graph for sequential tool availability (#2024).
    pub(crate) dependency_graph: Option<zeph_tools::ToolDependencyGraph>,
    /// Always-on tool IDs, mirrored from the tool schema filter for dependency gate bypass.
    pub(crate) dependency_always_on: HashSet<String>,
    /// Tool IDs that completed successfully in the current session.
    pub(crate) completed_tool_ids: HashSet<String>,
    /// Current tool loop iteration index within the active user turn.
    pub(crate) current_tool_iteration: usize,
}

/// Groups per-session I/O and policy state.
pub(crate) struct SessionState {
    pub(crate) env_context: EnvironmentContext,
    /// Timestamp of the last assistant message appended to context.
    /// Used by time-based microcompact to compute session idle gap (#2699).
    /// `None` before the first assistant response.
    pub(crate) last_assistant_at: Option<Instant>,
    pub(crate) response_cache: Option<std::sync::Arc<zeph_memory::ResponseCache>>,
    /// Parent tool call ID when this agent runs as a subagent inside another agent session.
    /// Propagated into every `LoopbackEvent::ToolStart` / `ToolOutput` so the IDE can build
    /// a subagent hierarchy.
    pub(crate) parent_tool_use_id: Option<String>,
    /// Current-turn intent snapshot for VIGIL. `None` between turns.
    ///
    /// Set at the top of `process_user_message` (before any tool call) to the first 1024 chars
    /// of the user message. Cleared at `end_turn`, on `/clear`, and on any turn-abort path.
    /// Never shared across turns or propagated into subagents.
    pub(crate) current_turn_intent: Option<String>,
    /// Optional status channel for sending spinner/status messages to TUI or stderr.
    pub(crate) status_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    /// LSP context injection hooks. Fires after native tool execution, injects
    /// diagnostics/hover notes as `Role::System` messages before the next LLM call.
    pub(crate) lsp_hooks: Option<crate::lsp_hooks::LspHookRunner>,
    /// Snapshot of the policy config for `/policy` command inspection.
    pub(crate) policy_config: Option<zeph_tools::PolicyConfig>,
    /// `CwdChanged` hook definitions extracted from `[hooks]` config.
    pub(crate) hooks_config: HooksConfigSnapshot,
}

/// Extracted hook lists from `[hooks]` config, stored in `SessionState`.
#[derive(Default)]
pub(crate) struct HooksConfigSnapshot {
    /// Hooks fired when working directory changes.
    pub(crate) cwd_changed: Vec<zeph_config::HookDef>,
    /// Hooks fired when a watched file changes.
    pub(crate) file_changed_hooks: Vec<zeph_config::HookDef>,
}

// Groups message buffering and image staging state.
pub(crate) struct MessageState {
    pub(crate) messages: Vec<Message>,
    // QueuedMessage is pub(super) in message_queue — same visibility as this struct; lint suppressed.
    #[allow(private_interfaces)]
    pub(crate) message_queue: VecDeque<QueuedMessage>,
    /// Image parts staged by `/image` commands, attached to the next user message.
    pub(crate) pending_image_parts: Vec<zeph_llm::provider::MessagePart>,
    /// DB row ID of the most recently persisted message. Set by `persist_message`;
    /// consumed by `push_message` call sites to populate `metadata.db_id` on in-memory messages.
    pub(crate) last_persisted_message_id: Option<i64>,
    /// DB message IDs pending hide after deferred tool pair summarization.
    pub(crate) deferred_db_hide_ids: Vec<i64>,
    /// Summary texts pending insertion after deferred tool pair summarization.
    pub(crate) deferred_db_summaries: Vec<String>,
}

impl McpState {
    /// Write the **full** `self.tools` set to the shared executor `RwLock`.
    ///
    /// This is the first of two writers to `shared_tools`. Within a turn this method must run
    /// **before** `apply_pruned_tools`, which writes the pruned subset. The normal call order
    /// guarantees this: tool-list change events call this method, and pruning runs later inside
    /// `rebuild_system_prompt`. See also: `apply_pruned_tools`.
    pub(crate) fn sync_executor_tools(&self) {
        if let Some(ref shared) = self.shared_tools {
            shared.write().clone_from(&self.tools);
        }
    }

    /// Write the **pruned** tool subset to the shared executor `RwLock`.
    ///
    /// Must only be called **after** `sync_executor_tools` has established the full tool set for
    /// the current turn. `self.tools` (the full set) is intentionally **not** modified.
    ///
    /// This method must **NOT** call `sync_executor_tools` internally — doing so would overwrite
    /// the pruned subset with the full set. See also: `sync_executor_tools`.
    pub(crate) fn apply_pruned_tools(&self, pruned: Vec<zeph_mcp::McpTool>) {
        debug_assert!(
            pruned.iter().all(|p| self
                .tools
                .iter()
                .any(|t| t.server_id == p.server_id && t.name == p.name)),
            "pruned set must be a subset of self.tools"
        );
        if let Some(ref shared) = self.shared_tools {
            *shared.write() = pruned;
        }
    }

    #[cfg(test)]
    pub(crate) fn tool_count(&self) -> usize {
        self.tools.len()
    }
}

impl IndexState {
    #[tracing::instrument(name = "core.index.fetch_code_rag", skip(self), fields(%query, token_budget))]
    pub(crate) async fn fetch_code_rag(
        &self,
        query: &str,
        token_budget: usize,
    ) -> Result<Option<String>, crate::agent::error::AgentError> {
        let Some(retriever) = &self.retriever else {
            return Ok(None);
        };
        if token_budget == 0 {
            return Ok(None);
        }

        let result = retriever
            .retrieve(query, token_budget)
            .await
            .map_err(|e| crate::agent::error::AgentError::Other(format!("{e:#}")))?;
        let context_text = zeph_index::retriever::format_as_context(&result);

        if context_text.is_empty() {
            Ok(None)
        } else {
            tracing::debug!(
                strategy = ?result.strategy,
                chunks = result.chunks.len(),
                tokens = result.total_tokens,
                "code context fetched"
            );
            Ok(Some(context_text))
        }
    }

    /// Return `Some(self)` when code indexing is enabled, `None` otherwise.
    ///
    /// Used by `prepare_context` to pass an optional `IndexAccess` reference to
    /// `zeph_context::assembler::ContextAssembler::gather` without wrapping the whole state.
    pub(crate) fn as_index_access(&self) -> Option<&dyn zeph_context::input::IndexAccess> {
        if self.retriever.is_some() {
            Some(self)
        } else {
            None
        }
    }
}

impl DebugState {
    pub(crate) fn start_iteration_span(&mut self, iteration_index: usize, text: &str) {
        if let Some(ref mut tc) = self.trace_collector {
            tc.begin_iteration(iteration_index, text);
            self.current_iteration_span_id = tc.current_iteration_span_id(iteration_index);
        }
    }

    pub(crate) fn end_iteration_span(
        &mut self,
        iteration_index: usize,
        status: crate::debug_dump::trace::SpanStatus,
    ) {
        if let Some(ref mut tc) = self.trace_collector {
            tc.end_iteration(iteration_index, status);
        }
        self.current_iteration_span_id = None;
    }

    pub(crate) fn switch_format(&mut self, new_format: crate::debug_dump::DumpFormat) {
        let was_trace = self.dump_format == crate::debug_dump::DumpFormat::Trace;
        let now_trace = new_format == crate::debug_dump::DumpFormat::Trace;

        if now_trace
            && !was_trace
            && let Some(ref dump_dir) = self.dump_dir.clone()
        {
            let service_name = self.trace_service_name.clone();
            let redact = self.trace_redact;
            match crate::debug_dump::trace::TracingCollector::new(
                dump_dir.as_path(),
                &service_name,
                redact,
                None,
            ) {
                Ok(collector) => {
                    self.trace_collector = Some(collector);
                }
                Err(e) => {
                    tracing::warn!(error = %e, "failed to create TracingCollector on format switch");
                }
            }
        }
        if was_trace
            && !now_trace
            && let Some(mut tc) = self.trace_collector.take()
        {
            tc.finish();
        }

        self.dump_format = new_format;
    }

    pub(crate) fn write_chat_debug_dump(
        &self,
        dump_id: Option<u32>,
        result: &zeph_llm::provider::ChatResponse,
        pii_filter: &zeph_sanitizer::pii::PiiFilter,
    ) {
        let Some((d, id)) = self.debug_dumper.as_ref().zip(dump_id) else {
            return;
        };
        let raw = match result {
            zeph_llm::provider::ChatResponse::Text(t) => t.clone(),
            zeph_llm::provider::ChatResponse::ToolUse {
                text, tool_calls, ..
            } => {
                let calls = serde_json::to_string_pretty(tool_calls).unwrap_or_default();
                format!(
                    "{}\n\n---TOOL_CALLS---\n{calls}",
                    text.as_deref().unwrap_or("")
                )
            }
        };
        let text = if pii_filter.is_enabled() {
            pii_filter.scrub(&raw).into_owned()
        } else {
            raw
        };
        d.dump_response(id, &text);
    }
}

impl Default for McpState {
    fn default() -> Self {
        Self {
            tools: Vec::new(),
            registry: None,
            manager: None,
            allowed_commands: Vec::new(),
            max_dynamic: 10,
            elicitation_rx: None,
            shared_tools: None,
            tool_rx: None,
            server_outcomes: Vec::new(),
            pruning_cache: zeph_mcp::PruningCache::new(),
            pruning_provider: None,
            pruning_enabled: false,
            pruning_params: zeph_mcp::PruningParams::default(),
            semantic_index: None,
            discovery_strategy: zeph_mcp::ToolDiscoveryStrategy::default(),
            discovery_params: zeph_mcp::DiscoveryParams::default(),
            discovery_provider: None,
            elicitation_warn_sensitive_fields: true,
            pending_semantic_rebuild: false,
        }
    }
}

impl Default for IndexState {
    fn default() -> Self {
        Self {
            retriever: None,
            repo_map_tokens: 0,
            cached_repo_map: None,
            repo_map_ttl: std::time::Duration::from_mins(5),
        }
    }
}

impl Default for DebugState {
    fn default() -> Self {
        Self {
            debug_dumper: None,
            dump_format: crate::debug_dump::DumpFormat::default(),
            trace_collector: None,
            iteration_counter: 0,
            anomaly_detector: None,
            reasoning_model_warning: true,
            logging_config: crate::config::LoggingConfig::default(),
            dump_dir: None,
            trace_service_name: String::new(),
            trace_redact: true,
            current_iteration_span_id: None,
        }
    }
}

impl Default for FeedbackState {
    fn default() -> Self {
        Self {
            detector: super::feedback_detector::FeedbackDetector::new(0.6),
            judge: None,
            llm_classifier: None,
        }
    }
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            security: SecurityConfig::default(),
            timeouts: TimeoutConfig::default(),
            model_name: String::new(),
            active_provider_name: String::new(),
            permission_policy: zeph_tools::PermissionPolicy::default(),
            redact_credentials: true,
            rate_limiter: super::rate_limiter::ToolRateLimiter::new(
                super::rate_limiter::RateLimitConfig::default(),
            ),
            semantic_cache_enabled: false,
            semantic_cache_threshold: 0.95,
            semantic_cache_max_candidates: 10,
            dependency_config: zeph_tools::DependencyConfig::default(),
            adversarial_policy_info: None,
            spawn_depth: 0,
            budget_hint_enabled: true,
            channel_skills: zeph_config::ChannelSkillsConfig::default(),
            loop_min_interval_secs: 5,
            layers: Vec::new(),
            supervisor_config: crate::config::TaskSupervisorConfig::default(),
            recap_config: zeph_config::RecapConfig::default(),
            acp_config: zeph_config::AcpConfig::default(),
            auto_recap_shown: false,
            msg_count_at_resume: 0,
        }
    }
}

impl SessionState {
    pub(crate) fn new() -> Self {
        Self {
            env_context: EnvironmentContext::gather(""),
            last_assistant_at: None,
            response_cache: None,
            parent_tool_use_id: None,
            current_turn_intent: None,
            status_tx: None,
            lsp_hooks: None,
            policy_config: None,
            hooks_config: HooksConfigSnapshot::default(),
        }
    }
}

impl SkillState {
    pub(crate) fn new(
        registry: Arc<RwLock<SkillRegistry>>,
        matcher: Option<SkillMatcherBackend>,
        max_active_skills: usize,
        last_skills_prompt: String,
    ) -> Self {
        Self {
            registry,
            trust_snapshot: Arc::new(RwLock::new(HashMap::new())),
            skill_paths: Vec::new(),
            managed_dir: None,
            trust_config: crate::config::TrustConfig::default(),
            matcher,
            max_active_skills,
            disambiguation_threshold: 0.20,
            min_injection_score: 0.20,
            embedding_model: String::new(),
            skill_reload_rx: None,
            plugin_dirs_supplier: None,
            active_skill_names: Vec::new(),
            last_skills_prompt,
            prompt_mode: crate::config::SkillPromptMode::Auto,
            available_custom_secrets: HashMap::new(),
            cosine_weight: 0.7,
            hybrid_search: false,
            bm25_index: None,
            two_stage_matching: false,
            confusability_threshold: 0.0,
            rl_head: None,
            rl_weight: 0.3,
            rl_warmup_updates: 50,
            generation_output_dir: None,
            generation_provider_name: String::new(),
        }
    }
}

impl LifecycleState {
    pub(crate) fn new() -> Self {
        let (_tx, rx) = watch::channel(false);
        Self {
            shutdown: rx,
            start_time: Instant::now(),
            cancel_signal: Arc::new(tokio::sync::Notify::new()),
            cancel_token: tokio_util::sync::CancellationToken::new(),
            cancel_bridge_handle: None,
            config_path: None,
            config_reload_rx: None,
            plugins_dir: PathBuf::new(),
            startup_shell_overlay: ShellOverlaySnapshot::default(),
            shell_policy_handle: None,
            warmup_ready: None,
            update_notify_rx: None,
            custom_task_rx: None,
            user_loop: None,
            last_known_cwd: std::env::current_dir().unwrap_or_default(),
            file_changed_rx: None,
            file_watcher: None,
            supervisor: super::agent_supervisor::BackgroundSupervisor::new(
                &crate::config::TaskSupervisorConfig::default(),
                None,
            ),
        }
    }
}

impl ProviderState {
    pub(crate) fn new(initial_prompt_tokens: u64) -> Self {
        Self {
            summary_provider: None,
            provider_override: None,
            judge_provider: None,
            probe_provider: None,
            compress_provider: None,
            cached_prompt_tokens: initial_prompt_tokens,
            server_compaction_active: false,
            stt: None,
            provider_pool: Vec::new(),
            provider_config_snapshot: None,
        }
    }
}

impl MetricsState {
    pub(crate) fn new(token_counter: Arc<zeph_memory::TokenCounter>) -> Self {
        Self {
            metrics_tx: None,
            cost_tracker: None,
            token_counter,
            extended_context: false,
            classifier_metrics: None,
            timing_window: std::collections::VecDeque::new(),
            pending_timings: crate::metrics::TurnTimings::default(),
            histogram_recorder: None,
        }
    }
}

impl ExperimentState {
    pub(crate) fn new() -> Self {
        let (notify_tx, notify_rx) = tokio::sync::mpsc::channel::<String>(4);
        Self {
            config: crate::config::ExperimentConfig::default(),
            cancel: None,
            baseline: zeph_experiments::ConfigSnapshot::default(),
            eval_provider: None,
            notify_rx: Some(notify_rx),
            notify_tx,
        }
    }
}

pub(super) mod security;
pub(super) mod skill;

#[cfg(test)]
mod tests;
