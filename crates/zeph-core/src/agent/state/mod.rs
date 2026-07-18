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
pub(crate) mod runtime;
pub(crate) mod services;
pub(crate) mod subsystems;

pub(crate) use self::compaction::MemoryCompactionState;
pub(crate) use self::extraction::MemoryExtractionState;
pub(crate) use self::persistence::MemoryPersistenceState;
pub(crate) use self::runtime::AgentRuntime;
pub(crate) use self::services::Services;
pub(crate) use self::subsystems::MemorySubsystemState;

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::RwLock;
use std::time::{Duration, Instant};

use tokio::sync::{Notify, mpsc, watch};
use tokio::time::Interval;
use tokio_util::sync::CancellationToken;
use zeph_llm::any::AnyProvider;
use zeph_llm::provider::{Message, Role};
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
use zeroize::Zeroizing;

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

#[allow(clippy::struct_excessive_bools)]
pub(crate) struct SkillState {
    pub(crate) registry: Arc<RwLock<SkillRegistry>>,
    /// Per-turn trust snapshot written by `prepare_context` after `build_skill_trust_map`.
    /// Shared with `SkillInvokeExecutor` so it can resolve trust without hitting `SQLite`
    /// on every tool call. Refreshed once per turn — stale by at most one turn.
    /// Carries full `SkillTrustSnapshot` (level + `requires_trust_check` + `blake3_hash`) so
    /// `SkillInvokeExecutor` can perform per-invocation re-hash when the flag is set.
    pub(crate) trust_snapshot:
        Arc<RwLock<HashMap<String, crate::skill_invoker::SkillTrustSnapshot>>>,
    pub(crate) skill_paths: Vec<PathBuf>,
    pub(crate) managed_dir: Option<PathBuf>,
    pub(crate) trust_config: crate::config::TrustConfig,
    pub(crate) matcher: Option<SkillMatcherBackend>,
    pub(crate) max_active_skills: usize,
    /// Token budget for skill bodies injected into a sub-agent's one-shot system prompt at
    /// spawn time. Mirrors `SkillsConfig::subagent_skill_token_budget` (#6421).
    pub(crate) subagent_skill_token_budget: usize,
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
    /// Linear blend weight for BM25 hybrid fusion: `fused = bm25_alpha * cosine + (1-bm25_alpha) * bm25_norm`.
    /// Clamped to `[0.0, 1.0]` at config load. Default: `0.7`.
    pub(crate) bm25_alpha: f32,
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
    /// Provider name for query rewriting before skill matching. Empty = disabled.
    pub(crate) query_rewrite_provider_name: String,
    /// Provider name for `/skill create` generation. Empty = primary.
    pub(crate) generation_provider_name: String,
    /// Provider name for skill disambiguation LLM calls. Empty = primary.
    pub(crate) disambiguate_provider_name: String,
    /// Timeout in milliseconds for `/skill create` LLM generation. Default: 60 000.
    pub(crate) generation_timeout_ms: u64,
    /// Optional quality-gate evaluator for generated SKILL.md files (#3319).
    ///
    /// When `Some`, the evaluator is attached to every `SkillGenerator` instance so that
    /// generated skills are scored before being written to disk.
    pub(crate) skill_evaluator: Option<std::sync::Arc<zeph_skills::evaluator::SkillEvaluator>>,
    /// Weights for the evaluator composite score — forwarded to `SkillGenerator::with_evaluator`.
    pub(crate) eval_weights: zeph_skills::evaluator::EvaluationWeights,
    /// Minimum composite score required to accept a generated skill (forwarded to the generator).
    pub(crate) eval_threshold: f32,
    /// Enable `GoSkills` group-structured skill injection.
    pub(crate) group_structured: bool,
    /// Inter-skill cosine similarity threshold for `GoSkills` grouping.
    pub(crate) support_similarity_threshold: f32,
    /// Whether Stage-2 LLM semantic compliance scan is enabled on `plugin add`.
    pub(crate) semantic_scan: bool,
    /// Provider name for the semantic scan LLM. Empty = use primary provider.
    pub(crate) semantic_scan_provider: String,
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
    /// Effective policy-LLM call timeout in milliseconds — either the explicitly
    /// configured `timeout_ms`, or the value auto-scaled for `provider`'s kind
    /// (local vs cloud). Surfaced so operators can tell at a glance whether a slow
    /// local `policy_provider` got a realistic budget (see #5870).
    pub timeout_ms: u64,
}

