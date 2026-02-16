use sqlx::SqlitePool;

use crate::error::SchedulerError;

pub struct JobStore {
    pool: SqlitePool,
}

impl JobStore {
    #[must_use]
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Initialize the `scheduled_jobs` table.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQL statement fails.
    pub async fn init(&self) -> Result<(), SchedulerError> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS scheduled_jobs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL UNIQUE,
                cron_expr TEXT NOT NULL,
                kind TEXT NOT NULL,
                last_run TEXT,
                next_run TEXT,
                status TEXT NOT NULL DEFAULT 'pending'
            )",
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Upsert a job definition.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQL statement fails.
    pub async fn upsert_job(
        &self,
        name: &str,
        cron_expr: &str,
        kind: &str,
    ) -> Result<(), SchedulerError> {
        sqlx::query(
            "INSERT INTO scheduled_jobs (name, cron_expr, kind)
             VALUES (?, ?, ?)
             ON CONFLICT(name) DO UPDATE SET cron_expr = excluded.cron_expr, kind = excluded.kind",
        )
        .bind(name)
        .bind(cron_expr)
        .bind(kind)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Record a job execution timestamp.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQL statement fails.
    pub async fn record_run(&self, name: &str, timestamp: &str) -> Result<(), SchedulerError> {
        sqlx::query("UPDATE scheduled_jobs SET last_run = ?, status = 'completed' WHERE name = ?")
            .bind(timestamp)
            .bind(name)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Get the last run timestamp for a job.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQL query fails.
    pub async fn last_run(&self, name: &str) -> Result<Option<String>, SchedulerError> {
        let row: Option<(Option<String>,)> =
            sqlx::query_as("SELECT last_run FROM scheduled_jobs WHERE name = ?")
                .bind(name)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.and_then(|r| r.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_pool() -> SqlitePool {
        SqlitePool::connect("sqlite::memory:").await.unwrap()
    }

    #[tokio::test]
    async fn init_creates_table() {
        let pool = test_pool().await;
        let store = JobStore::new(pool);
        assert!(store.init().await.is_ok());
    }

    #[tokio::test]
    async fn upsert_and_query() {
        let pool = test_pool().await;
        let store = JobStore::new(pool);
        store.init().await.unwrap();

        store
            .upsert_job("test_job", "0 * * * * *", "health_check")
            .await
            .unwrap();
        assert!(store.last_run("test_job").await.unwrap().is_none());

        store
            .record_run("test_job", "2026-01-01T00:00:00Z")
            .await
            .unwrap();
        assert_eq!(
            store.last_run("test_job").await.unwrap().as_deref(),
            Some("2026-01-01T00:00:00Z")
        );
    }

    #[tokio::test]
    async fn upsert_updates_existing() {
        let pool = test_pool().await;
        let store = JobStore::new(pool);
        store.init().await.unwrap();

        store
            .upsert_job("job1", "0 * * * * *", "health_check")
            .await
            .unwrap();
        store
            .upsert_job("job1", "0 0 * * * *", "memory_cleanup")
            .await
            .unwrap();

        let row: (String,) = sqlx::query_as("SELECT kind FROM scheduled_jobs WHERE name = 'job1'")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(row.0, "memory_cleanup");
    }

    #[tokio::test]
    async fn last_run_nonexistent_job() {
        let pool = test_pool().await;
        let store = JobStore::new(pool);
        store.init().await.unwrap();
        assert!(store.last_run("no_such_job").await.unwrap().is_none());
    }
}

impl JobStore {
    #[must_use]
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }
}
