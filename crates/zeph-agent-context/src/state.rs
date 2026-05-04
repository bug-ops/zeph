// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Borrow-lens view types used by [`crate::service::ContextService`].
//!
//! Each view holds `&`/`&mut` references to the exact sub-fields that the context
//! service needs. By accepting lenses instead of `&mut Agent<C>`, this crate avoids
//! depending on `zeph-core` while still letting the call site in `zeph-core` construct
//! them from disjoint field projections.
//!
//! Views are constructed at the call site in `zeph-core` using one literal struct
//! expression. The borrow checker proves disjointness at that level without additional
//! helper methods — each `&mut` resolves to a unique field path under `Agent<C>`.

use parking_lot::RwLock;
use std::borrow::Cow;
use std::collections::HashSet;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use zeph_common::SecurityEventCategory;
use zeph_common::task_supervisor::{BlockingHandle, TaskSupervisor};
use zeph_config::{
    ContextStrategy, DocumentConfig, GraphConfig, PersonaConfig, ReasoningConfig, TrajectoryConfig,
    TreeConfig,
};
use zeph_context::input::CorrectionConfig;
use zeph_context::manager::ContextManager;
use zeph_context::summarization::SummarizationDeps;
use zeph_llm::any::AnyProvider;
use zeph_llm::provider::Message;
use zeph_memory::semantic::SemanticMemory;
use zeph_memory::{ConversationId, TokenCounter};
use zeph_sanitizer::ContentSanitizer;
use zeph_sanitizer::quarantine::QuarantinedSummarizer;
use zeph_skills::proactive::ProactiveExplorer;
use zeph_skills::registry::SkillRegistry;

use crate::compaction::{SubgoalExtractionResult, SubgoalRegistry};

/// Borrow-lens over the agent's conversation window fields.
///
/// Holds `&mut` references to every message-list field that the context service
/// needs to read or write. Constructed by the `zeph-core` shim from disjoint
/// sub-fields of `Agent<C>::msg`.
pub struct MessageWindowView<'a> {
    /// Full message history. The context service reads and filters this list.
    pub messages: &'a mut Vec<Message>,
    /// `SQLite` row ID of the most recently persisted message.
    pub last_persisted_message_id: &'a mut Option<i64>,
    /// `SQLite` row IDs to be soft-deleted after context assembly completes.
    pub deferred_db_hide_ids: &'a mut Vec<i64>,
    /// Deferred summary strings to be appended after context assembly completes.
    pub deferred_db_summaries: &'a mut Vec<String>,
    /// Running token count for the current prompt window — updated after every
    /// message-list mutation to keep provider call budgets accurate.
    /// Maps to `Agent<C>::runtime.providers.cached_prompt_tokens`.
    pub cached_prompt_tokens: &'a mut u64,
    /// Shared token counter — cheap `Arc` clone from `Agent<C>::runtime.metrics.token_counter`.
    pub token_counter: Arc<TokenCounter>,
    /// Tool IDs that completed successfully in the current session.
    /// Maps to `Agent<C>::services.tool_state.completed_tool_ids`.
    /// Cleared by `clear_history` together with the message list.
    pub completed_tool_ids: &'a mut HashSet<String>,
}

/// Accumulated metric deltas for one context-assembly pass.
///
/// Holds owned counters that the service increments during `prepare_context`.
/// After the call returns, the `zeph-core` shim applies these deltas to the agent's
/// metrics snapshot via `update_metrics`. Using owned values (not references) avoids
/// borrowing into `MetricsSnapshot`, which lives behind a watch channel.
#[derive(Debug, Default)]
pub struct MetricsCounters {
    /// Sanitizer checks performed during this pass.
    pub sanitizer_runs: u64,
    /// Injection flags raised during this pass.
    pub sanitizer_injection_flags: u64,
    /// Truncations applied during this pass.
    pub sanitizer_truncations: u64,
    /// Quarantine invocations during this pass.
    pub quarantine_invocations: u64,
    /// Quarantine failures during this pass.
    pub quarantine_failures: u64,
}