#[allow(clippy::struct_excessive_bools)] // independent boolean flags; bitflags or enum would obscure semantics without reducing complexity
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
    /// Inject a `<current_time>` reminder into the volatile system prompt every
    /// `time_reminder_interval_requests` agent turns (#6361). Opt-in, default `false`.
    pub(crate) time_reminder_enabled: bool,
    /// Turn interval between `<current_time>` reminder injections (#6361).
    pub(crate) time_reminder_interval_requests: u32,
    /// Injectable wall-clock source for the `get_current_time` tool and time-reminder
    /// injection (#6361). Defaults to [`zeph_common::SystemClock`]; tests substitute
    /// `zeph_common::FixedClock` for deterministic assertions.
    pub(crate) clock: Arc<dyn zeph_common::ClockSource>,
    /// Per-channel skill allowlist. Skills not matching the allowlist are excluded from the
    /// prompt. An empty `allowed` list means all skills are permitted (default).
    pub(crate) channel_skills: zeph_config::ChannelSkillsConfig,
    /// Per-channel tool allowlist. `None` = no restriction. `Some` = only listed tools permitted.
    /// Populated from the active channel's `allowed_tools` config at agent build time.
    pub(crate) channel_tool_allowlist: Option<Vec<String>>,
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
    /// Resume-visibility banner and `/history` bound config (spec-068 §13, §18, #6420).
    pub(crate) resume_config: zeph_config::ResumeConfig,
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
    /// Callback that spawns an external ACP sub-agent process by shell command (#3302).
    ///
    /// Injected by the binary crate when the `acp` feature is enabled.
    /// `None` in bare / non-ACP mode; callers must degrade gracefully.
    pub(crate) acp_subagent_spawn_fn: Option<zeph_subagent::AcpSubagentSpawnFn>,
    /// Channel type string used as part of the `(channel_type, channel_id)` persistence key.
    ///
    /// Set at build time from the active I/O channel (e.g. `"cli"`, `"tui"`, `"telegram"`).
    /// Empty when channel identity has not been configured (persistence is skipped).
    pub(crate) channel_type: String,
    /// Whether provider preference persistence is enabled for this session (#3308).
    ///
    /// Controlled by `[session] provider_persistence = true` (the default). When `false`,
    /// the stored provider preference is never read or written.
    pub(crate) provider_persistence_enabled: bool,
    /// Whether per-session provider override params (e.g. `reasoning_effort`) should be
    /// persisted alongside the provider name (#4654).
    ///
    /// Only meaningful when `provider_persistence_enabled` is also `true`.
    pub(crate) persist_provider_overrides_enabled: bool,
    /// Guards against re-persisting during `restore_channel_provider` (#4654, F1).
    ///
    /// Set to `true` immediately before calling `provider_switch_as_string` inside the restore
    /// path, cleared on every branch after the call. While `true`, `persist_channel_provider`
    /// returns early without writing anything.
    pub(crate) restoring_provider: bool,
    /// Goal lifecycle feature configuration.
    pub(crate) goals: GoalRuntimeConfig,
    /// Set from the CLI `--bare` flag (#5551).
    ///
    /// Bare mode skips skill loading, memory init, MCP connections, scheduler startup, and
    /// filesystem watchers at startup; this flag lets shutdown-path subsystems (autoDream
    /// consolidation, skill trace-extraction, shutdown summary, session digest) apply the
    /// same gating instead of firing unconditional LLM calls at session end.
    pub(crate) bare: bool,
    /// Set from `config.cli.safe_mode` (`--safe-mode` / `ZEPH_SAFE_MODE`, #6031).
    ///
    /// Distinct from `bare`: safe mode disables ZEPH.md/CLAUDE.md/AGENTS.md discovery,
    /// plugins, skills, hooks, and MCP servers for troubleshooting isolation, rather than
    /// `bare`'s memory/tool-registry/background-task test-mode behavior. Read by
    /// `check_cwd_changed` (#6032) to gate whether a `/cd`-triggered directory change
    /// re-runs instruction discovery — a safe-mode session must never silently re-load
    /// project instructions mid-session, which would defeat the flag.
    pub(crate) safe_mode: bool,
    /// Global caps for MCP image passthrough (spec-072). Read by `emit_media_parts` to
    /// enforce `max_images_per_turn` when attaching MCP-sourced images.
    pub(crate) mcp_media: zeph_config::McpMediaConfig,
    /// `true` when at least one configured MCP server has `media_passthrough = true`
    /// (spec-072 FR-011). Read by `assemble_final_system_prompt` to add the static
    /// untrusted-image caveat line once per session.
    pub(crate) media_passthrough_note_enabled: bool,
}

