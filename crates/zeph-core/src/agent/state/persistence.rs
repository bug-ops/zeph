// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `SQLite` and conversation persistence state for the agent's memory subsystem.
//!
//! [`MemoryPersistenceState`] groups fields that control how the agent stores and recalls
//! conversation history: the semantic memory handle, conversation tracking, recall budgets,
//! and autosave policy.

use std::sync::Arc;

use zeph_config::ContextFormat;
use zeph_config::memory::{TieredRetrievalConfig, TypeAwareComposeConfig};
use zeph_llm::any::AnyProvider;
use zeph_memory::semantic::SemanticMemory;

/// Cross-thread store owner key used for every dispatch path (spec-080 §10 OQ-1,
/// GitHub #6363): the produce-side Command-handoff seam (`scheduler_loop.rs`, feature
/// `scheduler`-gated) and the always-available `/store` slash command
/// (`agent_access_impl.rs`) both need this constant, so it lives here rather than in
/// either feature-specific caller.
///
/// CLI/Telegram/gateway/A2A callers all collapse to this single bucket in v1 — correct
/// for CLI/TUI (genuinely single-user) but a documented, deferred tenancy blind spot for
/// gateway/A2A (multiple callers sharing one bearer token would land in the same
/// bucket). Not a bug: the schema and every store method already take `owner_key`
/// everywhere, so threading a real per-caller key through later is additive, not a
/// breaking migration. Tracked as a follow-up per spec-080 §10 OQ-1 — see GitHub #6389.
pub(crate) const DEFAULT_OWNER_KEY: &str = "local";

/// `SQLite` connection, conversation tracking, history limits, recall budget, and autosave policy.
///
/// All fields in this struct relate to the *persistence* concern: how messages are stored
/// in `SQLite`, how many are loaded per turn, and when they are automatically saved.
pub(crate) struct MemoryPersistenceState {
    /// Semantic memory backend (`SQLite` + `Qdrant`). `None` when memory is disabled.
    pub(crate) memory: Option<Arc<SemanticMemory>>,
    /// Active conversation ID in `SQLite`. `None` before the first message is persisted.
    pub(crate) conversation_id: Option<zeph_memory::ConversationId>,
    /// Maximum number of historical messages loaded from `SQLite` per turn.
    pub(crate) history_limit: u32,
    /// Maximum number of semantic recall hits injected per turn.
    pub(crate) recall_limit: usize,
    /// Minimum semantic similarity score for cross-session recall (0.0–1.0).
    pub(crate) cross_session_score_threshold: f32,
    /// When `true`, assistant messages are auto-saved to `SQLite` after each turn.
    pub(crate) autosave_assistant: bool,
    /// Minimum assistant message length (in characters) to trigger autosave.
    pub(crate) autosave_min_length: usize,
    /// Maximum number of tool call pairs retained in context before summarization.
    pub(crate) tool_call_cutoff: usize,
    /// Running count of messages added since the last compaction.
    pub(crate) unsummarized_count: usize,
    /// Top-1 semantic recall score from the most recent `prepare_context` cycle.
    ///
    /// Used by MAR (Memory-Augmented Routing) to bias the bandit toward cheap providers
    /// when memory confidence is high. Reset to `None` at the start of each turn.
    pub(crate) last_recall_confidence: Option<f32>,
    /// Memory snippet rendering format for context assembly (MM-F5, #3340).
    ///
    /// Applied exclusively in `assembler_helpers::fetch_semantic_recall` — never persisted.
    pub(crate) context_format: ContextFormat,

    // ── MemFlow tiered retrieval (#3712) ─────────────────────────────────────────
    /// `MemFlow` tiered retrieval configuration snapshot.
    ///
    /// Stored here so `ContextAssemblyView` can read it without accessing the full
    /// config tree. Set by `with_tiered_retrieval_providers`.
    pub(crate) tiered_retrieval_config: TieredRetrievalConfig,
    /// Optional provider for LLM-backed intent classification in tiered retrieval.
    ///
    /// `None` when `tiered_retrieval.classifier_provider` is empty; falls back to
    /// `HeuristicRouter`. Resolved at agent construction, never changed at runtime.
    pub(crate) tiered_retrieval_classifier: Option<Arc<AnyProvider>>,
    /// Optional provider for evidence quality validation and tier escalation.
    ///
    /// `None` when `tiered_retrieval.validator_provider` is empty. Resolved at agent
    /// construction, never changed at runtime.
    pub(crate) tiered_retrieval_validator: Option<Arc<AnyProvider>>,

    // ── MemGuard type-aware retrieval composition (spec 004-16, #6086) ──────────────
    /// Type-aware retrieval composition configuration snapshot (`[memory.type_aware_compose]`).
    ///
    /// Stored here so `ContextAssemblyView` can read it without accessing the full config
    /// tree. Set by `with_type_aware_compose_config`. No LLM providers are involved (v1 has
    /// no LLM classifier — see spec 004-16 §5).
    pub(crate) type_aware_compose_config: TypeAwareComposeConfig,

    // ── Cross-thread store (spec-080, GitHub #6363) ─────────────────────────────────
    /// Cross-thread store configuration snapshot (`[memory.store]`).
    ///
    /// Stored here so `scheduler_loop.rs`'s Command-handoff produce-side seam can read
    /// `enabled`/`max_value_bytes` without accessing the full config tree, mirroring
    /// `tiered_retrieval_config`/`type_aware_compose_config` above. Set via
    /// `AgentSessionConfig`/`apply_session_config`. The actual store I/O goes through
    /// `memory` (above) via `SemanticMemory::sqlite()`'s cross-thread-store methods —
    /// this field only gates and bounds those calls.
    pub(crate) store_config: zeph_config::CrossThreadStoreConfig,
}

impl Default for MemoryPersistenceState {
    fn default() -> Self {
        Self {
            memory: None,
            conversation_id: None,
            history_limit: 50,
            recall_limit: 5,
            cross_session_score_threshold: 0.35,
            autosave_assistant: false,
            autosave_min_length: 20,
            tool_call_cutoff: 6,
            unsummarized_count: 0,
            last_recall_confidence: None,
            context_format: ContextFormat::default(),
            tiered_retrieval_config: TieredRetrievalConfig::default(),
            tiered_retrieval_classifier: None,
            tiered_retrieval_validator: None,
            type_aware_compose_config: TypeAwareComposeConfig::default(),
            store_config: zeph_config::CrossThreadStoreConfig::default(),
        }
    }
}
