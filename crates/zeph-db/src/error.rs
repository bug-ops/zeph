// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use thiserror::Error;

/// Unified database error type for `zeph-db`.
#[derive(Debug, Error)]
pub enum DbError {
    /// Connection failed. The URL stored here is always credential-redacted.
    #[error("database connection failed (url: {url}): {source}")]
    Connection {
        url: String,
        #[source]
        source: sqlx::Error,
    },

    /// Migration failed.
    #[error("database migration failed: {0}")]
    Migration(#[from] sqlx::migrate::MigrateError),

    /// I/O error (e.g., creating parent directories for `SQLite` file).
    #[error("database I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Generic sqlx error not covered by the above variants.
    #[error("database error: {0}")]
    Sqlx(#[from] sqlx::Error),
}