/// Groups feedback detection subsystems: correction detector, judge detector, and LLM classifier.
pub(crate) struct FeedbackState {
    pub(crate) detector: zeph_agent_feedback::FeedbackDetector,
    pub(crate) judge: Option<zeph_agent_feedback::JudgeDetector>,
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
    /// SONAR NLI entailment-based injection detection stage (opt-in, observe-only).
    pub(crate) nli_sanitizer: Option<zeph_sanitizer::nli::NliSanitizer>,
    /// PAAC secret placeholder masking registry (opt-in). Shared with the bootstrap layer so
    /// vault-resolved secrets registered during config load are masked at the LLM boundary.
    pub(crate) secret_registry: Option<Arc<zeph_sanitizer::secret_mask::SecretMaskRegistry>>,
    /// Post-LLM response verification layer.
    pub(crate) response_verifier: zeph_sanitizer::response_verifier::ResponseVerifier,
    /// Temporal causal IPI analyzer (opt-in, disabled when `None`).
    pub(crate) causal_analyzer: Option<zeph_sanitizer::causal_ipi::TurnCausalAnalyzer>,
    /// VIGIL pre-sanitizer gate. `None` for subagent sessions (subagents are exempt).
    /// Set at agent build time for top-level agents; skipped for subagents (high FP rate).
    pub(crate) vigil: Option<crate::agent::vigil::VigilGate>,
    /// Cross-turn risk accumulator (spec 050 Phase 1).
    ///
    /// `advance_turn()` MUST be called once per turn, before `PolicyGateExecutor::check_policy`.
    /// Never expose score, level, or alerts to any LLM-callable surface.
    pub(crate) trajectory: crate::agent::trajectory::TrajectorySentinel,
    /// Shared risk-level slot for `PolicyGateExecutor` (spec 050).
    ///
    /// Written by the agent loop after each turn's `sentinel.current_risk()` call.
    /// `PolicyGateExecutor::check_policy` reads it to downgrade `Allow` at `Critical`.
    /// `u8` encoding: 0=Calm, 1=Elevated, 2=High, 3=Critical.
    pub(crate) trajectory_risk_slot: zeph_tools::TrajectoryRiskSlot,
    /// Pending risk signals from executor layers (spec 050 §2).
    ///
    /// `PolicyGateExecutor` and `ScopedToolExecutor` push signal codes here.
    /// `begin_turn()` drains this queue into `trajectory.record()`.
    pub(crate) trajectory_signal_queue: zeph_tools::RiskSignalQueue,
    /// Persistent safety stream + LLM pre-execution probe (spec 050 Phase 2).
    ///
    /// `None` when `security.shadow_sentinel.enabled = false` (default).
    /// When `Some`, `begin_turn()` calls `advance_turn()` to reset the per-turn probe counter.
    pub(crate) shadow_sentinel:
        Option<std::sync::Arc<crate::agent::shadow_sentinel::ShadowSentinel>>,
    /// Per-turn multi-step attack chain accumulator.
    ///
    /// `None` by default. When `Some`, `begin_turn()` calls `reset()` to clear per-turn state.
    /// The same `Arc` must be passed to `ShellExecutor::with_risk_chain` at build time.
    pub(crate) risk_chain_accumulator: Option<std::sync::Arc<zeph_tools::RiskChainAccumulator>>,
    /// MAGE trajectory risk accumulator (spec 004-16).
    ///
    /// Per-session in-memory accumulator that ingests sanitizer audit signals with exponential
    /// temporal decay and gates tool execution when cumulative risk exceeds `risk_threshold`.
    /// Initialized as noop when `memory.shadow_memory.enabled = false` (default).
    /// `begin_turn()` calls `advance_turn()` then ingests pending signal codes.
    pub(crate) mage_accumulator: zeph_memory::shadow::TrajectoryRiskAccumulator,
    /// Per-session append-only shadow memory for cross-turn goal-drift detection (spec 010-7).
    ///
    /// `None` when `security.causal_ipi.shadow_memory.enabled = false` (default).
    /// When `Some`, `process_tool_result_batch` records a `ShadowEvent` after each tool batch,
    /// then calls `goal_drift_score()` and emits a `GoalDrift` security event when alerted.
    pub(crate) shadow_memory: Option<zeph_sanitizer::ShadowMemory>,
    /// Handle into `TrustGateExecutor`'s MCP tool-id registry
    /// (`crates/zeph-tools/src/trust_gate.rs`), used to force-deny all MCP-sourced tools when
    /// the active skill trust is Quarantined.
    ///
    /// `None` when the caller didn't attach a handle (e.g. tests, or an executor tree built
    /// without `apply_common_tool_gating`). When `Some`, `check_tool_refresh` keeps it in sync
    /// with `self.services.mcp.tools` so MCP servers connected after startup (`/mcp add`,
    /// `tools/list_changed`) are folded into the Quarantine-deny set — the handle is otherwise
    /// only ever populated once, at startup, and goes stale (#5747).
    pub(crate) mcp_tool_ids: Option<Arc<RwLock<HashSet<String>>>>,
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
    /// User-defined resource attributes forwarded to `TracingCollector` (from `telemetry.trace_metadata`).
    pub(crate) trace_metadata: std::collections::HashMap<String, String>,
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
    pub(crate) cancel_bridge_handle: Option<zeph_common::task_supervisor::BlockingHandle<()>>,
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
    /// Ticks periodically so `Agent::next_event` refreshes `bg_enrichment_inflight` /
    /// `bg_telemetry_inflight` (and reaps completed tasks) during idle time between turns, not
    /// only at the top of the next turn. Background enrichment/telemetry tasks run *after* a
    /// turn's response is sent (spawned from `persist_message`), so without this the TUI status
    /// segment showing in-flight background work was invisible for the entire idle window (#6279).
    ///
    /// `None` until the first `Agent::next_event` call lazily constructs it: `tokio::time::interval`
    /// requires an active Tokio runtime, but `LifecycleState::new()` is also called from plain
    /// (non-`#[tokio::test]`) unit tests that construct an `Agent` outside any runtime.
    pub(crate) bg_metrics_tick: Option<Interval>,
    /// Per-turn completion notifier. `None` when `notifications.enabled = false`.
    pub(crate) notifier: Option<crate::notifications::Notifier>,
    /// Per-turn LLM request counter. Incremented by `process_response`; reset at turn start.
    pub(crate) turn_llm_requests: u32,
    /// Per-turn tool-call dispatch counter. Incremented by `check_and_update_quota` for every
    /// tool call in a dispatch batch; reset at turn start. Feeds `TurnSummary::tool_calls`.
    pub(crate) turn_tool_calls: u32,
    /// Timestamp of the last turn that ended with `LlmError::NoProviders`.
    ///
    /// Used to gate `advance_context_lifecycle`: when all providers are down, context preparation
    /// is skipped (degraded mode) until `no_providers_backoff_secs` has elapsed.
    pub(crate) last_no_providers_at: Option<Instant>,
    /// Completions from background shell runs waiting to be injected into the next turn.
    ///
    /// Drained at the top of `process_user_message_inner` after `supervisor.reap()`.
    /// All pending completions and the real user message are merged into a **single**
    /// user-role block to satisfy strict alternation requirements (Anthropic Messages API).
    ///
    /// Capacity is capped at `BACKGROUND_COMPLETION_BUFFER_CAP`. On overflow the oldest
    /// entry is dropped and a placeholder is substituted so the LLM learns results were lost.
    pub(crate) pending_background_completions:
        VecDeque<zeph_tools::shell::background::BackgroundCompletion>,
    /// Receiver end of the dedicated background-completion channel created alongside the
    /// `ShellExecutor`. Polled at the top of each turn to drain completions into
    /// `pending_background_completions`. `None` when no `ShellExecutor` is configured.
    pub(crate) background_completion_rx:
        Option<tokio::sync::mpsc::Receiver<zeph_tools::BackgroundCompletion>>,
    /// Shared reference to the `ShellExecutor` used to query in-flight background run snapshots
    /// for TUI metrics display. `None` when no `ShellExecutor` is wired (test harnesses, etc.).
    pub(crate) shell_executor_handle: Option<std::sync::Arc<zeph_tools::ShellExecutor>>,
    /// Session-level task supervisor, shared with bootstrap and TUI. Used to register
    /// background agent tasks (cancel bridge, compaction, sidequest eviction) for
    /// observability and graceful shutdown.
    ///
    /// Created with a fresh [`CancellationToken`] in `LifecycleState::new()` for test
    /// harnesses; production code overwrites it via `Agent::with_task_supervisor`.
    pub(crate) task_supervisor: Arc<zeph_common::TaskSupervisor>,
}

