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
}