/// Abstract sink for security events raised during context assembly.
///
/// Implemented in `zeph-core` by a stack-local adapter that appends to
/// `Agent<C>::runtime.metrics.security_events`. Using a trait keeps this crate
/// free of `zeph-core` internal types.
pub trait SecurityEventSink: Send {
    /// Record a security event.
    fn push(&mut self, category: SecurityEventCategory, source: &'static str, detail: String);
}

/// Borrow-lens over all fields needed for `prepare_context` and `Agent<C>::rebuild_system_prompt`.
///
/// Every field maps to a single sub-field of `Agent<C>` and uses a type from a
/// lower-level crate (`zeph-memory`, `zeph-skills`, `zeph-context`, `zeph-sanitizer`,
/// `zeph-config`, `zeph-common`, `zeph-llm`). No `zeph-core`-internal `*State`
/// aggregator ever crosses this boundary.
///
/// Constructed by the `zeph-core` shim using one literal struct expression. The
/// borrow checker verifies disjointness because no two `&mut` paths share a prefix.
pub struct ContextAssemblyView<'a> {
    // ── Memory (one mut field; the rest are read-only clones/copies) ─────────────────
    /// `services.memory.persistence.memory` — `Arc` clone is cheap.
    pub memory: Option<Arc<SemanticMemory>>,
    /// `services.memory.persistence.conversation_id`.
    pub conversation_id: Option<ConversationId>,
    /// `services.memory.persistence.recall_limit`.
    pub recall_limit: usize,
    /// `services.memory.persistence.cross_session_score_threshold`.
    pub cross_session_score_threshold: f32,
    /// `services.memory.persistence.context_format` — determines recall entry formatting.
    pub context_format: zeph_config::ContextFormat,
    /// `services.memory.persistence.last_recall_confidence` — written by apply path.
    pub last_recall_confidence: &'a mut Option<f32>,

    /// `services.memory.compaction.context_strategy` (Copy enum).
    pub context_strategy: ContextStrategy,
    /// `services.memory.compaction.crossover_turn_threshold`.
    pub crossover_turn_threshold: u32,
    /// `services.memory.compaction.cached_session_digest` — cloned into assembler input.
    ///
    /// The `usize` is the token count of the digest (used by `ContextMemoryView`).
    pub cached_session_digest: Option<(String, usize)>,
    /// `services.memory.compaction.digest_config.enabled`.
    pub digest_enabled: bool,

    /// `services.memory.extraction.graph_config` — cloned (small, `Clone`).
    pub graph_config: GraphConfig,
    /// `services.memory.extraction.document_config` — cloned.
    pub document_config: DocumentConfig,
    /// `services.memory.extraction.persona_config` — cloned.
    pub persona_config: PersonaConfig,
    /// `services.memory.extraction.trajectory_config` — cloned.
    pub trajectory_config: TrajectoryConfig,
    /// `services.memory.extraction.reasoning_config` — cloned.
    pub reasoning_config: ReasoningConfig,
    /// `services.memory.extraction.memcot_config` — cloned.
    pub memcot_config: zeph_config::MemCotConfig,
    /// Current `MemCoT` semantic state buffer. `Some` when the accumulator has a non-empty state.
    ///
    /// Snapshot taken at context-assembly time; used to prefix graph recall queries.
    pub memcot_state: Option<String>,
    /// `services.memory.subsystems.tree_config` — cloned.
    pub tree_config: TreeConfig,

    // ── Skill ─────────────────────────────────────────────────────────────────────────
    /// `services.skill.last_skills_prompt` — written by `Agent<C>::rebuild_system_prompt`.
    pub last_skills_prompt: &'a mut String,
    /// `services.skill.active_skill_names` — written by `Agent<C>::rebuild_system_prompt`.
    pub active_skill_names: &'a mut Vec<String>,
    /// `services.skill.registry` — `Arc` clone enables concurrent read access.
    pub skill_registry: Arc<RwLock<SkillRegistry>>,
    /// `services.skill.skill_paths` — read during proactive reload.
    pub skill_paths: &'a [PathBuf],