/// Minimal config snapshot needed to reconstruct a provider at runtime via `/provider <name>`.
///
/// Secrets are stored as plain strings because [`Secret`] intentionally does not implement
/// `Clone`. They are re-wrapped in `Secret` when passed to `build_provider_for_switch`.
///
/// `Clone` so ACP/serve deps structs (built once per process) can hand each session its own
/// owned copy via [`Agent::with_provider_pool`](crate::agent::Agent::with_provider_pool).
#[derive(Clone, Default)]
pub struct ProviderConfigSnapshot {
    pub claude_api_key: Option<String>,
    pub openai_api_key: Option<String>,
    pub gemini_api_key: Option<String>,
    pub compatible_api_keys: std::collections::HashMap<String, String>,
    pub llm_request_timeout_secs: u64,
    pub embedding_model: String,
    pub gonka_private_key: Option<Zeroizing<String>>,
    pub gonka_address: Option<String>,
    pub cocoon_access_hash: Option<String>,
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
    /// Shared classifier latency ring buffer, allocated unconditionally at agent construction
    /// (before any builder call) so it is available regardless of builder-chain ordering.
    /// Recorded into by `ContentSanitizer` (injection, PII) and `LlmClassifier` (feedback);
    /// stays empty (all-`None` percentiles) when no classifier is configured.
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
    /// Lookahead tool hints snapshot taken after the most recent scheduler tick.
    ///
    /// Populated by `run_scheduler_loop` after each `scheduler.tick()` call via
    /// `zeph_orchestration::lookahead_tools`. Cleared when the scheduler loop exits.
    /// Read by `prepare_context` in `assembly.rs` to pass PAACE hints to `FidelityScorer`.
    pub(crate) cached_lookahead: Vec<zeph_common::PlannedToolHint>,
    /// On `OrchestrationState` (not `ProviderState`) because this provider is used exclusively
    /// by `LlmPlanner` during orchestration, not shared across subsystems.
    pub(crate) planner_provider: Option<AnyProvider>,
    /// Provider for `PlanVerifier` LLM calls. `None` falls back to `orchestrator_provider`
    /// then the primary provider.
    pub(crate) verify_provider: Option<AnyProvider>,
    /// Provider for scheduling-tier LLM calls (aggregation, predicate evaluation, verification
    /// fallback). `None` falls back to the primary provider.
    /// Set from `config.orchestration.orchestrator_provider` at startup.
    pub(crate) orchestrator_provider: Option<AnyProvider>,
    /// Provider for predicate gate evaluation. `None` falls back to `orchestrator_provider`
    /// then `verify_provider` then primary.
    pub(crate) predicate_provider: Option<AnyProvider>,
    /// Resolved ensemble members for ORCH-style deterministic verifier ensemble-merge
    /// (spec `073-orch-ensemble-merge`). Each entry pairs the `[[llm.providers]]` name with
    /// its resolved provider — kept as pairs (not a bare `Vec<AnyProvider>`) so a partial
    /// bootstrap-time resolution failure can never desynchronize a ballot's `member` name
    /// from the wrong config entry.
    ///
    /// Empty when `[orchestration.ensemble].enabled = false` (the default) or when no member
    /// resolved successfully. `SchedulerAction::Verify` only takes the ensemble branch when
    /// this is non-empty.
    pub(crate) ensemble_members: Vec<(String, AnyProvider)>,
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
    /// Named execution environment for the current orchestration task.
    ///
    /// Set by the scheduler when dispatching a `TaskNode` that has
    /// `execution_environment: Some(name)`. Cleared between tasks. When `Some`,
    /// `prepare_tool_dispatch` injects an [`ExecutionContext`] named `name` into
    /// every `ToolCall` so that `ShellExecutor::resolve_context` uses the right env.
    pub(crate) task_execution_env: Option<String>,

