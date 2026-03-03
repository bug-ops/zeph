// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

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

    /// Open (or create) a `JobStore` from a `SQLite` file path.
    ///
    /// # Errors
    ///
    /// Returns `SchedulerError::Database` if the connection cannot be established.
    pub async fn open(path: &str) -> Result<Self, SchedulerError> {
        let pool = SqlitePool::connect(&format!("sqlite:{path}?mode=rwc")).await?;
        Ok(Self { pool })
    }

    /// Initialize the `scheduled_jobs` table with oneshot support columns.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQL statement fails.
    pub async fn init(&self) -> Result<(), SchedulerError> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS scheduled_jobs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL UNIQUE,
                cron_expr TEXT NOT NULL DEFAULT '',
                kind TEXT NOT NULL,
                last_run TEXT,
                next_run TEXT,
                status TEXT NOT NULL DEFAULT 'pending',
                task_mode TEXT NOT NULL DEFAULT 'periodic',
                run_at TEXT
            )",
        )
        .execute(&self.pool)
        .await?;
        // Add columns if upgrading from an older schema without them.
        let _ = sqlx::query(
            "ALTER TABLE scheduled_jobs ADD COLUMN task_mode TEXT NOT NULL DEFAULT 'periodic'",
        )
        .execute(&self.pool)
        .await;
        let _ = sqlx::query("ALTER TABLE scheduled_jobs ADD COLUMN run_at TEXT")
            .execute(&self.pool)
            .await;
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
        self.upsert_job_with_mode(name, cron_expr, kind, "periodic", None)
            .await
    }

    /// Upsert a job definition with explicit `task_mode` and optional `run_at`.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQL statement fails.
    pub async fn upsert_job_with_mode(
        &self,
        name: &str,
        cron_expr: &str,
        kind: &str,
        task_mode: &str,
        run_at: Option<&str>,
    ) -> Result<(), SchedulerError> {
        sqlx::query(
            "INSERT INTO scheduled_jobs (name, cron_expr, kind, task_mode, run_at)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(name) DO UPDATE SET
               cron_expr = excluded.cron_expr,
               kind = excluded.kind,
               task_mode = excluded.task_mode,
               run_at = excluded.run_at",
        )
        .bind(name)
        .bind(cron_expr)
        .bind(kind)
        .bind(task_mode)
        .bind(run_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Record a job execution and persist the next scheduled run time.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQL statement fails.
    pub async fn record_run(
        &self,
        name: &str,
        timestamp: &str,
        next_run: &str,
    ) -> Result<(), SchedulerError> {
        sqlx::query(
            "UPDATE scheduled_jobs SET last_run = ?, next_run = ?, status = 'completed' WHERE name = ?",
        )
        .bind(timestamp)
        .bind(next_run)
        .bind(name)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Mark a one-shot job as done.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQL statement fails.
    pub async fn mark_done(&self, name: &str) -> Result<(), SchedulerError> {
        sqlx::query(
            "UPDATE scheduled_jobs SET status = 'done', last_run = datetime('now') WHERE name = ?",
        )
        .bind(name)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Delete a job by name.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQL statement fails.
    pub async fn delete_job(&self, name: &str) -> Result<bool, SchedulerError> {
        let result = sqlx::query("DELETE FROM scheduled_jobs WHERE name = ?")
            .bind(name)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Check if a job with the given name exists.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQL query fails.
    pub async fn job_exists(&self, name: &str) -> Result<bool, SchedulerError> {
        let row: Option<(i64,)> = sqlx::query_as("SELECT 1 FROM scheduled_jobs WHERE name = ?")
            .bind(name)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.is_some())
    }

    /// Persist the next scheduled run time for a job (used during init).
    ///
    /// # Errors
    ///
    /// Returns an error if the SQL statement fails.
    pub async fn set_next_run(&self, name: &str, next_run: &str) -> Result<(), SchedulerError> {
        sqlx::query("UPDATE scheduled_jobs SET next_run = ? WHERE name = ?")
            .bind(next_run)
            .bind(name)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Get the persisted next run timestamp for a job.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQL query fails.
    pub async fn get_next_run(&self, name: &str) -> Result<Option<String>, SchedulerError> {
        let row: Option<(Option<String>,)> =
            sqlx::query_as("SELECT next_run FROM scheduled_jobs WHERE name = ?")
                .bind(name)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.and_then(|r| r.0))
    }

    /// List all active (non-done) jobs. Returns `(name, kind, task_mode, next_run)` tuples.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQL query fails.
    pub async fn list_jobs(&self) -> Result<Vec<(String, String, String, String)>, SchedulerError> {
        let rows: Vec<(String, String, String, Option<String>)> = sqlx::query_as(
            "SELECT name, kind, task_mode, next_run FROM scheduled_jobs WHERE status != 'done' ORDER BY name",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(name, kind, mode, next_run)| (name, kind, mode, next_run.unwrap_or_default()))
            .collect())
    }

    #[must_use]
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
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
        assert!(store.get_next_run("test_job").await.unwrap().is_none());

        store
            .record_run("test_job", "2026-01-01T00:00:00Z", "2026-01-01T00:01:00Z")
            .await
            .unwrap();
        assert_eq!(
            store.get_next_run("test_job").await.unwrap().as_deref(),
            Some("2026-01-01T00:01:00Z")
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
    async fn next_run_nonexistent_job() {
        let pool = test_pool().await;
        let store = JobStore::new(pool);
        store.init().await.unwrap();
        assert!(store.get_next_run("no_such_job").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn job_exists_returns_true_for_existing() {
        let pool = test_pool().await;
        let store = JobStore::new(pool);
        store.init().await.unwrap();
        store
            .upsert_job("exists_job", "0 * * * * *", "health_check")
            .await
            .unwrap();
        assert!(store.job_exists("exists_job").await.unwrap());
        assert!(!store.job_exists("missing").await.unwrap());
    }

    #[tokio::test]
    async fn delete_job_removes_row() {
        let pool = test_pool().await;
        let store = JobStore::new(pool);
        store.init().await.unwrap();
        store
            .upsert_job("del_job", "0 * * * * *", "health_check")
            .await
            .unwrap();
        assert!(store.delete_job("del_job").await.unwrap());
        assert!(!store.job_exists("del_job").await.unwrap());
        assert!(!store.delete_job("del_job").await.unwrap());
    }

    #[tokio::test]
    async fn mark_done_sets_status() {
        let pool = test_pool().await;
        let store = JobStore::new(pool);
        store.init().await.unwrap();
        store
            .upsert_job_with_mode(
                "os_job",
                "",
                "health_check",
                "oneshot",
                Some("2026-01-01T01:00:00Z"),
            )
            .await
            .unwrap();
        store.mark_done("os_job").await.unwrap();
        let row: (String,) =
            sqlx::query_as("SELECT status FROM scheduled_jobs WHERE name = 'os_job'")
                .fetch_one(store.pool())
                .await
                .unwrap();
        assert_eq!(row.0, "done");
    }

    #[tokio::test]
    async fn duplicate_name_detected() {
        let pool = test_pool().await;
        let store = JobStore::new(pool);
        store.init().await.unwrap();
        store
            .upsert_job("dup", "0 * * * * *", "health_check")
            .await
            .unwrap();
        assert!(store.job_exists("dup").await.unwrap());
    }
}
