// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Error type for context assembly operations.

use thiserror::Error;

/// Errors that can occur during context assembly.
///
/// All async fetch operations in [`crate::assembler::ContextAssembler`] propagate
/// errors through this type. Callers in `zeph-core` convert to `AgentError` at the
/// boundary using `From<ContextError> for AgentError`.
#[derive(Debug, Error)]
pub enum ContextError {
    /// A memory subsystem operation failed.
    #[error("memory error: {0}")]
    Memory(#[from] zeph_memory::MemoryError),

    /// An unexpected error occurred during context assembly.
    #[error("context assembly error: {0}")]
    Assembly(String),
}
