// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

mod acp_sessions;
pub mod compression_guidelines;
pub mod corrections;
#[cfg(feature = "experiments")]
pub mod experiments;
pub mod graph_store;
mod history;
pub(crate) mod messages;
pub mod overflow;
pub mod preferences;
pub mod session_digest;
mod skills;
mod summaries;
mod trust;

use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::str::FromStr;
use std::time::Duration;

use crate::error::MemoryError;

pub use acp_sessions::{AcpSessionEvent, AcpSessionInfo};
pub use messages::role_str;
pub use skills::{SkillMetricsRow, SkillUsageRow, SkillVersionRow};
pub use trust::{SkillTrustRow, SourceKind};

#[derive(Debug, Clone)]
pub struct SqliteStore {
    pool: SqlitePool,
}

impl SqliteStore {
    const DEFAULT_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

    /// Open (or create) the `SQLite` database and run migrations.
    ///
    /// Enables foreign key constraints at connection level so that
    /// `ON DELETE CASCADE` and other FK rules are enforced.
    ///
    /// # Errors
    ///
    /// Returns an error if the database cannot be opened or migrations fail.
    pub async fn new(path: &str) -> Result<Self, MemoryError> {
        Self::with_pool_size(path, 5).await
    }

    /// Open (or create) the `SQLite` database with a configurable connection pool size.
    ///
    /// # Errors
    ///
    /// Returns an error if the database cannot be opened or migrations fail.
    pub async fn with_pool_size(path: &str, pool_size: u32) -> Result<Self, MemoryError> {
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

        let opts = SqliteConnectOptions::from_str(&url)?
            .create_if_missing(true)
            .foreign_keys(true)
            .busy_timeout(Self::DEFAULT_BUSY_TIMEOUT)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .synchronous(sqlx::sqlite::SqliteSynchronous::Normal);

        let pool = SqlitePoolOptions::new()
            .max_connections(pool_size)
            .connect_with(opts)
            .await?;

        sqlx::migrate!("./migrations").run(&pool).await?;

        if path != ":memory:" {
            sqlx::query("PRAGMA wal_checkpoint(PASSIVE)")
                .execute(&pool)
                .await?;
        }

        Ok(Self { pool })
    }

    /// Expose the underlying pool for shared access by other stores.
    #[must_use]
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Run all migrations on the given pool.
    ///
    /// # Errors
    ///
    /// Returns an error if any migration fails.
    pub async fn run_migrations(pool: &SqlitePool) -> Result<(), MemoryError> {
        sqlx::migrate!("./migrations").run(pool).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[tokio::test]
    async fn wal_journal_mode_enabled_on_file_db() {
        let file = NamedTempFile::new().expect("tempfile");
        let path = file.path().to_str().expect("valid path");

        let store = SqliteStore::new(path).await.expect("SqliteStore::new");

        let mode: String = sqlx::query_scalar("PRAGMA journal_mode")
            .fetch_one(store.pool())
            .await
            .expect("PRAGMA query");

        assert_eq!(mode, "wal", "expected WAL journal mode, got: {mode}");
    }

    #[tokio::test]
    async fn busy_timeout_enabled_on_file_db() {
        let file = NamedTempFile::new().expect("tempfile");
        let path = file.path().to_str().expect("valid path");

        let store = SqliteStore::new(path).await.expect("SqliteStore::new");

        let timeout_ms: i64 = sqlx::query_scalar("PRAGMA busy_timeout")
            .fetch_one(store.pool())
            .await
            .expect("PRAGMA query");

        assert_eq!(
            timeout_ms,
            i64::try_from(SqliteStore::DEFAULT_BUSY_TIMEOUT.as_millis()).unwrap(),
            "expected busy_timeout pragma to match configured timeout"
        );
    }

    #[tokio::test]
    async fn creates_parent_dirs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let deep = dir.path().join("a/b/c/zeph.db");
        let path = deep.to_str().expect("valid path");
        let _store = SqliteStore::new(path).await.expect("SqliteStore::new");
        assert!(deep.exists(), "database file should exist");
    }

    #[tokio::test]
    async fn with_pool_size_accepts_custom_size() {
        let store = SqliteStore::with_pool_size(":memory:", 2)
            .await
            .expect("with_pool_size");
        // Verify the store is operational with the custom pool size.
        let _cid = store
            .create_conversation()
            .await
            .expect("create_conversation");
    }
}
