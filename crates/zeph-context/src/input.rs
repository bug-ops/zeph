// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Input types for context assembly.
//!
//! [`ContextAssemblyInput`] collects all references needed for one assembly turn.
//! [`ContextMemoryView`] is a snapshot of memory-subsystem configuration that the
//! assembler reads but never mutates — callers in `zeph-core` populate it from
//! `MemoryState` before each assembly pass.

use std::borrow::Cow;
use std::sync::Arc;

use zeph_config::{
    DocumentConfig, GraphConfig, PersonaConfig, ReasoningConfig, TrajectoryConfig, TreeConfig,
};
use zeph_memory::compression::CompressionLevel;
use zeph_memory::semantic::SemanticMemory;
use zeph_memory::{ConversationId, TokenCounter};

use crate::manager::ContextManager;

/// All borrowed data needed to assemble context for one agent turn.
///
/// All fields are shared references — `ContextAssembler::gather` never mutates any state.
/// The caller (in `zeph-core`) is responsible for populating this struct and passing it to
/// [`crate::assembler::ContextAssembler::gather`].
pub struct ContextAssemblyInput<'a> {
    /// Snapshot of memory subsystem configuration for this turn.
    pub memory: &'a ContextMemoryView,
    /// Context lifecycle state machine.
    pub context_manager: &'a ContextManager,
    /// Token counter for budget enforcement.
    pub token_counter: &'a TokenCounter,
    /// Text of the skills prompt injected in the last turn (used for budget calculation).
    pub skills_prompt: &'a str,
    /// Index RAG accessor. `None` when code-index is disabled.
    pub index: Option<&'a dyn IndexAccess>,
    /// Learning engine corrections config. `None` when self-learning is disabled.
    pub correction_config: Option<CorrectionConfig>,
    /// Current value of the sidequest turn counter, for adaptive strategy selection.
    pub sidequest_turn_counter: u64,
    /// Message window snapshot used for strategy resolution and system-prompt extraction.
    pub messages: &'a [zeph_llm::provider::Message],
    /// The user query for the current turn, used as the search query for all memory lookups.
    pub query: &'a str,
    /// Content scrubber for PII removal. Passed as a function pointer to avoid a dependency
    /// on `zeph-core`'s redact module.
    pub scrub: fn(&str) -> Cow<'_, str>,
    /// Compression tiers active for this turn, derived from [`zeph_memory::compression::RetrievalPolicy`].
    ///
    /// The assembler skips fetchers whose tier is not present in this slice.
    /// An empty slice means "no tier filtering" — all fetchers run subject to their own budget
    /// gates. This is the defensive default: a caller that accidentally passes an empty slice
    /// will get the same behaviour as before this field existed, rather than silently dropping
    /// all memory recall.
    ///
    /// A caller computing this from a config-driven policy must guarantee non-empty intent or
    /// accept that an empty slice disables tier-based filtering entirely.
    pub active_levels: &'a [CompressionLevel],
}

/// Configuration extracted from `LearningEngine` needed by correction recall.
///
/// Populated from `LearningEngine::config` in `zeph-core` and passed into
/// [`ContextAssemblyInput`].
#[derive(Debug, Clone, Copy)]
pub struct CorrectionConfig {
    /// Whether correction detection is active.
    pub correction_detection: bool,
    /// Maximum number of corrections to recall per turn.
    pub correction_recall_limit: u32,
    /// Minimum similarity score for a correction to be considered relevant.
    pub correction_min_similarity: f32,
}

/// Read-only snapshot of memory subsystem state needed for context assembly.
///
/// This struct is populated by the caller (`zeph-core`) from `MemoryState` before each
/// assembly pass. It contains only the fields that [`crate::assembler::ContextAssembler`]
/// actually reads — no `Agent` methods, no mutation.
pub struct ContextMemoryView {
    // ── persistence fields ────────────────────────────────────────────────────
    /// Semantic memory backend. `None` when memory is disabled.
    pub memory: Option<Arc<SemanticMemory>>,
    /// Active conversation ID. `None` before the first message is persisted.
    pub conversation_id: Option<ConversationId>,
    /// Maximum number of semantic recall hits injected per turn.
    pub recall_limit: usize,
    /// Minimum semantic similarity score for cross-session recall (0.0–1.0).
    pub cross_session_score_threshold: f32,

    // ── compaction fields ─────────────────────────────────────────────────────
    /// Context assembly strategy (`FullHistory` / `MemoryFirst` / `Adaptive`).
    pub context_strategy: zeph_config::ContextStrategy,
    /// Turn threshold for `Adaptive` strategy crossover.
    pub crossover_turn_threshold: u32,
    /// Cached session digest text and token count, loaded at session start.
    pub cached_session_digest: Option<(String, usize)>,

    // ── extraction fields ─────────────────────────────────────────────────────
    /// Knowledge graph configuration.
    pub graph_config: GraphConfig,
    /// Document RAG configuration.
    pub document_config: DocumentConfig,
    /// Persona memory configuration.
    pub persona_config: PersonaConfig,
    /// Trajectory-informed memory configuration.
    pub trajectory_config: TrajectoryConfig,
    /// `ReasoningBank` configuration (#3343).
    pub reasoning_config: ReasoningConfig,

    // ── subsystem fields ──────────────────────────────────────────────────────
    /// `TiMem` temporal-hierarchical memory tree configuration.
    pub tree_config: TreeConfig,
}

/// Read-only access to a code-index retriever.
///
/// Implemented by `IndexState` in `zeph-core`. The assembler calls only `fetch_code_rag`
/// to populate the `code_context` slot.
///
/// The return type uses `Pin<Box<dyn Future>>` rather than `async fn` to preserve
/// dyn-compatibility: the trait is used as `&dyn IndexAccess` in `ContextAssemblyInput`.
pub trait IndexAccess: Send + Sync {
    /// Retrieve up to `budget_tokens` of code context for the given `query`.
    ///
    /// Returns `None` when no relevant context is found or when code-index is disabled.
    ///
    /// # Errors
    ///
    /// Propagates errors from the underlying code retriever.
    fn fetch_code_rag<'a>(
        &'a self,
        query: &'a str,
        budget_tokens: usize,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<Option<String>, crate::error::ContextError>>
                + Send
                + 'a,
        >,
    >;
}