    // ── Index (feature-gated) ─────────────────────────────────────────────────────────
    /// Built at the shim by `IndexState::as_index_access()`. The lifetime reflects
    /// the borrow back into `services.index`.
    ///
    /// Only populated when the `index` feature is enabled.
    #[cfg(feature = "index")]
    pub index: Option<&'a dyn zeph_context::input::IndexAccess>,

    // ── Learning / sidequest / proactive ──────────────────────────────────────────────
    /// Built at the shim from `services.learning_engine.config` — the engine itself
    /// never crosses the crate boundary.
    pub correction_config: Option<CorrectionConfig>,
    /// `services.sidequest.turn_counter`.
    pub sidequest_turn_counter: u64,
    /// `services.proactive_explorer` — `Arc` clone for async use without borrowing self.
    pub proactive_explorer: Option<Arc<ProactiveExplorer>>,

    // ── Security ──────────────────────────────────────────────────────────────────────
    /// `services.security.sanitizer` — borrowed from `SecurityState`; not Arc-wrapped in `zeph-core`.
    pub sanitizer: &'a ContentSanitizer,
    /// `services.security.quarantine_summarizer` — borrowed from `SecurityState`.
    pub quarantine_summarizer: Option<&'a QuarantinedSummarizer>,

    // ── Context manager ───────────────────────────────────────────────────────────────
    /// `self.context_manager` — mutably borrowed for token recompute hooks.
    pub context_manager: &'a mut ContextManager,

    // ── Runtime / metrics ─────────────────────────────────────────────────────────────
    /// `runtime.metrics.token_counter` — `Arc` clone is cheap.
    pub token_counter: Arc<zeph_memory::TokenCounter>,
    /// Accumulated metric deltas — incremented during the pass, applied to the metrics
    /// snapshot by the `zeph-core` shim after `prepare_context` returns.
    pub metrics: MetricsCounters,
    /// Abstract sink for security events raised during context assembly.
    pub security_events: &'a mut dyn SecurityEventSink,
    /// `runtime.providers.cached_prompt_tokens` — read for compression-spectrum ratio.
    pub cached_prompt_tokens: u64,

    // ── Config flags ──────────────────────────────────────────────────────────────────
    /// `runtime.config.redact_credentials`.
    pub redact_credentials: bool,
    /// `runtime.config.channel_skills` — per-channel skill filter for system prompt rebuild.
    pub channel_skills: &'a [String],

    // ── Credential scrubber ───────────────────────────────────────────────────────────
    /// Function pointer for scrubbing credentials from message content.
    ///
    /// Passed as a function pointer so `zeph-agent-context` does not need to depend on
    /// `zeph-core::redact`. The shim in `zeph-core` sets this to `crate::redact::scrub_content`.
    /// When `redact_credentials = false` the service does not call this function.
    pub scrub: fn(&str) -> Cow<'_, str>,
}

/// Values produced by [`crate::service::ContextService::prepare_context`] that must be applied by the caller.
///
/// `ContextService` cannot inject code context directly because `inject_code_context` touches
/// the system prompt (position-0 message), which involves subsystems beyond the context-window
/// boundary. Instead, the service returns the code-context body and the caller applies it.
#[derive(Debug, Default)]
pub struct ContextDelta {
    /// Sanitized code-context body to inject into the system prompt by the `Agent<C>` shim.
    ///
    /// `None` when no code context was fetched or the fetch returned empty.
    pub code_context: Option<String>,
}

