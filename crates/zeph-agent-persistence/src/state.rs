// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Borrow-lens view types used by [`crate::service::PersistenceService`].
//!
//! These structs hold `&`/`&mut` references to the exact sub-fields that the persistence
//! service needs. By accepting lenses instead of `&mut Agent<C>`, the new crate avoids
//! depending on `zeph-core` while still letting the call site in `zeph-core` construct them
//! from disjoint field projections.
//!
//! Each view is constructed at the call site in `zeph-core` using one literal struct expression.
//! The borrow checker can prove disjointness at that level without additional helper methods.

use std::sync::Arc;

use zeph_llm::any::AnyProvider;
use zeph_memory::semantic::SemanticMemory;

/// Borrow-lens view over the agent's memory persistence state fields.
///
/// Aggregates the exact fields that [`crate::service::PersistenceService`] reads and writes
/// from `MemoryPersistenceState`. Constructed by the `zeph-core` shim with one literal expression.
pub struct MemoryPersistenceView<'a> {
    /// Semantic memory backend (`SQLite` + Qdrant). `None` when memory is disabled.
    pub memory: Option<&'a Arc<SemanticMemory>>,
    /// Active conversation ID. `None` before the first message.
    pub conversation_id: Option<zeph_memory::ConversationId>,
    /// When `true`, assistant messages are auto-saved to Qdrant.
    pub autosave_assistant: bool,
    /// Minimum length (chars) for assistant autosave.
    pub autosave_min_length: usize,
    /// Mutable count of messages added since last compaction.
    pub unsummarized_count: &'a mut usize,
    /// Optional current goal text for embedding enrichment.
    pub goal_text: Option<String>,
}

/// Borrow-lens view over the agent's security state fields used during persistence.
///
/// Read-only — no mutation needed in the persistence service.
pub struct SecurityView<'a> {
    /// When `true`, Qdrant embedding is skipped for messages with injection flags.
    pub guard_memory_writes: bool,
    /// Phantom to bind the lifetime of the borrow to the parent.
    pub _phantom: std::marker::PhantomData<&'a ()>,
}

/// Borrow-lens view over the agent's metrics state fields used during persistence.
pub struct MetricsView<'a> {
    /// Mutable total `SQLite` message count.
    pub sqlite_message_count: &'a mut u64,
    /// Mutable total embeddings generated counter.
    pub embeddings_generated: &'a mut u64,
    /// Mutable exfiltration guard event counter.
    pub exfiltration_memory_guards: &'a mut u64,
}

/// Bundle of provider handles needed by background extraction tasks.
///
/// Each handle is an `Arc`-backed clone, suitable for moving into spawned tasks.
pub struct ProviderHandles {
    /// Primary LLM provider (used as fallback for background tasks).
    pub primary: AnyProvider,
    /// Dedicated embedding provider.
    pub embedding: AnyProvider,
}
