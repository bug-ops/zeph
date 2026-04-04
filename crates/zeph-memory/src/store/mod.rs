// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

mod acp_sessions;
pub mod admission_training;
pub mod compression_guidelines;
pub mod compression_predictor;
pub mod corrections;
pub mod experiments;
pub mod graph_store;
mod history;
mod mem_scenes;
pub(crate) mod messages;
pub mod overflow;
pub mod preferences;
pub mod session_digest;
mod skills;
mod summaries;
mod trust;

#[allow(unused_imports)]
use zeph_db::sql;
use zeph_db::{DbConfig, DbPool};

use crate::error::MemoryError;

pub use acp_sessions::{AcpSessionEvent, AcpSessionInfo};
pub use messages::role_str;
pub use skills::{SkillMetricsRow, SkillUsageRow, SkillVersionRow};
pub use trust::{SkillTrustRow, SourceKind};

/// Backward-compatible type alias. Prefer [`DbStore`] in new code.
pub type SqliteStore = DbStore;

#[derive(Debug, Clone)]
pub struct DbStore {
    pool: DbPool,
}

impl DbStore {
    /// Open (or create) the database and run migrations.
    ///
    /// # Errors
    ///
    /// Returns an error if the database cannot be opened or migrations fail.
    pub async fn new(path: &str) -> Result<Self, MemoryError> {
        Self::with_pool_size(path, 5).await
    }

    /// Open (or create) the database with a configurable connection pool size.
    ///
    /// # Errors
    ///
    /// Returns an error if the database cannot be opened or migrations fail.
    pub async fn with_pool_size(path: &str, pool_size: u32) -> Result<Self, MemoryError> {
        let pool = DbConfig {
            url: path.to_string(),
            max_connections: pool_size,
            pool_size,
        }
        .connect()
        .await?;

        Ok(Self { pool })
    }

    /// Expose the underlying pool for shared access by other stores.
    #[must_use]
    pub fn pool(&self) -> &DbPool {
        &self.pool
    }

    /// Run all migrations on the given pool.
    ///
    /// # Errors
    ///
    /// Returns an error if any migration fails.
    pub async fn run_migrations(pool: &DbPool) -> Result<(), MemoryError> {
        zeph_db::run_migrations(pool).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    // Matches `DbConfig` busy_timeout default (5 seconds in ms)
    const DEFAULT_BUSY_TIMEOUT_MS: i64 = 5_000;

    #[tokio::test]
    async fn wal_journal_mode_enabled_on_file_db() {
        let file = NamedTempFile::new().expect("tempfile");
        let path = file.path().to_str().expect("valid path");

        let store = DbStore::new(path).await.expect("DbStore::new");

        let mode: String = zeph_db::query_scalar(sql!("PRAGMA journal_mode"))
            .fetch_one(store.pool())
            .await
            .expect("PRAGMA query");

        assert_eq!(mode, "wal", "expected WAL journal mode, got: {mode}");
    }

    #[tokio::test]
    async fn busy_timeout_enabled_on_file_db() {
        let file = NamedTempFile::new().expect("tempfile");
        let path = file.path().to_str().expect("valid path");

        let store = DbStore::new(path).await.expect("DbStore::new");

        let timeout_ms: i64 = zeph_db::query_scalar(sql!("PRAGMA busy_timeout"))
            .fetch_one(store.pool())
            .await
            .expect("PRAGMA query");

        assert_eq!(
            timeout_ms, DEFAULT_BUSY_TIMEOUT_MS,
            "expected busy_timeout pragma to match configured timeout"
        );
    }

    #[tokio::test]
    async fn creates_parent_dirs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let deep = dir.path().join("a/b/c/zeph.db");
        let path = deep.to_str().expect("valid path");
        let _store = DbStore::new(path).await.expect("DbStore::new");
        assert!(deep.exists(), "database file should exist");
    }

    #[tokio::test]
    async fn with_pool_size_accepts_custom_size() {
        let store = DbStore::with_pool_size(":memory:", 2)
            .await
            .expect("with_pool_size");
        // Verify the store is operational with the custom pool size.
        let _cid = store
            .create_conversation()
            .await
            .expect("create_conversation");
    }
}