/// Borrow-lens over all fields needed for compaction and summarization operations.
///
/// Every field maps to a specific sub-field of `Agent<C>` and uses a type from a
/// crate below `zeph-core` in the dependency graph. Constructed in `zeph-core` using
/// one literal struct expression; the borrow checker verifies disjointness.
///
/// The view covers: message history mutation, deferred summary queues, context-manager
/// compaction state, provider handles for LLM calls, memory persistence for flushing,
/// subgoal registry for context-compression strategies, and background task handles for
/// non-blocking goal/subgoal extraction.
pub struct ContextSummarizationView<'a> {
    // ── Message window ────────────────────────────────────────────────────────
    /// Full conversation history. Mutated by pruning, compaction, and deferred summary
    /// application.
    pub messages: &'a mut Vec<Message>,
    /// `SQLite` row IDs to be soft-deleted after deferred summaries are applied.
    pub deferred_db_hide_ids: &'a mut Vec<i64>,
    /// Summary strings paired with the hide IDs above — flushed to `SQLite` as a batch.
    pub deferred_db_summaries: &'a mut Vec<String>,
    /// Running token count for the current prompt window. Updated after every mutation
    /// that changes message content.
    pub cached_prompt_tokens: &'a mut u64,

    // ── Context manager ───────────────────────────────────────────────────────
    /// Full context manager — contains compaction state, thresholds, strategy config.
    pub context_manager: &'a mut ContextManager,

    // ── Runtime ───────────────────────────────────────────────────────────────
    /// Whether server-side compaction is currently active (skip client compaction when
    /// true, unless context has grown past the safety fallback threshold).
    pub server_compaction_active: bool,
    /// Token counter used for budget calculations and prompt recomputation.
    pub token_counter: Arc<TokenCounter>,
    /// Pre-built summarization deps (provider + timeout + `token_counter` + callbacks).
    /// Built by the `zeph-core` shim from `build_summarization_deps()` before constructing
    /// the view, so the view does not need to hold a raw `DebugDumper` reference.
    pub summarization_deps: SummarizationDeps,
    /// Background task supervisor for spawning non-blocking goal/subgoal extractions.
    pub task_supervisor: Arc<TaskSupervisor>,

    // ── Memory persistence ────────────────────────────────────────────────────
    /// Semantic memory store — used to flush deferred summaries and store session digests.
    pub memory: Option<Arc<SemanticMemory>>,
    /// Conversation ID for all SQLite/Qdrant persistence calls.
    pub conversation_id: Option<ConversationId>,
    /// Maximum unsummarized tool-call pairs before forced deferred summarization kicks in.
    pub tool_call_cutoff: usize,

    // ── Context-compression (SubgoalRegistry + task handles) ─────────────────
    /// In-memory registry of all subgoals in the current session.
    pub subgoal_registry: &'a mut SubgoalRegistry,
    /// Handle to the background task-goal extraction spawned last turn.
    pub pending_task_goal: &'a mut Option<BlockingHandle<Option<String>>>,
    /// Handle to the background subgoal extraction spawned last turn.
    pub pending_subgoal: &'a mut Option<BlockingHandle<Option<SubgoalExtractionResult>>>,
    /// Cached task goal for `TaskAware`/`MIG` pruning. `None` before first extraction.
    pub current_task_goal: &'a mut Option<String>,
    /// Hash of the last user message when `current_task_goal` was populated.
    /// Used to detect when a new extraction is needed.
    pub task_goal_user_msg_hash: &'a mut Option<u64>,
    /// Hash of the last user message when subgoal extraction was scheduled.
    pub subgoal_user_msg_hash: &'a mut Option<u64>,
    /// TUI / channel status sender for spinner messages. `None` when TUI is disabled.
    pub status_tx: Option<tokio::sync::mpsc::UnboundedSender<String>>,

    // ── Credential scrubber ───────────────────────────────────────────────────
    /// Function pointer for scrubbing credentials from summary text.
    ///
    /// Set to `crate::redact::scrub_content` by the `zeph-core` shim when
    /// `redact_credentials = true`, or to a no-op identity function otherwise.
    pub scrub: fn(&str) -> Cow<'_, str>,

    // ── Compaction callbacks (populated by zeph-core shim) ────────────────────
    /// Compression guidelines text loaded from `SQLite` by the `zeph-core` shim.
    ///
    /// `None` when the feature is disabled or the caller does not load guidelines.
    /// The service passes the contained string (or `""`) to `summarize_with_llm`. Closes #3528.
    ///
    /// Set via [`ContextSummarizationView::with_compression_guidelines`]. Both the reactive
    /// (`compact_context`) and proactive (`maybe_proactive_compress`) paths populate this field.
    pub compression_guidelines: Option<String>,

    /// Optional probe-validation callback. When `Some`, the service invokes it after LLM
    /// summarization and before draining/reinsert. See [`CompactionProbeCallback`] for the
    /// full implementor contract.
    pub probe: Option<&'a mut dyn CompactionProbeCallback>,

    /// Optional pre-summary archive hook (Memex #2432). The service calls `archive(to_compact)`
    /// BEFORE summarization and appends the returned reference list as a postfix AFTER the
    /// LLM call so the LLM cannot destroy the `[archived:UUID]` markers.
    pub archive: Option<&'a dyn ToolOutputArchive>,

    /// Optional persistence completion callback. The service calls `after_compaction` once
    /// the in-memory drain+reinsert is finalized. The optional Qdrant future returned by the
    /// callback is bubbled back through [`CompactionOutcome::Compacted::qdrant_future`].
    pub persistence: Option<&'a dyn CompactionPersistence>,

    /// Metrics sink for compaction-related counter increments. Used for
    /// `compaction_hard_count`, `tool_output_prunes`, and the four probe-outcome counters.
    /// Closes #3527.
    pub metrics: Option<&'a dyn MetricsCallback>,
}

