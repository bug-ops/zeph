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
    #[error("{}", describe_already_locked(path, *pid, *pid_alive))]
    AlreadyLocked {
        /// Path to the contended lock file.
        path: String,
        /// PID of the process holding the lock, read back from the lock file's contents at
        /// contention time. `None` if the file was empty or its contents could not be parsed
        /// as a PID (e.g. the lock was acquired by a build predating PID recording).
        pid: Option<u32>,
        /// Whether `pid` was confirmed still running via a liveness check (`kill(pid, 0)`) at
        /// contention time. `None` if `pid` is `None`, or on non-Unix targets where no
        /// liveness check runs.
        pid_alive: Option<bool>,
    },

    /// A `UserMessage.image_refs` entry was not a bare hex string, so it was rejected before
    /// being joined into a filesystem path (#5982 follow-up, path-traversal guard).
    #[error("invalid blob hash (must be a non-empty hex string): {0:?}")]
    InvalidBlobHash(String),
}

/// Formats [`SessionError::AlreadyLocked`]'s message, distinguishing a recorded-not-running
/// holder (#6378: the flock conflict is real, but the codebase previously gave operators
/// nothing to verify it against) from a live or unknown one. The recorded-not-running case is
/// hedged, not asserted as definitely stale: the lock file is a permanent sentinel that is
/// truncated and rewritten (not atomically replaced) on each acquire, so a contender can observe
/// the *previous* holder's now-dead PID during the brief window between a new holder's
/// successful `flock` and its PID write — see the `WOULDBLOCK` branch in
/// [`AdvisoryLock::acquire`](crate::log::AdvisoryLock::acquire). Never suggests auto-recovery —
/// breaking a live flock without the holder's cooperation is unsafe, so this is diagnostic-only.
fn describe_already_locked(path: &str, pid: Option<u32>, pid_alive: Option<bool>) -> String {
    match (pid, pid_alive) {
        (Some(pid), Some(false)) => format!(
            "session event log at {path} is already locked: the recorded holder pid {pid} is \
             not running; the lock may be stale, or may have just been re-acquired by another \
             process — verify before removing it. Not auto-recovered because breaking a live \
             flock without holder cooperation is unsafe"
        ),
        (Some(pid), _) => format!(
            "session event log at {path} is already locked by another process (held by pid {pid})"
        ),
        (None, _) => format!(
            "session event log at {path} is already locked by another process (holder pid unknown)"
        ),
    }
}