    // ── P2 durable adapter (spec-064) ─────────────────────────────────────────
    /// Durable config snapshot used by the P2 adapter in `plan.rs`.
    ///
    /// `None` when durable execution is disabled or the agent was built without a durable config
    /// (e.g. unit tests). When `Some` and `durable.orchestration = true`, `/plan resume` restores
    /// the replan budget from the journal instead of zeroing it.
    pub(crate) durable_config: Option<zeph_config::DurableConfig>,
    /// Resolved path to `durable.db` (the dedicated journal file for `LocalBackend`).
    ///
    /// Derived at build time from `memory.sqlite_path` sibling directory. `None` when the durable
    /// adapter is not configured.
    pub(crate) durable_db_url: Option<String>,
    /// Shared durable backend for P2 budget snapshots.
    ///
    /// Lazily initialised by `plan.rs` on first journal call; shared across pause/resume cycles
    /// for the same process lifetime.
    // Accessed exclusively through ensure_durable_backend() in plan.rs; rustc's cross-module
    // dead_code analysis does not follow the indirect Option method chains.
    #[allow(dead_code)]
    pub(crate) durable_backend: Option<std::sync::Arc<zeph_durable::DurableBackendEnum>>,
    /// Writer handle for the shared P2 durable backend.
    // Same as durable_backend: accessed through plan.rs ensure_durable_backend().
    #[allow(dead_code)]
    pub(crate) durable_writer: Option<zeph_durable::JournalWriterHandle>,
    /// [`BlockingHandle`] for the background `JournalWriter` actor task, tracked by `TaskSupervisor`.
    ///
    /// Kept so the agent can abort the writer on shutdown rather than relying on process exit.
    /// `None` until `ensure_durable_backend()` initialises the backend for the first time.
    pub(crate) durable_writer_task: Option<zeph_common::task_supervisor::BlockingHandle<()>>,
    /// Cipher for encrypting P2 budget snapshots. `None` when `encrypt_payload = false`.
    pub(crate) durable_cipher: Option<std::sync::Arc<dyn zeph_durable::PayloadCipher>>,
    /// Control-entry row HMAC key (INV-8) for the P2 durable backend. `None` for a single-user
    /// local, non-shared database — the documented stance where control entries carry no HMAC.
    pub(crate) durable_hmac_key: Option<[u8; 32]>,
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
    /// Handle for the background experiment task. Stored so shutdown can abort it if the
    /// `CancellationToken` signal is not observed in time (e.g. the task is blocked on I/O).
    pub(crate) handle: Option<zeph_common::task_supervisor::BlockingHandle<()>>,
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

/// Groups context-compression feature state (gated behind `context-compression` feature flag).
#[derive(Default)]
pub(crate) struct CompressionState {
    /// Cached task goal for TaskAware/MIG pruning. Set by `maybe_compact()`,
    /// invalidated when the last user message hash changes.
    pub(crate) current_task_goal: Option<String>,
    /// Hash of the last user message when `current_task_goal` was populated.
    pub(crate) task_goal_user_msg_hash: Option<u64>,
    /// Pending background task for goal extraction. Spawned when the user message hash changes;
    /// result applied at the start of the next Soft compaction (#1909).
    pub(crate) pending_task_goal:
        Option<zeph_common::task_supervisor::BlockingHandle<Option<String>>>,
    /// Pending `SideQuest` eviction result from the background LLM call spawned last turn.
    /// Applied at the START of the next turn before compaction (PERF-1 fix).
    pub(crate) pending_sidequest_result:
        Option<zeph_common::task_supervisor::BlockingHandle<Option<Vec<usize>>>>,
    /// In-memory subgoal registry for `Subgoal`/`SubgoalMig` pruning strategies (#2022).
    pub(crate) subgoal_registry: zeph_agent_context::SubgoalRegistry,
    /// Pending background subgoal extraction task.
    pub(crate) pending_subgoal: Option<
        zeph_common::task_supervisor::BlockingHandle<
            Option<zeph_agent_context::SubgoalExtractionResult>,
        >,
    >,
    /// Hash of the last user message when subgoal extraction was scheduled.
    pub(crate) subgoal_user_msg_hash: Option<u64>,
    /// Shared typed-page state (#3630). `None` when `typed_pages.enabled = false`.
    pub(crate) typed_pages_state: Option<Arc<zeph_context::typed_page::TypedPagesState>>,
}

/// Groups runtime tool filtering, dependency tracking, and iteration bookkeeping.
pub(crate) struct ToolState {
    /// `config.tools.enabled`, mirrored here so `process_response_native_tools` can gate
    /// tool-definition construction without threading a live `Config` through `Agent<C>`.
    /// When `false`, no tool definitions are built or sent to the LLM (#6386).
    pub(crate) tools_enabled: bool,
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
    /// PASTE pattern store for tool invocation history and prediction (#3642).
    ///
    /// `Some` only when `config.tools.speculative.mode` is `Pattern` or `Both`.
    pub(crate) pattern_store: Option<Arc<crate::agent::speculative::paste::PatternStore>>,
    /// Per-turn mapping from tool name to `(skill_name, skill_hash)`, populated at skill
    /// activation and used by `observe()` to attribute tool completions to their owning skill.
    pub(crate) tool_to_skill: HashMap<String, (String, String)>,
    /// Last tool executed per skill in the current turn, keyed by skill name.
    /// Used as `prev_tool` for PASTE pattern transition recording.
    pub(crate) last_tool_per_skill: HashMap<String, String>,
    /// `config.tools.shell.allowed_paths`, mirrored here so `AgentAccess::change_working_directory`
    /// (#6032 SEC-2) can validate a `/cd` target against the same sandbox boundary
    /// `FileExecutor`/`DiagnosticsExecutor`/`SetCwdExecutor` already enforce, via
    /// `zeph_common::security::validate_path_within`. Empty means "no session has set it yet";
    /// callers treat empty the same way `FileExecutor::new` does (default to `[cwd]`), not as
    /// "allow every path".
    pub(crate) allowed_paths: Vec<std::path::PathBuf>,
}

impl Default for ToolState {
    fn default() -> Self {
        Self {
            tools_enabled: true,
            tool_schema_filter: None,
            cached_filtered_tool_ids: None,
            dependency_graph: None,
            dependency_always_on: HashSet::new(),
            completed_tool_ids: HashSet::new(),
            current_tool_iteration: 0,
            pattern_store: None,
            tool_to_skill: HashMap::new(),
            last_tool_per_skill: HashMap::new(),
            allowed_paths: Vec::new(),
        }
    }
}

/// Groups per-session I/O and policy state.
#[allow(clippy::struct_excessive_bools)] // runtime state — boolean flags are idiomatic here
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
    /// Whether the current turn originates from a Telegram guest query (`guest_message` update).
    ///
    /// When `true`, the agent prompt includes a brief guest-context annotation, and the response
    /// is delivered via `answerGuestQuery` instead of `sendMessage`.
    pub(crate) is_guest_context: bool,
    /// Cross-thread store owner key for the current turn (spec-080 §10 OQ-1, GitHub #6389).
    ///
    /// Set from `ChannelMessage::owner_key` when a `LoopEvent::Message` is dispatched (falls back
    /// to [`persistence::DEFAULT_OWNER_KEY`] when the originating channel leaves it unset —
    /// CLI/TUI/Telegram). Reset to the default unconditionally at the top of every iteration of
    /// [`Agent::run`]'s main loop (#6418), and again at [`Agent::end_turn`] as defense-in-depth
    /// for the normal end-of-turn path — same as `is_guest_context` above. The loop-top reset is
    /// what guarantees no stale value survives into a `LoopEvent` triggered by a different
    /// sender/channel or a non-`Message` event (scheduled task, autonomous tick, ...), even when a
    /// fast-path slash-command dispatch `continue`s/`break`s before ever reaching `end_turn`. Read
    /// by `/store` (`agent_access_impl.rs`) and the Command-handoff store writes/reads
    /// (`scheduler_loop.rs`).
    ///
    /// [`Agent::run`]: crate::agent::Agent::run
    /// [`Agent::end_turn`]: crate::agent::Agent::end_turn
    pub(crate) owner_key: String,
    /// Active durable execution context for the P1 agent-loop adapter (spec-064 §P1, #5452).
    ///
    /// `Some` when `[durable] enabled = true` and `agent_turns = true`. The context is opened
    /// lazily by [`Agent::ensure_session_durable_ctx`](crate::agent::Agent::ensure_session_durable_ctx)
    /// the first time a durable-gated call site runs (not eagerly in the builder chain, since the
    /// real `TaskSupervisor` is only attached later via `with_task_supervisor`), keyed on the
    /// session's `ConversationId` so every turn replays under the same execution. `None` when
    /// durable execution is disabled (or construction failed and degraded) — in which case the
    /// loop runs unmodified.
    pub(crate) durable_ctx: Option<std::sync::Arc<zeph_durable::DurableContext>>,
    /// Mirror of `[durable] subagent` config flag (spec-064 §P4, #5452), set unconditionally at
    /// bootstrap via `AgentBuilder::with_durable_subagent` from `config.durable.subagent`.
    ///
    /// When `true` and `durable_ctx` is `Some`, sub-agent spawns are wrapped in a durable
    /// promise so a resumed parent can replay the child result without re-running the child.
    pub(crate) durable_subagent: bool,
    /// Set to `true` for the duration of a turn whose LLM step was replayed from the journal.
    ///
    /// Used by `process_single_native_turn` to suppress re-printing already-emitted assistant
    /// output (spec-064 §INV-001 §15 `RuntimeLayer` double-print suppression). Cleared at the
    /// start of each turn.
    pub(crate) durable_turn_replayed: bool,
    /// `DurableConfig`/db url/cipher stashed cheaply (no I/O) by
    /// `AgentBuilder::with_durable_agent_turns` when `[durable] enabled = true` and
    /// `agent_turns = true`. Consumed by `ensure_session_durable_ctx` to lazily open the backend
    /// and construct `durable_ctx` on the first durable-gated call. `None` when the P1 adapter is
    /// not configured, in which case `durable_ctx` stays `None` forever (#5452 FR-002).
    pub(crate) durable_agent_turns_config: Option<zeph_config::DurableConfig>,
    /// Sibling companion to [`Self::durable_agent_turns_config`]: the `durable.db` connection
    /// string resolved at bootstrap.
    pub(crate) durable_agent_turns_db_url: Option<String>,
    /// Sibling companion to [`Self::durable_agent_turns_config`]: `config.memory.sqlite_path`,
    /// folded into the P1 `ExecutionId` derivation alongside `ConversationId` so distinct memory
    /// databases never collide on execution identity even if they ever shared a journal
    /// `db_url` (#5553).
    pub(crate) durable_agent_turns_sqlite_path: Option<String>,
    /// Sibling companion to [`Self::durable_agent_turns_config`]: the AEAD cipher to attach to
    /// the backend, `None` when `encrypt_payload = false` (development mode only).
    pub(crate) durable_agent_turns_cipher: Option<std::sync::Arc<dyn zeph_durable::PayloadCipher>>,
    /// Sibling companion to [`Self::durable_agent_turns_config`]: the control-entry row HMAC key
    /// (INV-8) to attach to the backend. `None` for a single-user local, non-shared database —
    /// the documented stance where control entries carry no HMAC.
    pub(crate) durable_agent_turns_hmac_key: Option<[u8; 32]>,
    /// Set to `true` the first time `ensure_session_durable_ctx` runs (success or failure) so a
    /// failed backend construction (missing vault key, disk error) is not retried on every turn.
    /// Reset to `false` by `reset_durable_ctx_for_conversation_switch` (`/new`, `/conv resume`,
    /// `/conv fork` — #5452 critic finding S1) so a conversation switch re-derives a fresh
    /// execution keyed on the new `ConversationId` instead of leaving this latched forever.
    pub(crate) durable_ctx_init_attempted: bool,
    /// Writer handle for the P1 adapter's durable backend, flushed on shutdown by
    /// `flush_durable_writer` (mirrors `services.orchestration.durable_writer` for the P2 adapter).
    pub(crate) durable_writer: Option<zeph_durable::JournalWriterHandle>,
    /// [`BlockingHandle`] for the P1 adapter's background `JournalWriter` actor task, aborted on
    /// shutdown by `flush_durable_writer` (mirrors `services.orchestration.durable_writer_task`).
    pub(crate) durable_writer_task: Option<zeph_common::task_supervisor::BlockingHandle<()>>,
    /// Process-exclusivity lock on `durable_ctx`'s `ExecutionId` (INV-15, #6122), held for as long
    /// as this session drives the execution. Dropping it (on shutdown or
    /// `reset_durable_ctx_for_conversation_switch`) releases the lock so another process — or a
    /// later conversation switch in this same process — can open the same `ExecutionId`. `None`
    /// when `durable_ctx` is `None`, or when the backend could not derive a lock (`:memory:`,
    /// Postgres — see [`zeph_durable::LocalBackend::open_execution_exclusive`]).
    pub(crate) durable_execution_lock: Option<zeph_durable::ExecutionLock>,
    /// When `true`, the system prompt volatile block includes the `CAVEMAN_DIRECTIVE` on every
    /// turn, instructing the LLM to use ultra-compressed telegraphic output.
    ///
    /// Initialized from `config.caveman.default_on` in `builder.rs`. Toggled at runtime by
    /// `/caveman [on|off]`. Preserved across `/new` (session resets do not clear style flags —
    /// only process restart returns to `default_on`).
    pub(crate) caveman_active: bool,
    /// Durable JSONL event-log dual-writer for this conversation-session (spec-068, #5343).
    ///
    /// `Some` when `[session] enabled = true`; the session has minted a
    /// [`zeph_common::SessionId`](zeph_common::SessionId) and opened its event log. `None` when
    /// session persistence is disabled — in which case only the `SQLite` `messages` projection
    /// is written (pre-#5343 behavior).
    pub(crate) session_sink: Option<std::sync::Arc<zeph_agent_persistence::SessionSink>>,
    /// `[session]` config snapshot (spec-068, #5343, D-9) — retained (not just consumed at
    /// construction) so a mid-session `/conv resume`/`/conv fork` swap can locate `data_dir` to
    /// replay a different session's event log and re-point [`Self::session_sink`] to it.
    /// `None` when session persistence is disabled.
    pub(crate) session_persistence_config: Option<zeph_config::SessionConfig>,
}

