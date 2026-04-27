// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Error types for the context-assembly service.

use thiserror::Error;

/// Errors that can occur during agent context-assembly operations.
///
/// The caller in `zeph-core` maps these variants to `AgentError` via a `From` impl.
/// Each variant corresponds to one subsystem that the context service touches.
#[derive(Debug, Error)]
pub enum ContextError {
    /// Memory backend returned an error during recall or summary loading.
    ///
    /// The agent should degrade gracefully — inject an empty recall section and continue.
    #[error("memory error during context assembly: {0}")]
    Memory(#[from] zeph_memory::MemoryError),

    /// Context assembler in `zeph-context` returned an error.
    #[error("context assembler error: {0}")]
    Assembler(#[from] zeph_context::error::ContextError),

    /// Serialization failed (e.g., building a JSON payload for the LLM).
    #[error("serialization error: {0}")]
    Serialize(#[from] serde_json::Error),

    /// An operation timed out (e.g., spreading-activation recall, LLM probe).
    #[error("operation timed out: {operation}")]
    Timeout {
        /// Short name of the operation that timed out.
        operation: &'static str,
    },

    /// A required provider handle was absent (embedding or primary provider not configured).
    #[error("provider not configured: {0}")]
    ProviderMissing(&'static str),
}
