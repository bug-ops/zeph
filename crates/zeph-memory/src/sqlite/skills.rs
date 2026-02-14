use super::SqliteStore;
use crate::error::MemoryError;

#[derive(Debug)]
pub struct SkillUsageRow {
    pub skill_name: String,
    pub invocation_count: i64,
    pub last_used_at: String,
}

#[derive(Debug)]
pub struct SkillMetricsRow {
    pub skill_name: String,
    pub version_id: Option<i64>,
    pub total: i64,
    pub successes: i64,
    pub failures: i64,
}

#[derive(Debug)]
pub struct SkillVersionRow {
    pub id: i64,
    pub skill_name: String,
    pub version: i64,
    pub body: String,
    pub description: String,
    pub source: String,
    pub is_active: bool,
    pub success_count: i64,
    pub failure_count: i64,
    pub created_at: String,
}

type SkillVersionTuple = (
    i64,
    String,
    i64,
    String,
    String,
    String,
    i64,
    i64,
    i64,
    String,
);

fn skill_version_from_tuple(t: SkillVersionTuple) -> SkillVersionRow {
    SkillVersionRow {
        id: t.0,
        skill_name: t.1,
        version: t.2,
        body: t.3,
        description: t.4,
        source: t.5,
        is_active: t.6 != 0,
        success_count: t.7,
        failure_count: t.8,
        created_at: t.9,
    }
}

impl SqliteStore {
    /// Record usage of skills (UPSERT: increment count and update timestamp).
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    pub async fn record_skill_usage(&self, skill_names: &[&str]) -> Result<(), MemoryError> {
        for name in skill_names {
            sqlx::query(
                "INSERT INTO skill_usage (skill_name, invocation_count, last_used_at) \
                 VALUES (?, 1, datetime('now')) \
                 ON CONFLICT(skill_name) DO UPDATE SET \
                 invocation_count = invocation_count + 1, \
                 last_used_at = datetime('now')",
            )
            .bind(name)
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }

    /// Load all skill usage statistics.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn load_skill_usage(&self) -> Result<Vec<SkillUsageRow>, MemoryError> {
        let rows: Vec<(String, i64, String)> = sqlx::query_as(
            "SELECT skill_name, invocation_count, last_used_at \
             FROM skill_usage ORDER BY invocation_count DESC",
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(
                |(skill_name, invocation_count, last_used_at)| SkillUsageRow {
                    skill_name,
                    invocation_count,
                    last_used_at,
                },
            )
            .collect())
    }

