mod history;
mod messages;
mod skills;
mod summaries;
mod trust;

use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use std::str::FromStr;

use crate::error::MemoryError;

pub use messages::role_str;
pub use skills::{SkillMetricsRow, SkillUsageRow, SkillVersionRow};
pub use trust::SkillTrustRow;

#[derive(Debug, Clone)]
pub struct SqliteStore {
    pool: SqlitePool,
}

impl SqliteStore {
    /// Open (or create) the `SQLite` database and run migrations.
    ///
    /// Enables foreign key constraints at connection level so that
    /// `ON DELETE CASCADE` and other FK rules are enforced.
    ///
    /// # Errors
    ///
    /// Returns an error if the database cannot be opened or migrations fail.
    pub async fn new(path: &str) -> Result<Self, MemoryError> {
        let url = if path == ":memory:" {
            "sqlite::memory:".to_string()
        } else {
            format!("sqlite:{path}?mode=rwc")
        };

        let opts = SqliteConnectOptions::from_str(&url)?
            .create_if_missing(true)
            .foreign_keys(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .synchronous(sqlx::sqlite::SqliteSynchronous::Normal);

        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(opts)
            .await?;

        sqlx::migrate!("../../migrations").run(&pool).await?;

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
        sqlx::migrate!("../../migrations").run(pool).await?;
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
}
