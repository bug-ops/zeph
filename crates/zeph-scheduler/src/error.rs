// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use thiserror::Error;

/// Errors that can occur inside the scheduler subsystem.
#[derive(Debug, Error)]
pub enum SchedulerError {
    /// The provided cron expression could not be parsed.
    ///
    /// The inner string contains the original expression and the parser's error message.
    #[error("invalid cron expression: {0}")]
    InvalidCron(String),

    /// A low-level `SQLx` error occurred during a database operation.
    #[error("database error: {0}")]
    Database(#[from] zeph_db::SqlxError),

    /// A high-level `zeph-db` error occurred (e.g. during migrations or connection setup).
    #[error("database error: {0}")]
    Db(#[from] zeph_db::DbError),

    /// The [`crate::TaskHandler`] returned an error during task execution.
    ///
    /// The inner string is the human-readable description from the handler.
    #[error("task execution failed: {0}")]
    TaskFailed(String),

    /// A job with the given name already exists in the store.
    ///
    /// Returned by [`crate::JobStore::insert_job`] on a UNIQUE constraint violation.
    #[error("job '{0}' already exists")]
    DuplicateJob(String),

    /// Another `zeph serve` instance is already running with the given PID.
    ///
    /// Returned by [`crate::PidFile::acquire`] when the pid file is locked by another process.
    #[cfg(unix)]
    #[error(
        "daemon pid file is locked: another zeph serve instance appears to be running (pid {pid})"
    )]
    AlreadyRunning {
        /// PID of the running daemon, as stored in the pid file.
        pid: u32,
    },

    /// Failed to detach the daemon process (fork, exec, or I/O redirection error).
    #[cfg(unix)]
    #[error("daemon detach failed: {0}")]
    Detach(String),

    /// A generic I/O error from daemon lifecycle operations (pid file, log file).
    #[cfg(unix)]
    #[error("daemon I/O error: {0}")]
    Io(String),

    /// A task prompt matched an injection pattern and was blocked by the RTW-A defense.
    ///
    /// The task is skipped for this tick and logged at `WARN` level. No prompt is
    /// forwarded to the agent loop.
    #[error("prompt injection blocked in task '{task_name}': {reason}")]
    PromptInjectionBlocked {
        /// Name of the task whose prompt was blocked.
        task_name: String,
        /// Description of the pattern that triggered the block.
        reason: String,
    },

    /// A task was quarantined by the RTW-A write-fence and skipped for this tick.
    ///
    /// Tasks written to the store in the same tick they would execute are held back
    /// for one tick to prevent write-before-exposed-read re-entry attacks.
    #[error("task '{task_name}' quarantined by write-fence (written this tick)")]
    TaskQuarantined {
        /// Name of the task that was quarantined.
        task_name: String,
    },
}