/// Extracted hook lists from `[hooks]` config, stored in `SessionState`.
#[derive(Default)]
pub(crate) struct HooksConfigSnapshot {
    /// Hooks fired when working directory changes.
    pub(crate) cwd_changed: Vec<zeph_config::HookDef>,
    /// Hooks fired when a watched file changes.
    pub(crate) file_changed_hooks: Vec<zeph_config::HookDef>,
    /// Hooks fired when a tool execution is blocked by a `RuntimeLayer::before_tool` check.
    pub(crate) permission_denied: Vec<zeph_config::HookDef>,
    /// Hooks fired after each agent turn completes (#3327).
    ///
    /// Populated from `HooksConfig::turn_complete` at session construction. Shares the
    /// `Notifier::should_fire` gate when a notifier is configured; fires on every completion
    /// when no notifier is present.
    pub(crate) turn_complete: Vec<zeph_config::HookDef>,
    /// Hooks fired before each tool execution, matched by tool name pattern.
    pub(crate) pre_tool_use: Vec<zeph_config::HookMatcher>,
    /// Hooks fired after each tool execution completes, matched by tool name pattern.
    pub(crate) post_tool_use: Vec<zeph_config::HookMatcher>,
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
    /// Set by `AgentBuilder::with_preloaded_messages` (spec-068, #5343) when `messages` was
    /// seeded from a durable event-log replay rather than the default single system-prompt
    /// message `Agent::new` always seeds. Makes [`super::super::Agent::load_history`]'s
    /// `SQLite`-skip guard precise — `messages.is_empty()` is never true at that point in the
    /// normal flow (the system prompt is always present), so a plain emptiness check cannot
    /// distinguish "already hydrated from the log" from "not yet loaded."
    pub(crate) history_preloaded: bool,
    /// `/history all`/`/history next` pagination cursor (spec-068 §13.6). `0` = not yet
    /// started or reset by a subsequent bounded `/history [N]` call.
    pub(crate) history_cursor: usize,
    /// Count of `messages` entries with `role != Role::System` (#6427).
    ///
    /// Backs `MessageAccessImpl::transcript_len`/`transcript_page`
    /// (`command_context_impls.rs`) so `/history` doesn't rescan the full vector on every
    /// call. Every mutation site of `messages` must keep this in sync — use
    /// [`MessageState::track_single_message`] for a per-message push/insert/remove (O(1)) or
    /// [`MessageState::recompute_non_system_count`] after a batch mutation (O(n), but never on
    /// the `/history` read path this field exists to speed up).
    pub(crate) non_system_count: usize,
}

