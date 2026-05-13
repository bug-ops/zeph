// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use zeph_db::DbPool;
#[allow(unused_imports)]
use zeph_db::sql;

use crate::error::SchedulerError;

/// A scheduled task row returned by [`JobStore::list_jobs`].
///
/// Replaces the previous `(String, String, String, String)` tuple to eliminate
/// positional destructuring bugs. Fields map 1-to-1 to the SQL columns in the
/// same order as the query: `name`, `kind`, `task_mode`, and the coalesced
/// `next_run`.
///
/// # Examples
///
/// ```rust,no_run
/// use zeph_scheduler::JobStore;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let store = JobStore::open("sqlite:scheduler.db").await?;
/// store.init().await?;
///
/// for job in store.list_jobs().await? {
///     println!("{}: {} ({}) → {}", job.name, job.kind, job.task_mode, job.next_run);
/// }
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct ScheduledTaskRecord {
    /// Unique task name (primary key in the `scheduled_jobs` table).
    pub name: String,
    /// Serialised [`crate::TaskKind`] string (e.g. `"health_check"`).
    pub kind: String,
    /// Execution mode: `"periodic"` or `"oneshot"`.
    pub task_mode: String,
    /// Next scheduled run time as an ISO 8601 / RFC 3339 string.
    ///
    /// Falls back to `run_at` for one-shot jobs that have not yet computed a
    /// `next_run`. Empty string when neither field is set.
    pub next_run: String,
}

/// Full details for a scheduled task, returned by [`JobStore::list_jobs_full`].
///
/// Intended for display in the TUI or CLI task list. All string fields are UTF-8
/// and come directly from the `scheduled_jobs` `SQLite` table.
#[derive(Debug, Clone)]
pub struct ScheduledTaskInfo {
    /// Unique task name (primary key in the `scheduled_jobs` table).
    pub name: String,
    /// Serialised [`crate::TaskKind`] string (e.g. `"health_check"`).
    pub kind: String,
    /// Execution mode: `"periodic"` or `"oneshot"`.
    pub task_mode: String,
    /// Cron expression for periodic tasks, empty string for one-shot tasks.
    pub cron_expr: String,
    /// Next scheduled run time as an ISO 8601 / RFC 3339 string, or empty if unknown.
    pub next_run: String,
    /// Stored task prompt for custom tasks; empty for config-driven built-in tasks.
    pub task_data: String,
    /// Current job status: `"pending"`, `"completed"`, `"done"`, or `"error"`.
    pub status: String,
}

/// Persistent storage layer for scheduled jobs.
///
/// All job definitions and run history are stored in a `SQLite` database managed by
/// `zeph-db` migrations. The `scheduled_jobs` table schema is defined in migration
/// `051_scheduler_jobs.sql`.
///
/// # Examples
///
/// ```rust,no_run
/// use zeph_scheduler::JobStore;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// // Open from a file path.
/// let store = JobStore::open("sqlite:scheduler.db").await?;
/// store.init().await?;
///
/// // Query job list.
/// let jobs = store.list_jobs().await?;
/// for job in &jobs {
///     println!("{}: {} ({}) → {}", job.name, job.kind, job.task_mode, job.next_run);
/// }
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct JobStore {
    pool: DbPool,
}

impl JobStore {
    /// Create a `JobStore` from an existing [`zeph_db::DbPool`].
    ///
    /// You must call [`JobStore::init`] before any other operation to ensure the
    /// schema migrations have been applied.
    #[must_use]
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    /// Open (or create) a `JobStore` from a `SQLite` file path.
    ///
    /// # Errors
    ///
    /// Returns [`SchedulerError::Db`] if the connection cannot be established.
    pub async fn open(path: &str) -> Result<Self, SchedulerError> {
        let pool = zeph_db::DbConfig {
            url: path.to_string(),
            max_connections: 5,
            pool_size: 5,
        }
        .connect()
        .await?;
        Ok(Self { pool })
    }

