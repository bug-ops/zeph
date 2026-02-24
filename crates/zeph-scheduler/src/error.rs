// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use thiserror::Error;

#[derive(Debug, Error)]
pub enum SchedulerError {
    #[error("invalid cron expression: {0}")]
    InvalidCron(String),
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("task execution failed: {0}")]
    TaskFailed(String),
}