impl MessageState {
    /// O(1) count of non-system messages — see [`Self::non_system_count`].
    pub(crate) fn non_system_len(&self) -> usize {
        self.non_system_count
    }

    /// Recompute [`Self::non_system_count`] from scratch after a batch mutation (append,
    /// truncate, drain, retain, or a cross-crate mutation through a borrowed `&mut
    /// Vec<Message>` view) where tracking the exact delta inline would be error-prone.
    ///
    /// O(n), but only called on structural mutation paths (history replay, compaction, focus
    /// lifecycle, `/clear`) — never on the `/history` read path this counter exists to speed
    /// up. Call alongside `recompute_prompt_tokens()`, which follows the same pattern for the
    /// cached token count.
    pub(crate) fn recompute_non_system_count(&mut self) {
        self.non_system_count = self
            .messages
            .iter()
            .filter(|m| m.role != Role::System)
            .count();
    }

    /// Adjust [`Self::non_system_count`] for a single message added to or removed from
    /// `messages`. `added = true` for push/insert, `false` for remove/pop.
    pub(crate) fn track_single_message(&mut self, role: Role, added: bool) {
        if role == Role::System {
            return;
        }
        if added {
            self.non_system_count += 1;
        } else {
            self.non_system_count = self.non_system_count.saturating_sub(1);
        }
    }
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
            .map_err(|e| crate::agent::error::AgentError::ContextError(format!("{e:#}")))?;
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
            let trace_metadata = self.trace_metadata.clone();
            match crate::debug_dump::trace::TracingCollector::new(
                dump_dir.as_path(),
                &service_name,
                trace_metadata,
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
            // Fire-and-forget: this is a sync fn, and unlike the session-end call site
            // (`agent/mod.rs`) a subsequent format switch or session activity follows, so
            // there's a real concurrency benefit to not blocking on this write (#6107 critic S1).
            let _ = tc.finish();
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
        let raw = crate::debug_dump::DebugDumper::chat_response_dump_text(result);
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
            trace_metadata: std::collections::HashMap::new(),
            current_iteration_span_id: None,
        }
    }
}

