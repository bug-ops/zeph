// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Error types for the persistence service.

use thiserror::Error;

/// Errors that can occur during agent persistence operations.
///
/// The caller in `zeph-core` maps these variants to `AgentError` via `From<PersistenceError>`.
/// Callers can distinguish degradable errors (Qdrant offline) from fatal errors (`SQLite` corrupt).
#[derive(Debug, Error)]
pub enum PersistenceError {
    /// Qdrant vector store is unavailable. Embedding is skipped; `SQLite` write may still succeed.
    /// Callers should degrade gracefully and continue the conversation without semantic search.
    #[error("Qdrant unavailable: {0}")]
    QdrantUnavailable(String),

    /// `SQLite` database error. This is typically fatal — abort the current operation.
    #[error("SQLite error: {0}")]
    SqliteCorrupt(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),

    /// Memory backend returned a generic error that doesn't fit the categories above.
    #[error("memory backend error: {0}")]
    Memory(#[from] zeph_memory::MemoryError),

    /// Failed to serialize message parts to JSON.
    #[error("serialization error: {0}")]
    Serialize(#[from] serde_json::Error),
}