impl ContextSummarizationView<'_> {
    /// Set the compression guidelines text.
    ///
    /// Call this on the view returned by `Agent::summarization_view()` before passing it to
    /// `ContextService::compact_context`. Using a builder method keeps construction uniform
    /// and avoids direct field mutation.
    #[must_use]
    pub fn with_compression_guidelines(mut self, guidelines: Option<String>) -> Self {
        self.compression_guidelines = guidelines;
        self
    }
}

/// Bundle of LLM provider handles needed for async context operations.
///
/// Each handle is an `Arc`-backed clone, suitable for moving into spawned tasks
/// or passing across async boundaries.
pub struct ProviderHandles {
    /// Primary LLM provider used for completions and compaction.
    pub primary: AnyProvider,
    /// Dedicated embedding provider.
    pub embedding: AnyProvider,
}

/// Abstract status sink for emitting short progress strings to the channel.
///
/// Implemented in `zeph-core` by a stack-local adapter wrapping `Channel::send_status`.
/// Using a trait keeps this crate free of the `Channel` trait from `zeph-core`.
pub trait StatusSink: Send + Sync {
    /// Send a short status string to the active channel.
    fn send_status(&self, msg: &str) -> impl Future<Output = ()> + Send + '_;
}

/// Abstract gate for applying a skill trust level to the tool executor.
///
/// Implemented in `zeph-core` by a thin adapter over `Arc<dyn ErasedToolExecutor>`.
/// Using a trait keeps this crate free of the tool executor abstraction.
pub trait TrustGate: Send + Sync {
    /// Apply the given trust level to the underlying tool executor.
    fn set_effective_trust(&self, level: zeph_common::SkillTrustLevel);
}

/// Boxed `'static` future for the off-thread Qdrant session-summary write.
///
/// Returned from [`CompactionPersistence::after_compaction`] and bubbled back through
/// [`CompactionOutcome::Compacted`] / [`CompactionOutcome::CompactedWithPersistError`].
/// The caller (shim in `zeph-core`) dispatches this through `BackgroundSupervisor::spawn_summarization`.
/// The future must return `bool` (`false` = success, `true` = error) to match the supervisor API.
pub type QdrantPersistFuture = Pin<Box<dyn Future<Output = bool> + Send + 'static>>;