impl Default for FeedbackState {
    fn default() -> Self {
        Self {
            detector: zeph_agent_feedback::FeedbackDetector::new(0.6),
            judge: None,
            llm_classifier: None,
        }
    }
}

/// Goal lifecycle feature configuration stored in `RuntimeConfig`.
#[derive(Debug, Clone)]
pub(crate) struct GoalRuntimeConfig {
    /// Whether goal tracking is enabled.
    pub(crate) enabled: bool,
    /// Maximum allowed length (in Unicode chars) of goal text at creation.
    pub(crate) max_text_chars: usize,
    /// Default token budget for new goals (`None` = unlimited).
    pub(crate) default_token_budget: Option<u64>,
    /// Whether to inject the active goal block into the volatile system prompt region.
    pub(crate) inject_into_system_prompt: bool,
    /// Whether autonomous multi-turn execution is permitted.
    pub(crate) autonomous_enabled: bool,
    /// Maximum turns per autonomous session.
    pub(crate) autonomous_max_turns: u32,
    /// Provider name for the supervisor LLM call (`None` = use main provider).
    pub(crate) supervisor_provider: Option<zeph_config::ProviderName>,
    /// Turns between supervisor verification checks.
    pub(crate) verify_interval: u32,
    /// Timeout for a single supervisor call in seconds.
    pub(crate) supervisor_timeout_secs: u64,
    /// Consecutive stuck-detection threshold before aborting.
    pub(crate) max_stuck_count: u32,
    /// Wall-clock timeout in seconds for a single autonomous LLM turn.
    pub(crate) autonomous_turn_timeout_secs: u64,
    /// Maximum consecutive supervisor verification failures before pausing the session.
    pub(crate) max_supervisor_fail_count: u32,
}

impl Default for GoalRuntimeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_text_chars: 2000,
            default_token_budget: None,
            inject_into_system_prompt: true,
            autonomous_enabled: false,
            autonomous_max_turns: 20,
            supervisor_provider: None,
            verify_interval: 5,
            supervisor_timeout_secs: 30,
            max_stuck_count: 3,
            autonomous_turn_timeout_secs: 300,
            max_supervisor_fail_count: 3,
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
            time_reminder_enabled: false,
            time_reminder_interval_requests: 10,
            clock: Arc::new(zeph_common::SystemClock),
            channel_skills: zeph_config::ChannelSkillsConfig::default(),
            channel_tool_allowlist: None,
            loop_min_interval_secs: 5,
            layers: Vec::new(),
            supervisor_config: crate::config::TaskSupervisorConfig::default(),
            recap_config: zeph_config::RecapConfig::default(),
            resume_config: zeph_config::ResumeConfig::default(),
            acp_config: zeph_config::AcpConfig::default(),
            auto_recap_shown: false,
            msg_count_at_resume: 0,
            acp_subagent_spawn_fn: None,
            channel_type: String::new(),
            provider_persistence_enabled: true,
            persist_provider_overrides_enabled: true,
            restoring_provider: false,
            goals: GoalRuntimeConfig::default(),
            bare: false,
            safe_mode: false,
            mcp_media: zeph_config::McpMediaConfig::default(),
            media_passthrough_note_enabled: false,
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
            is_guest_context: false,
            owner_key: persistence::DEFAULT_OWNER_KEY.to_owned(),
            durable_ctx: None,
            durable_subagent: false,
            durable_turn_replayed: false,
            durable_agent_turns_config: None,
            durable_agent_turns_db_url: None,
            durable_agent_turns_sqlite_path: None,
            durable_agent_turns_cipher: None,
            durable_agent_turns_hmac_key: None,
            durable_ctx_init_attempted: false,
            durable_writer: None,
            durable_writer_task: None,
            durable_execution_lock: None,
            caveman_active: false,
            session_sink: None,
            session_persistence_config: None,
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
            subagent_skill_token_budget: zeph_config::default_subagent_skill_token_budget().get(),
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
            hybrid_search: true,
            bm25_alpha: 0.7,
            bm25_index: None,
            two_stage_matching: false,
            confusability_threshold: 0.0,
            rl_head: None,
            rl_weight: 0.3,
            rl_warmup_updates: 50,
            generation_output_dir: None,
            query_rewrite_provider_name: String::new(),
            generation_provider_name: String::new(),
            disambiguate_provider_name: String::new(),
            generation_timeout_ms: 60_000,
            skill_evaluator: None,
            eval_weights: zeph_skills::evaluator::EvaluationWeights::default(),
            eval_threshold: 0.60,
            group_structured: false,
            support_similarity_threshold: 0.50,
            semantic_scan: false,
            semantic_scan_provider: String::new(),
        }
    }
}

/// Interval between periodic `bg_metrics_tick` refreshes (#6279).
///
/// Short enough that the TUI's background-work status segment feels live during idle time
/// between turns, long enough to be a negligible fraction of `BackgroundSupervisor::reap`'s cost.
pub(crate) const BG_METRICS_TICK_INTERVAL: Duration = Duration::from_secs(2);

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
            bg_metrics_tick: None,
            notifier: None,
            turn_llm_requests: 0,
            turn_tool_calls: 0,
            last_no_providers_at: None,
            pending_background_completions: VecDeque::new(),
            background_completion_rx: None,
            shell_executor_handle: None,
            task_supervisor: Arc::new(zeph_common::TaskSupervisor::new(
                tokio_util::sync::CancellationToken::new(),
            )),
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
            classifier_metrics: Some(Arc::new(zeph_llm::ClassifierMetrics::new(
                zeph_llm::classifier::metrics::DEFAULT_RING_BUFFER_SIZE,
            ))),
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
            handle: None,
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
