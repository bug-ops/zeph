// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Crate-wide error type for `zeph-session`.

use thiserror::Error;

/// Errors produced by the session persistence, replay, and fork engines.
#[derive(Debug, Error)]
pub enum SessionError {
    /// Filesystem I/O failed while reading or appending to an event log.
    #[error("session event log I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// A `SessionEvent` line failed to (de)serialize.
    #[error("session event (de)serialization error: {0}")]
    Serde(#[from] serde_json::Error),

    /// The `acp_sessions` metadata store returned a database error.
    #[error("session store database error: {0}")]
    Db(#[from] zeph_db::SqlxError),

    /// A lookup by [`zeph_common::SessionId`] found no matching session.
    #[error("session not found: {0}")]
    NotFound(String),

    /// A fork was requested at a `seq` beyond the source session's `last_seq`.
    #[error("invalid fork point: {0}")]
    InvalidForkPoint(String),

    /// A condensation or compaction range overlapped a previously replaced range (INV-SP-4).
    #[error("condensation range overlap: {0}")]
    CondensationOverlap(String),

    /// Condensation summarization failed (LLM call error or timeout).
    #[error("condensation summarization failed: {0}")]
    Llm(#[from] zeph_llm::LlmError),

    /// [`crate::log::SessionEventLog::open_exclusive`] found another process already
    /// holding the session's advisory write lock.
    #[error("session event log at {0} is already locked by another process")]
    AlreadyLocked(String),
}
