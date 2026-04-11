// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `SQLite` and conversation persistence state for the agent's memory subsystem.
//!
//! [`MemoryPersistenceState`] groups fields that control how the agent stores and recalls
//! conversation history: the semantic memory handle, conversation tracking, recall budgets,
//! and autosave policy.

use std::sync::Arc;

use zeph_memory::semantic::SemanticMemory;

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
        }
    }
}