    /// Run all pending migrations on the underlying pool.
    ///
    /// Replaces the former inline `CREATE TABLE IF NOT EXISTS` DDL. The
    /// `scheduled_jobs` schema is now managed by migration
    /// `051_scheduler_jobs.sql` in `zeph-db`.
    ///
    /// # Errors
    ///
    /// Returns an error if any migration fails.
    pub async fn init(&self) -> Result<(), SchedulerError> {
        zeph_db::run_migrations(&self.pool).await?;
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
        self.upsert_job_with_mode(name, cron_expr, kind, "periodic", None, "")
            .await
    }

    /// Upsert a job definition with explicit `task_mode`, optional `run_at`, and `task_data`.
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
        task_data: &str,
    ) -> Result<(), SchedulerError> {
        zeph_db::query(sql!(
            "INSERT INTO scheduled_jobs (name, cron_expr, kind, task_mode, run_at, task_data)
             VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT(name) DO UPDATE SET
               cron_expr = excluded.cron_expr,
               kind = excluded.kind,
               task_mode = excluded.task_mode,
               run_at = excluded.run_at,
               task_data = excluded.task_data"
        ))
        .bind(name)
        .bind(cron_expr)
        .bind(kind)
        .bind(task_mode)
        .bind(run_at)
        .bind(task_data)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Insert a new job. Returns [`SchedulerError::DuplicateJob`] if a job with this name exists.
    ///
    /// # Errors
    ///
    /// Returns [`SchedulerError::DuplicateJob`] on unique constraint violation,
    /// or [`SchedulerError::Database`] on other SQL errors.
    pub async fn insert_job(
        &self,
        name: &str,
        cron_expr: &str,
        kind: &str,
        task_mode: &str,
        run_at: Option<&str>,
        task_data: &str,
    ) -> Result<(), SchedulerError> {
        let result = zeph_db::query(sql!(
            "INSERT INTO scheduled_jobs (name, cron_expr, kind, task_mode, run_at, task_data)
             VALUES (?, ?, ?, ?, ?, ?)"
        ))
        .bind(name)
        .bind(cron_expr)
        .bind(kind)
        .bind(task_mode)
        .bind(run_at)
        .bind(task_data)
        .execute(&self.pool)
        .await;
        match result {
            Ok(_) => Ok(()),
            Err(zeph_db::SqlxError::Database(db_err))
                if db_err.message().contains("UNIQUE constraint failed")
                    || db_err.code().as_deref() == Some("23505") =>
            {
                Err(SchedulerError::DuplicateJob(name.to_string()))
            }
            Err(e) => Err(SchedulerError::Database(e)),
        }
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
        zeph_db::query(
            sql!("UPDATE scheduled_jobs SET last_run = ?, next_run = ?, status = 'completed' WHERE name = ?"),
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
        zeph_db::query(sql!(
            "UPDATE scheduled_jobs SET status = 'done', last_run = CURRENT_TIMESTAMP WHERE name = ?"
        ))
        .bind(name)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Mark a job as permanently errored (e.g. invalid cron expression on hydration).
    ///
    /// The job remains visible in [`JobStore::list_jobs_full`] with `status = "error"` so
    /// operators can identify it via `zeph scheduler list` without reading debug logs.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQL statement fails.
    pub async fn mark_error(&self, name: &str) -> Result<(), SchedulerError> {
        zeph_db::query(sql!(
            "UPDATE scheduled_jobs SET status = 'error' WHERE name = ?"
        ))
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
        let result = zeph_db::query(sql!("DELETE FROM scheduled_jobs WHERE name = ?"))
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
        let row: Option<(i64,)> =
            zeph_db::query_as(sql!("SELECT 1 FROM scheduled_jobs WHERE name = ?"))
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
        zeph_db::query(sql!(
            "UPDATE scheduled_jobs SET next_run = ? WHERE name = ?"
        ))
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
            zeph_db::query_as(sql!("SELECT next_run FROM scheduled_jobs WHERE name = ?"))
                .bind(name)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.and_then(|r| r.0))
    }

    /// List all active (non-done) jobs.
    ///
    /// Returns a [`ScheduledTaskRecord`] per active job, ordered by name.
    /// One-shot jobs without a computed `next_run` fall back to their `run_at` value;
    /// if neither is set the field is an empty string.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQL query fails.
    pub async fn list_jobs(&self) -> Result<Vec<ScheduledTaskRecord>, SchedulerError> {
        let rows: Vec<(String, String, String, Option<String>)> = zeph_db::query_as(
            sql!("SELECT name, kind, task_mode, COALESCE(next_run, run_at) FROM scheduled_jobs WHERE status != 'done' ORDER BY name"),
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(name, kind, task_mode, next_run)| ScheduledTaskRecord {
                name,
                kind,
                task_mode,
                next_run: next_run.unwrap_or_default(),
            })
            .collect())
    }

    /// List all active (non-done) jobs with full details for display.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQL query fails.
    pub async fn list_jobs_full(&self) -> Result<Vec<ScheduledTaskInfo>, SchedulerError> {
        #[allow(clippy::type_complexity)]
        let rows: Vec<(String, String, String, String, Option<String>, String, String)> =
            zeph_db::query_as(sql!(
                "SELECT name, kind, task_mode, cron_expr, COALESCE(next_run, run_at), task_data, status \
                 FROM scheduled_jobs WHERE status != 'done' ORDER BY name"
            ))
            .fetch_all(&self.pool)
            .await?;
        Ok(rows
            .into_iter()
            .map(
                |(name, kind, task_mode, cron_expr, next_run, task_data, status)| {
                    ScheduledTaskInfo {
                        name,
                        kind,
                        task_mode,
                        cron_expr,
                        next_run: next_run.unwrap_or_default(),
                        task_data,
                        status,
                    }
                },
            )
            .collect())
    }

    /// Returns a reference to the underlying connection pool.
    ///
    /// Primarily used in tests that need to execute raw SQL against the same database.
    #[must_use]
    pub fn pool(&self) -> &DbPool {
        &self.pool
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_pool() -> DbPool {
        zeph_db::DbConfig {
            url: ":memory:".to_string(),
            max_connections: 5,
            pool_size: 5,
        }
        .connect()
        .await
        .unwrap()
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

        let row: (String,) =
            zeph_db::query_as(sql!("SELECT kind FROM scheduled_jobs WHERE name = 'job1'"))
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
                "",
            )
            .await
            .unwrap();
        store.mark_done("os_job").await.unwrap();
        let row: (String,) = zeph_db::query_as(sql!(
            "SELECT status FROM scheduled_jobs WHERE name = 'os_job'"
        ))
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert_eq!(row.0, "done");
    }

    #[tokio::test]
    async fn list_jobs_excludes_done_jobs() {
        let pool = test_pool().await;
        let store = JobStore::new(pool);
        store.init().await.unwrap();
        store
            .upsert_job_with_mode(
                "done_job",
                "",
                "health_check",
                "oneshot",
                Some("2026-01-01T01:00:00Z"),
                "",
            )
            .await
            .unwrap();
        store.mark_done("done_job").await.unwrap();
        let jobs = store.list_jobs().await.unwrap();
        assert!(
            jobs.iter().all(|j| j.name != "done_job"),
            "list_jobs must not return done jobs"
        );
    }

    #[tokio::test]
    async fn list_jobs_uses_run_at_for_oneshot_when_next_run_is_null() {
        let pool = test_pool().await;
        let store = JobStore::new(pool);
        store.init().await.unwrap();
        store
            .upsert_job_with_mode(
                "oneshot_job",
                "",
                "custom",
                "oneshot",
                Some("2026-06-01T10:00:00Z"),
                "",
            )
            .await
            .unwrap();
        let jobs = store.list_jobs().await.unwrap();
        let job = jobs.iter().find(|j| j.name == "oneshot_job").unwrap();
        assert_eq!(
            job.next_run, "2026-06-01T10:00:00Z",
            "run_at must be shown as next_run for oneshot jobs"
        );
    }

    #[tokio::test]
    async fn list_jobs_full_returns_correct_fields() {
        let pool = test_pool().await;
        let store = JobStore::new(pool);
        store.init().await.unwrap();
        store
            .upsert_job("periodic_job", "0 0 3 * * *", "memory_cleanup")
            .await
            .unwrap();
        store
            .upsert_job_with_mode(
                "oneshot_job",
                "",
                "custom",
                "oneshot",
                Some("2030-01-01T10:00:00Z"),
                "",
            )
            .await
            .unwrap();

        let jobs = store.list_jobs_full().await.unwrap();
        assert_eq!(jobs.len(), 2);

        let periodic = jobs.iter().find(|j| j.name == "periodic_job").unwrap();
        assert_eq!(periodic.kind, "memory_cleanup");
        assert_eq!(periodic.task_mode, "periodic");
        assert_eq!(periodic.cron_expr, "0 0 3 * * *");

        let oneshot = jobs.iter().find(|j| j.name == "oneshot_job").unwrap();
        assert_eq!(oneshot.kind, "custom");
        assert_eq!(oneshot.task_mode, "oneshot");
        assert!(oneshot.cron_expr.is_empty());
        assert_eq!(oneshot.next_run, "2030-01-01T10:00:00Z");
    }

    #[tokio::test]
    async fn list_jobs_full_excludes_done_jobs() {
        let pool = test_pool().await;
        let store = JobStore::new(pool);
        store.init().await.unwrap();
        store
            .upsert_job_with_mode(
                "done_job",
                "",
                "custom",
                "oneshot",
                Some("2026-01-01T01:00:00Z"),
                "",
            )
            .await
            .unwrap();
        store.mark_done("done_job").await.unwrap();
        let jobs = store.list_jobs_full().await.unwrap();
        assert!(jobs.iter().all(|j| j.name != "done_job"));
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

    #[tokio::test]
    async fn insert_job_success() {
        let pool = test_pool().await;
        let store = JobStore::new(pool);
        store.init().await.unwrap();
        store
            .insert_job(
                "new_job",
                "0 * * * * *",
                "custom",
                "periodic",
                None,
                "run daily report",
            )
            .await
            .unwrap();
        assert!(store.job_exists("new_job").await.unwrap());
    }

    #[tokio::test]
    async fn insert_job_duplicate_returns_error() {
        let pool = test_pool().await;
        let store = JobStore::new(pool);
        store.init().await.unwrap();
        store
            .insert_job(
                "dup_job",
                "0 * * * * *",
                "custom",
                "periodic",
                None,
                "first",
            )
            .await
            .unwrap();
        let result = store
            .insert_job(
                "dup_job",
                "0 0 * * * *",
                "custom",
                "periodic",
                None,
                "second",
            )
            .await;
        assert!(
            matches!(result, Err(SchedulerError::DuplicateJob(ref n)) if n == "dup_job"),
            "expected DuplicateJob, got {result:?}"
        );
    }

    #[tokio::test]
    async fn list_jobs_full_includes_task_data() {
        let pool = test_pool().await;
        let store = JobStore::new(pool);
        store.init().await.unwrap();
        store
            .insert_job(
                "task_job",
                "0 * * * * *",
                "custom",
                "periodic",
                None,
                "my prompt",
            )
            .await
            .unwrap();
        let jobs = store.list_jobs_full().await.unwrap();
        let job = jobs.iter().find(|j| j.name == "task_job").unwrap();
        assert_eq!(job.task_data, "my prompt");
    }
}