/// Return type from `compact_context()` that distinguishes between successful compaction,
/// probe rejection, and no-op.
///
/// Gives `maybe_compact()` enough information to handle probe rejection without triggering
/// the `Exhausted` state — which would only be correct if summarization itself is stuck.
#[must_use]
pub enum CompactionOutcome {
    /// Messages were drained and replaced with a summary. `SQLite` persistence succeeded.
    ///
    /// `qdrant_future` is an optional `'static` future for the off-thread Qdrant write;
    /// the shim must dispatch it through `BackgroundSupervisor::spawn_summarization` and
    /// must not await it inline.
    Compacted {
        /// Optional Qdrant write future to dispatch via the supervisor.
        qdrant_future: Option<QdrantPersistFuture>,
    },
    /// Messages were drained and replaced with a summary, but synchronous `SQLite` persistence
    /// reported failure. The in-memory state is correct; only persistence failed.
    CompactedWithPersistError {
        /// Optional Qdrant write future to dispatch via the supervisor.
        qdrant_future: Option<QdrantPersistFuture>,
    },
    /// Probe rejected the summary — original messages are preserved.
    /// Caller must NOT check `freed_tokens` or transition to `Exhausted`.
    ProbeRejected,
    /// No compaction was performed (too few messages, empty `to_compact`, etc.).
    NoChange,
}

impl PartialEq for CompactionOutcome {
    fn eq(&self, other: &Self) -> bool {
        // Compare variants only; qdrant_future is not comparable (it is a dyn Future).
        matches!(
            (self, other),
            (Self::Compacted { .. }, Self::Compacted { .. })
                | (
                    Self::CompactedWithPersistError { .. },
                    Self::CompactedWithPersistError { .. }
                )
                | (Self::ProbeRejected, Self::ProbeRejected)
                | (Self::NoChange, Self::NoChange)
        )
    }
}

impl std::fmt::Debug for CompactionOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Compacted { qdrant_future } => f
                .debug_struct("Compacted")
                .field("qdrant_future", &qdrant_future.as_ref().map(|_| "<future>"))
                .finish(),
            Self::CompactedWithPersistError { qdrant_future } => f
                .debug_struct("CompactedWithPersistError")
                .field("qdrant_future", &qdrant_future.as_ref().map(|_| "<future>"))
                .finish(),
            Self::ProbeRejected => write!(f, "ProbeRejected"),
            Self::NoChange => write!(f, "NoChange"),
        }
    }
}

impl CompactionOutcome {
    /// Remove and return the Qdrant persistence future embedded in `Compacted` or
    /// `CompactedWithPersistError` variants. Returns `None` for `ProbeRejected` / `NoChange`.
    ///
    /// The shim calls this immediately after the service returns and dispatches the
    /// future through `BackgroundSupervisor::spawn_summarization`.
    pub fn qdrant_future_take(&mut self) -> Option<QdrantPersistFuture> {
        match self {
            Self::Compacted { qdrant_future }
            | Self::CompactedWithPersistError { qdrant_future } => qdrant_future.take(),
            _ => None,
        }
    }

    /// Returns `true` when compaction succeeded (either variant of `Compacted`).
    #[must_use]
    pub fn is_compacted(&self) -> bool {
        matches!(
            self,
            Self::Compacted { .. } | Self::CompactedWithPersistError { .. }
        )
    }
}

/// Verdict returned by a [`CompactionProbeCallback`] after evaluating a candidate summary.
///
/// The implementor — not the service — is responsible for routing the verdict-specific data
/// (score, `category_scores`, thresholds) through [`MetricsCallback`] and for calling
/// `dump_compaction_probe` before returning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// Probe accepted the summary; pipeline continues normally.
    Pass,
    /// Probe soft-rejected; pipeline continues but the summary is flagged as borderline.
    SoftFail,
    /// Probe hard-rejected; service must abort and return [`CompactionOutcome::ProbeRejected`].
    HardFail,
}