    /// Record a skill outcome event.
    ///
    /// # Errors
    ///
    /// Returns an error if the insert fails.
    pub async fn record_skill_outcome(
        &self,
        skill_name: &str,
        version_id: Option<i64>,
        conversation_id: Option<i64>,
        outcome: &str,
        error_context: Option<&str>,
    ) -> Result<(), MemoryError> {
        sqlx::query(
            "INSERT INTO skill_outcomes (skill_name, version_id, conversation_id, outcome, error_context) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(skill_name)
        .bind(version_id)
        .bind(conversation_id)
        .bind(outcome)
        .bind(error_context)
        .execute(&self.pool)
        .await
        ?;
        Ok(())
    }

    /// Record outcomes for multiple skills in a single transaction.
    ///
    /// # Errors
    ///
    /// Returns an error if any insert fails (whole batch is rolled back).
    pub async fn record_skill_outcomes_batch(
        &self,
        skill_names: &[String],
        conversation_id: Option<i64>,
        outcome: &str,
        error_context: Option<&str>,
    ) -> Result<(), MemoryError> {
        let mut tx = self.pool.begin().await?;
        for name in skill_names {
            sqlx::query(
                "INSERT INTO skill_outcomes \
                 (skill_name, version_id, conversation_id, outcome, error_context) \
                 VALUES (?, ?, ?, ?, ?)",
            )
            .bind(name)
            .bind(None::<i64>)
            .bind(conversation_id)
            .bind(outcome)
            .bind(error_context)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Load metrics for a skill (latest version group).
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn skill_metrics(
        &self,
        skill_name: &str,
    ) -> Result<Option<SkillMetricsRow>, MemoryError> {
        let row: Option<(String, Option<i64>, i64, i64, i64)> = sqlx::query_as(
            "SELECT skill_name, version_id, \
             COUNT(*) as total, \
             SUM(CASE WHEN outcome = 'success' THEN 1 ELSE 0 END) as successes, \
             COUNT(*) - SUM(CASE WHEN outcome = 'success' THEN 1 ELSE 0 END) as failures \
             FROM skill_outcomes WHERE skill_name = ? \
             GROUP BY skill_name, version_id \
             ORDER BY version_id DESC LIMIT 1",
        )
        .bind(skill_name)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(
            |(skill_name, version_id, total, successes, failures)| SkillMetricsRow {
                skill_name,
                version_id,
                total,
                successes,
                failures,
            },
        ))
    }

    /// Load all skill outcome stats grouped by skill name.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn load_skill_outcome_stats(&self) -> Result<Vec<SkillMetricsRow>, MemoryError> {
        let rows: Vec<(String, Option<i64>, i64, i64, i64)> = sqlx::query_as(
            "SELECT skill_name, version_id, \
             COUNT(*) as total, \
             SUM(CASE WHEN outcome = 'success' THEN 1 ELSE 0 END) as successes, \
             COUNT(*) - SUM(CASE WHEN outcome = 'success' THEN 1 ELSE 0 END) as failures \
             FROM skill_outcomes \
             GROUP BY skill_name \
             ORDER BY total DESC",
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(
                |(skill_name, version_id, total, successes, failures)| SkillMetricsRow {
                    skill_name,
                    version_id,
                    total,
                    successes,
                    failures,
                },
            )
            .collect())
    }

    /// Save a new skill version and return its ID.
    ///
    /// # Errors
    ///
    /// Returns an error if the insert fails.
    #[allow(clippy::too_many_arguments)]
    pub async fn save_skill_version(
        &self,
        skill_name: &str,
        version: i64,
        body: &str,
        description: &str,
        source: &str,
        error_context: Option<&str>,
        predecessor_id: Option<i64>,
    ) -> Result<i64, MemoryError> {
        let row: (i64,) = sqlx::query_as(
            "INSERT INTO skill_versions \
             (skill_name, version, body, description, source, error_context, predecessor_id) \
             VALUES (?, ?, ?, ?, ?, ?, ?) RETURNING id",
        )
        .bind(skill_name)
        .bind(version)
        .bind(body)
        .bind(description)
        .bind(source)
        .bind(error_context)
        .bind(predecessor_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0)
    }

    /// Load the active version for a skill.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn active_skill_version(
        &self,
        skill_name: &str,
    ) -> Result<Option<SkillVersionRow>, MemoryError> {
        let row: Option<SkillVersionTuple> = sqlx::query_as(
            "SELECT id, skill_name, version, body, description, source, \
                 is_active, success_count, failure_count, created_at \
                 FROM skill_versions WHERE skill_name = ? AND is_active = 1 LIMIT 1",
        )
        .bind(skill_name)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(skill_version_from_tuple))
    }

    /// Activate a specific version (deactivates others for the same skill).
    ///
    /// # Errors
    ///
    /// Returns an error if the update fails.
    pub async fn activate_skill_version(
        &self,
        skill_name: &str,
        version_id: i64,
    ) -> Result<(), MemoryError> {
        let mut tx = self.pool.begin().await?;

        sqlx::query(
            "UPDATE skill_versions SET is_active = 0 WHERE skill_name = ? AND is_active = 1",
        )
        .bind(skill_name)
        .execute(&mut *tx)
        .await?;

        sqlx::query("UPDATE skill_versions SET is_active = 1 WHERE id = ?")
            .bind(version_id)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;
        Ok(())
    }

    /// Get the next version number for a skill.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn next_skill_version(&self, skill_name: &str) -> Result<i64, MemoryError> {
        let row: (i64,) = sqlx::query_as(
            "SELECT COALESCE(MAX(version), 0) + 1 FROM skill_versions WHERE skill_name = ?",
        )
        .bind(skill_name)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0)
    }

    /// Get the latest auto-generated version's `created_at` for cooldown check.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn last_improvement_time(
        &self,
        skill_name: &str,
    ) -> Result<Option<String>, MemoryError> {
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT created_at FROM skill_versions \
             WHERE skill_name = ? AND source = 'auto' \
             ORDER BY id DESC LIMIT 1",
        )
        .bind(skill_name)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| r.0))
    }

    /// Ensure a base (v1 manual) version exists for a skill. Idempotent.
    ///
    /// # Errors
    ///
    /// Returns an error if the DB operation fails.
    pub async fn ensure_skill_version_exists(
        &self,
        skill_name: &str,
        body: &str,
        description: &str,
    ) -> Result<(), MemoryError> {
        let existing: Option<(i64,)> =
            sqlx::query_as("SELECT id FROM skill_versions WHERE skill_name = ? LIMIT 1")
                .bind(skill_name)
                .fetch_optional(&self.pool)
                .await?;

        if existing.is_none() {
            let id = self
                .save_skill_version(skill_name, 1, body, description, "manual", None, None)
                .await?;
            self.activate_skill_version(skill_name, id).await?;
        }
        Ok(())
    }

    /// Load all versions for a skill, ordered by version number.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn load_skill_versions(
        &self,
        skill_name: &str,
    ) -> Result<Vec<SkillVersionRow>, MemoryError> {
        let rows: Vec<SkillVersionTuple> = sqlx::query_as(
            "SELECT id, skill_name, version, body, description, source, \
                 is_active, success_count, failure_count, created_at \
                 FROM skill_versions WHERE skill_name = ? ORDER BY version ASC",
        )
        .bind(skill_name)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.into_iter().map(skill_version_from_tuple).collect())
    }

    /// Count auto-generated versions for a skill.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn count_auto_versions(&self, skill_name: &str) -> Result<i64, MemoryError> {
        let row: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM skill_versions WHERE skill_name = ? AND source = 'auto'",
        )
        .bind(skill_name)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0)
    }

    /// Delete oldest non-active auto versions exceeding max limit.
    /// Returns the number of pruned versions.
    ///
    /// # Errors
    ///
    /// Returns an error if the delete fails.
    pub async fn prune_skill_versions(
        &self,
        skill_name: &str,
        max_versions: u32,
    ) -> Result<u32, MemoryError> {
        let result = sqlx::query(
            "DELETE FROM skill_versions WHERE id IN (\
                SELECT id FROM skill_versions \
                WHERE skill_name = ? AND source = 'auto' AND is_active = 0 \
                ORDER BY id ASC \
                LIMIT max(0, (SELECT COUNT(*) FROM skill_versions \
                    WHERE skill_name = ? AND source = 'auto') - ?)\
            )",
        )
        .bind(skill_name)
        .bind(skill_name)
        .bind(max_versions)
        .execute(&self.pool)
        .await?;
        Ok(u32::try_from(result.rows_affected()).unwrap_or(0))
    }

    /// Get the predecessor version for rollback.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn predecessor_version(
        &self,
        version_id: i64,
    ) -> Result<Option<SkillVersionRow>, MemoryError> {
        let pred_id: Option<(Option<i64>,)> =
            sqlx::query_as("SELECT predecessor_id FROM skill_versions WHERE id = ?")
                .bind(version_id)
                .fetch_optional(&self.pool)
                .await?;

        let Some((Some(pid),)) = pred_id else {
            return Ok(None);
        };

        let row: Option<SkillVersionTuple> = sqlx::query_as(
            "SELECT id, skill_name, version, body, description, source, \
                 is_active, success_count, failure_count, created_at \
                 FROM skill_versions WHERE id = ?",
        )
        .bind(pid)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(skill_version_from_tuple))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_store() -> SqliteStore {
        SqliteStore::new(":memory:").await.unwrap()
    }

    #[tokio::test]
    async fn record_skill_usage_increments() {
        let store = test_store().await;

        store.record_skill_usage(&["git"]).await.unwrap();
        store.record_skill_usage(&["git"]).await.unwrap();

        let usage = store.load_skill_usage().await.unwrap();
        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0].skill_name, "git");
        assert_eq!(usage[0].invocation_count, 2);
    }

    #[tokio::test]
    async fn load_skill_usage_returns_all() {
        let store = test_store().await;

        store.record_skill_usage(&["git", "docker"]).await.unwrap();
        store.record_skill_usage(&["git"]).await.unwrap();

        let usage = store.load_skill_usage().await.unwrap();
        assert_eq!(usage.len(), 2);
        assert_eq!(usage[0].skill_name, "git");
        assert_eq!(usage[0].invocation_count, 2);
        assert_eq!(usage[1].skill_name, "docker");
        assert_eq!(usage[1].invocation_count, 1);
    }

    #[tokio::test]
    async fn migration_005_creates_tables() {
        let store = test_store().await;
        let pool = store.pool();

        let versions: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='skill_versions'",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        assert_eq!(versions.0, 1);

        let outcomes: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='skill_outcomes'",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        assert_eq!(outcomes.0, 1);
    }

    #[tokio::test]
    async fn record_skill_outcome_inserts() {
        let store = test_store().await;

        store
            .record_skill_outcome("git", None, Some(1), "success", None)
            .await
            .unwrap();
        store
            .record_skill_outcome("git", None, Some(1), "tool_failure", Some("exit code 1"))
            .await
            .unwrap();

        let metrics = store.skill_metrics("git").await.unwrap().unwrap();
        assert_eq!(metrics.total, 2);
        assert_eq!(metrics.successes, 1);
        assert_eq!(metrics.failures, 1);
    }

    #[tokio::test]
    async fn skill_metrics_none_for_unknown() {
        let store = test_store().await;
        let m = store.skill_metrics("nonexistent").await.unwrap();
        assert!(m.is_none());
    }

    #[tokio::test]
    async fn load_skill_outcome_stats_grouped() {
        let store = test_store().await;

        store
            .record_skill_outcome("git", None, None, "success", None)
            .await
            .unwrap();
        store
            .record_skill_outcome("git", None, None, "tool_failure", None)
            .await
            .unwrap();
        store
            .record_skill_outcome("docker", None, None, "success", None)
            .await
            .unwrap();

        let stats = store.load_skill_outcome_stats().await.unwrap();
        assert_eq!(stats.len(), 2);
        assert_eq!(stats[0].skill_name, "git");
        assert_eq!(stats[0].total, 2);
        assert_eq!(stats[1].skill_name, "docker");
        assert_eq!(stats[1].total, 1);
    }

    #[tokio::test]
    async fn save_and_load_skill_version() {
        let store = test_store().await;

        let id = store
            .save_skill_version("git", 1, "body v1", "Git helper", "manual", None, None)
            .await
            .unwrap();
        assert!(id > 0);

        store.activate_skill_version("git", id).await.unwrap();

        let active = store.active_skill_version("git").await.unwrap().unwrap();
        assert_eq!(active.version, 1);
        assert_eq!(active.body, "body v1");
        assert!(active.is_active);
    }

    #[tokio::test]
    async fn activate_deactivates_previous() {
        let store = test_store().await;

        let v1 = store
            .save_skill_version("git", 1, "v1", "desc", "manual", None, None)
            .await
            .unwrap();
        store.activate_skill_version("git", v1).await.unwrap();

        let v2 = store
            .save_skill_version("git", 2, "v2", "desc", "auto", None, Some(v1))
            .await
            .unwrap();
        store.activate_skill_version("git", v2).await.unwrap();

        let versions = store.load_skill_versions("git").await.unwrap();
        assert_eq!(versions.len(), 2);
        assert!(!versions[0].is_active);
        assert!(versions[1].is_active);
    }

    #[tokio::test]
    async fn next_skill_version_increments() {
        let store = test_store().await;

        let next = store.next_skill_version("git").await.unwrap();
        assert_eq!(next, 1);

        store
            .save_skill_version("git", 1, "v1", "desc", "manual", None, None)
            .await
            .unwrap();
        let next = store.next_skill_version("git").await.unwrap();
        assert_eq!(next, 2);
    }

    #[tokio::test]
    async fn last_improvement_time_returns_auto_only() {
        let store = test_store().await;

        store
            .save_skill_version("git", 1, "v1", "desc", "manual", None, None)
            .await
            .unwrap();

        let t = store.last_improvement_time("git").await.unwrap();
        assert!(t.is_none());

        store
            .save_skill_version("git", 2, "v2", "desc", "auto", None, None)
            .await
            .unwrap();

        let t = store.last_improvement_time("git").await.unwrap();
        assert!(t.is_some());
    }

    #[tokio::test]
    async fn ensure_skill_version_exists_idempotent() {
        let store = test_store().await;

        store
            .ensure_skill_version_exists("git", "body", "Git helper")
            .await
            .unwrap();
        store
            .ensure_skill_version_exists("git", "body2", "Git helper 2")
            .await
            .unwrap();

        let versions = store.load_skill_versions("git").await.unwrap();
        assert_eq!(versions.len(), 1);
        assert_eq!(versions[0].body, "body");
    }

    #[tokio::test]
    async fn load_skill_versions_ordered() {
        let store = test_store().await;

        let v1 = store
            .save_skill_version("git", 1, "v1", "desc", "manual", None, None)
            .await
            .unwrap();
        store
            .save_skill_version("git", 2, "v2", "desc", "auto", None, Some(v1))
            .await
            .unwrap();

        let versions = store.load_skill_versions("git").await.unwrap();
        assert_eq!(versions.len(), 2);
        assert_eq!(versions[0].version, 1);
        assert_eq!(versions[1].version, 2);
    }

    #[tokio::test]
    async fn count_auto_versions_only() {
        let store = test_store().await;

        store
            .save_skill_version("git", 1, "v1", "desc", "manual", None, None)
            .await
            .unwrap();
        store
            .save_skill_version("git", 2, "v2", "desc", "auto", None, None)
            .await
            .unwrap();
        store
            .save_skill_version("git", 3, "v3", "desc", "auto", None, None)
            .await
            .unwrap();

        let count = store.count_auto_versions("git").await.unwrap();
        assert_eq!(count, 2);
    }

    #[tokio::test]
    async fn prune_preserves_manual_and_active() {
        let store = test_store().await;

        let v1 = store
            .save_skill_version("git", 1, "v1", "desc", "manual", None, None)
            .await
            .unwrap();
        store.activate_skill_version("git", v1).await.unwrap();

        for i in 2..=5 {
            store
                .save_skill_version("git", i, &format!("v{i}"), "desc", "auto", None, None)
                .await
                .unwrap();
        }

        let pruned = store.prune_skill_versions("git", 2).await.unwrap();
        assert_eq!(pruned, 2);

        let versions = store.load_skill_versions("git").await.unwrap();
        assert!(versions.iter().any(|v| v.source == "manual"));
        let auto_count = versions.iter().filter(|v| v.source == "auto").count();
        assert_eq!(auto_count, 2);
    }

    #[tokio::test]
    async fn predecessor_version_returns_parent() {
        let store = test_store().await;

        let v1 = store
            .save_skill_version("git", 1, "v1", "desc", "manual", None, None)
            .await
            .unwrap();
        let v2 = store
            .save_skill_version("git", 2, "v2", "desc", "auto", None, Some(v1))
            .await
            .unwrap();

        let pred = store.predecessor_version(v2).await.unwrap().unwrap();
        assert_eq!(pred.id, v1);
        assert_eq!(pred.version, 1);
    }

    #[tokio::test]
    async fn predecessor_version_none_for_root() {
        let store = test_store().await;

        let v1 = store
            .save_skill_version("git", 1, "v1", "desc", "manual", None, None)
            .await
            .unwrap();

        let pred = store.predecessor_version(v1).await.unwrap();
        assert!(pred.is_none());
    }

    #[tokio::test]
    async fn active_skill_version_none_for_unknown() {
        let store = test_store().await;
        let active = store.active_skill_version("nonexistent").await.unwrap();
        assert!(active.is_none());
    }

    #[tokio::test]
    async fn load_skill_outcome_stats_empty() {
        let store = test_store().await;
        let stats = store.load_skill_outcome_stats().await.unwrap();
        assert!(stats.is_empty());
    }

    #[tokio::test]
    async fn load_skill_versions_empty() {
        let store = test_store().await;
        let versions = store.load_skill_versions("nonexistent").await.unwrap();
        assert!(versions.is_empty());
    }

    #[tokio::test]
    async fn count_auto_versions_zero_for_unknown() {
        let store = test_store().await;
        let count = store.count_auto_versions("nonexistent").await.unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn prune_nothing_when_below_limit() {
        let store = test_store().await;

        store
            .save_skill_version("git", 1, "v1", "desc", "auto", None, None)
            .await
            .unwrap();

        let pruned = store.prune_skill_versions("git", 5).await.unwrap();
        assert_eq!(pruned, 0);
    }

    #[tokio::test]
    async fn record_skill_outcome_with_error_context() {
        let store = test_store().await;

        store
            .record_skill_outcome(
                "docker",
                None,
                Some(1),
                "tool_failure",
                Some("container not found"),
            )
            .await
            .unwrap();

        let metrics = store.skill_metrics("docker").await.unwrap().unwrap();
        assert_eq!(metrics.total, 1);
        assert_eq!(metrics.failures, 1);
    }

    #[tokio::test]
    async fn save_skill_version_with_error_context() {
        let store = test_store().await;

        let id = store
            .save_skill_version(
                "git",
                1,
                "improved body",
                "Git helper",
                "auto",
                Some("exit code 128"),
                None,
            )
            .await
            .unwrap();
        assert!(id > 0);
    }
}
