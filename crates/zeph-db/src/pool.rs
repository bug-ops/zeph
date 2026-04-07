// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::DbPool;
use crate::error::DbError;

/// Configuration for database pool construction.
pub struct DbConfig {
    /// Database URL. `SQLite`: file path or `:memory:`. `PostgreSQL`: connection URL.
    pub url: String,
    /// Maximum number of connections in the pool.
    pub max_connections: u32,
    /// `SQLite` only: connection pool size. Default 5.
    ///
    /// `BEGIN IMMEDIATE` serializes concurrent writers at the `SQLite` level;
    /// the pool size controls read concurrency only.
    pub pool_size: u32,
}

impl Default for DbConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            max_connections: 5,
            pool_size: 5,
        }
    }
}

impl DbConfig {
    /// Connect to the database and run migrations.
    ///
    /// # Errors
    ///
    /// Returns [`DbError`] if connection or migration fails.
    pub async fn connect(&self) -> Result<DbPool, DbError> {
        #[cfg(all(feature = "sqlite", not(feature = "postgres")))]
        {
            Self::connect_sqlite(&self.url, self.max_connections, self.pool_size).await
        }
        #[cfg(feature = "postgres")]
        {
            Self::connect_postgres(&self.url, self.pool_size).await
        }
    }

    #[cfg(all(feature = "sqlite", not(feature = "postgres")))]
    async fn connect_sqlite(
        path: &str,
        max_connections: u32,
        pool_size: u32,
    ) -> Result<DbPool, DbError> {
        use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
        use std::str::FromStr;

        let url = if path == ":memory:" {
            "sqlite::memory:".to_string()
        } else {
            if let Some(parent) = std::path::Path::new(path).parent()
                && !parent.as_os_str().is_empty()
            {
                std::fs::create_dir_all(parent)?;
            }
            format!("sqlite:{path}?mode=rwc")
        };

        let opts = SqliteConnectOptions::from_str(&url)
            .map_err(DbError::Sqlx)?
            .create_if_missing(true)
            .foreign_keys(true)
            .busy_timeout(std::time::Duration::from_secs(5))
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .synchronous(sqlx::sqlite::SqliteSynchronous::Normal);

        // BEGIN IMMEDIATE serializes concurrent writers at the SQLite level.
        // pool_size controls the connection count; max_connections is the upper bound.
        // In-memory databases are connection-scoped: each new connection is a separate
        // empty DB. Force a single connection so all queries share the migrated schema.
        let effective_max = if path == ":memory:" {
            1
        } else {
            max_connections.max(pool_size)
        };
        let pool = SqlitePoolOptions::new()
            .max_connections(effective_max)
            .min_connections(1)
            .acquire_timeout(std::time::Duration::from_secs(30))
            .connect_with(opts)
            .await
            .map_err(DbError::Sqlx)?;

        crate::migrate::run_migrations(&pool).await?;

        // Restrict file permissions to owner-only on Unix.
        #[cfg(unix)]
        if path != ":memory:" {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(metadata) = std::fs::metadata(path) {
                let mut perms = metadata.permissions();
                perms.set_mode(0o600);
                let _ = std::fs::set_permissions(path, perms);
            }
        }

        // Run a passive WAL checkpoint after migrations to avoid unbounded WAL growth.
        // Skipped for in-memory databases (no WAL file).
        if path != ":memory:" {
            sqlx::query("PRAGMA wal_checkpoint(PASSIVE)")
                .execute(&pool)
                .await
                .map_err(DbError::Sqlx)?;
        }

        Ok(pool)
    }

    #[cfg(feature = "postgres")]
    async fn connect_postgres(url: &str, pool_size: u32) -> Result<DbPool, DbError> {
        use sqlx::postgres::PgPoolOptions;

        if !url.contains("sslmode=") {
            tracing::warn!(
                "postgres connection string has no sslmode; plaintext connections are allowed"
            );
        }

        let pool = PgPoolOptions::new()
            .max_connections(pool_size)
            .acquire_timeout(std::time::Duration::from_secs(30))
            .connect(url)
            .await
            .map_err(|e| DbError::Connection {
                url: redact_url(url).unwrap_or_else(|| "[redacted]".into()),
                source: e,
            })?;

        crate::migrate::run_migrations(&pool).await?;

        Ok(pool)
    }
}

/// Strip password from a database URL for safe logging.
///
/// Replaces `://user:password@` with `://[redacted]@`.
///
/// Returns `None` if the URL contains no embedded credentials (already safe).
/// Returns `Some(redacted)` if credentials were found and replaced.
#[must_use]
pub fn redact_url(url: &str) -> Option<String> {
    use std::sync::LazyLock;
    static RE: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"://[^:]+:[^@]+@").expect("static regex"));
    if RE.is_match(url) {
        Some(RE.replace(url, "://[redacted]@").into_owned())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_url_replaces_credentials() {
        let url = "postgres://user:secret@localhost:5432/zeph";
        let redacted = redact_url(url).unwrap();
        assert_eq!(redacted, "postgres://[redacted]@localhost:5432/zeph");
        assert!(!redacted.contains("secret"));
    }

    #[test]
    fn redact_url_returns_none_for_no_credentials() {
        // URL without credentials — no match, returns None
        let url = "postgres://localhost:5432/zeph";
        assert!(redact_url(url).is_none());
    }

    #[test]
    fn redact_url_handles_sqlite_path() {
        let url = "sqlite:/path/to/db";
        assert!(redact_url(url).is_none());
    }
}