/// Probe-validation callback invoked by `ContextService::compact_context` after the LLM
/// produces a candidate summary.
///
/// # Contract (mandatory)
///
/// Implementations MUST, before returning:
/// 1. Call `dump_compaction_probe(result)` if a debug dumper is configured.
/// 2. Update verdict-specific metric counters via the appropriate
///    `MetricsCallback::record_compaction_probe_*` method. The score, `category_scores`,
///    threshold, and `hard_fail_threshold` travel through the metrics adapter and are not
///    part of the `ProbeOutcome` payload.
/// 3. On internal validation error (`validate_compaction` returns `Err`), call
///    `MetricsCallback::record_compaction_probe_error()` and return `ProbeOutcome::Pass`.
///    An error must not abort compaction.
///
/// The service treats the returned `ProbeOutcome` exclusively as routing:
/// `HardFail` → abort with `ProbeRejected`; `Pass | SoftFail` → continue.
pub trait CompactionProbeCallback: Send {
    /// Validate the candidate `summary` produced from `to_compact` messages.
    fn validate<'a>(
        &'a mut self,
        to_compact: &'a [Message],
        summary: &'a str,
    ) -> Pin<Box<dyn Future<Output = ProbeOutcome> + Send + 'a>>;
}

/// Pre-summary tool-output archiving hook (Memex #2432).
///
/// The service calls `archive(to_compact)` BEFORE summarization. The returned reference
/// strings are appended as a postfix AFTER the LLM summary to prevent the LLM from
/// destroying the `[archived:UUID]` markers.
pub trait ToolOutputArchive: Send + Sync {
    /// Archive tool output bodies from `to_compact` and return reference strings.
    ///
    /// Returns an empty `Vec` when archiving is disabled or no bodies are archived.
    fn archive<'a>(
        &'a self,
        to_compact: &'a [Message],
    ) -> Pin<Box<dyn Future<Output = Vec<String>> + Send + 'a>>;
}

/// Persistence completion hook invoked after the in-memory drain/reinsert is finalized.
///
/// Returns:
/// - `persist_failed`: whether the synchronous `SQLite` persistence step failed.
/// - `qdrant_future`: optional `'static` future for the off-thread Qdrant write, bubbled
///   back to the caller via [`CompactionOutcome::Compacted::qdrant_future`].
pub trait CompactionPersistence: Send + Sync {
    /// Persist the compaction result and return the Qdrant write future.
    fn after_compaction<'a>(
        &'a self,
        compacted_count: usize,
        summary_content: &'a str,
        summary: &'a str,
    ) -> Pin<Box<dyn Future<Output = (bool, Option<QdrantPersistFuture>)> + Send + 'a>>;
}

/// Metrics-counter sink for `ContextService` increments.
///
/// Implemented in `zeph-core` by an adapter wrapping `Arc<MetricsCollector>`. Keeps
/// `zeph-agent-context` free of `zeph-core` internal metrics types. Closes #3527.
///
/// All four `record_compaction_probe_*` methods are called from inside the
/// [`CompactionProbeCallback`] implementation — not from the service itself — per the
/// probe-callback contract.
pub trait MetricsCallback: Send + Sync {
    /// Record that a hard-compaction event occurred.
    ///
    /// `turns_since_last` is `None` on the first hard compaction of the session.
    fn record_hard_compaction(&self, turns_since_last: Option<u32>);

    /// Record that tool outputs were pruned.
    ///
    /// `count` is the number of tool-output bodies pruned in this pass.
    fn record_tool_output_prune(&self, count: usize);

    /// Record a probe pass verdict with full score data.
    fn record_compaction_probe_pass(
        &self,
        score: f32,
        category_scores: Vec<zeph_memory::CategoryScore>,
        threshold: f32,
        hard_fail_threshold: f32,
    );

    /// Record a probe soft-fail verdict with full score data.
    fn record_compaction_probe_soft_fail(
        &self,
        score: f32,
        category_scores: Vec<zeph_memory::CategoryScore>,
        threshold: f32,
        hard_fail_threshold: f32,
    );

    /// Record a probe hard-fail verdict with full score data.
    fn record_compaction_probe_hard_fail(
        &self,
        score: f32,
        category_scores: Vec<zeph_memory::CategoryScore>,
        threshold: f32,
        hard_fail_threshold: f32,
    );

    /// Record that the probe returned an error (non-fatal; compaction proceeded).
    fn record_compaction_probe_error(&self);
}
