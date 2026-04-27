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

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use parking_lot::RwLock;
use zeph_common::SecurityEventCategory;
use zeph_config::{
    ContextStrategy, DocumentConfig, GraphConfig, PersonaConfig, ReasoningConfig, TrajectoryConfig,
    TreeConfig,
};
use zeph_context::input::CorrectionConfig;
use zeph_context::manager::ContextManager;
use zeph_llm::any::AnyProvider;
use zeph_llm::provider::Message;
use zeph_memory::ConversationId;
use zeph_memory::semantic::SemanticMemory;
use zeph_sanitizer::ContentSanitizer;
use zeph_sanitizer::quarantine::QuarantinedSummarizer;
use zeph_skills::proactive::ProactiveExplorer;
use zeph_skills::registry::SkillRegistry;

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
}

/// Narrow mutable counters lens for sanitizer and quarantine metrics.
///
/// Groups five disjoint sub-fields of `Agent<C>::runtime.metrics` into one struct
/// so the borrow checker can prove disjointness at the shim literal expression.
pub struct MetricsCounters<'a> {
    /// Total sanitizer checks performed.
    pub sanitizer_runs: &'a mut u64,
    /// Total injection flags raised by the sanitizer.
    pub sanitizer_injection_flags: &'a mut u64,
    /// Total truncations applied by the sanitizer.
    pub sanitizer_truncations: &'a mut u64,
    /// Total quarantine invocations.
    pub quarantine_invocations: &'a mut u64,
    /// Total quarantine failures (summarizer error or timeout).
    pub quarantine_failures: &'a mut u64,
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

/// Borrow-lens over all fields needed for `prepare_context` and `rebuild_system_prompt`.
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
    pub cached_session_digest: Option<(String, Instant)>,
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
    /// `services.memory.subsystems.tree_config` — cloned.
    pub tree_config: TreeConfig,

    // ── Skill ─────────────────────────────────────────────────────────────────────────
    /// `services.skill.last_skills_prompt` — written by `rebuild_system_prompt`.
    pub last_skills_prompt: &'a mut String,
    /// `services.skill.active_skill_names` — written by `rebuild_system_prompt`.
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
    pub sidequest_turn_counter: u32,
    /// `services.proactive_explorer` — `Arc` clone for async use without borrowing self.
    pub proactive_explorer: Option<Arc<ProactiveExplorer>>,

    // ── Security ──────────────────────────────────────────────────────────────────────
    /// `services.security.sanitizer` — `Arc` clone is cheap.
    pub sanitizer: Arc<ContentSanitizer>,
    /// `services.security.quarantine_summarizer` — `Arc` clone is cheap.
    pub quarantine_summarizer: Option<Arc<QuarantinedSummarizer>>,

    // ── Context manager ───────────────────────────────────────────────────────────────
    /// `self.context_manager` — mutably borrowed for token recompute hooks.
    pub context_manager: &'a mut ContextManager,

    // ── Runtime / metrics ─────────────────────────────────────────────────────────────
    /// `runtime.metrics.token_counter` — `Arc` clone is cheap.
    pub token_counter: Arc<zeph_memory::TokenCounter>,
    /// Five disjoint mutable counters from `runtime.metrics`.
    pub metrics: MetricsCounters<'a>,
    /// Abstract sink for security events raised during context assembly.
    pub security_events: &'a mut dyn SecurityEventSink,
    /// `runtime.providers.cached_prompt_tokens` — read for compression-spectrum ratio.
    pub cached_prompt_tokens: u64,

    // ── Config flags ──────────────────────────────────────────────────────────────────
    /// `runtime.config.redact_credentials`.
    pub redact_credentials: bool,
    /// `runtime.config.channel_skills` — per-channel skill filter for system prompt rebuild.
    pub channel_skills: &'a [String],
}

/// Borrow-lens over fields needed for compaction and summarization operations.
///
/// Fields are enumerated in Step 8 of the migration once the summarization
/// sub-module is moved. This placeholder holds the minimum surface needed by
/// the scaffold stage.
///
/// # TODO(review): enumerate full field set in Step 8 migration
pub struct ContextSummarizationView<'a> {
    #[doc(hidden)]
    pub _phantom: std::marker::PhantomData<&'a ()>,
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
