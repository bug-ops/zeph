// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use super::SqliteStore;
use crate::error::MemoryError;
#[allow(unused_imports)]
use zeph_db::{begin_write, sql};

/// Aggregated usage statistics for a single skill, returned by [`SqliteStore::load_skill_usage`].
#[derive(Debug)]
pub struct SkillUsageRow {
    /// The skill's unique name.
    pub skill_name: String,
    /// Total number of times this skill has been invoked.
    pub invocation_count: i64,
    /// ISO-8601 timestamp of the most recent invocation.
    pub last_used_at: String,
}

/// Per-skill outcome metrics (success/failure counts) returned by [`SqliteStore::load_skill_outcome_stats`].
#[derive(Debug)]
pub struct SkillMetricsRow {
    /// The skill's unique name.
    pub skill_name: String,
    /// The version ID this row refers to, if tracked per-version.
    pub version_id: Option<i64>,
    /// Total number of recorded outcomes.
    pub total: i64,
    /// Number of successful outcomes.
    pub successes: i64,
    /// Number of failed outcomes.
    pub failures: i64,
}

/// A single persisted skill version, returned by [`SqliteStore::load_skill_versions`] and related queries.
#[derive(Debug)]
pub struct SkillVersionRow {
    /// Primary key assigned by the database.
    pub id: i64,
    /// The skill's unique name.
    pub skill_name: String,
    /// Monotonically increasing version number within the skill.
    pub version: i64,
    /// Full SKILL.md body for this version.
    pub body: String,
    /// Human-readable description extracted from the skill body.
    pub description: String,
    /// Origin of the version: `"manual"`, `"auto"`, etc.
    pub source: String,
    /// Whether this version is currently the active one for the skill.
    pub is_active: bool,
    /// Number of successful tool invocations recorded against this version.
    pub success_count: i64,
    /// Number of failed tool invocations recorded against this version.
    pub failure_count: i64,
    /// ISO-8601 timestamp when this version was created.
    pub created_at: String,
}

// `version`/`success_count`/`failure_count` are `INTEGER` (`INT4`) on Postgres, so they decode
// as `i32`, not `i64`; `is_active` is `BOOLEAN` on Postgres, so it decodes as `bool` directly,
// not `i64`. Widened/converted back to `SkillVersionRow`'s field types in the function below.
type SkillVersionTuple = (
    i64,
    String,
    i32,
    String,
    String,
    String,
    bool,
    i32,
    i32,
    String,
);

fn skill_version_from_tuple(t: SkillVersionTuple) -> SkillVersionRow {
    SkillVersionRow {
        id: t.0,
        skill_name: t.1,
        version: i64::from(t.2),
        body: t.3,
        description: t.4,
        source: t.5,
        is_active: t.6,
        success_count: i64::from(t.7),
        failure_count: i64::from(t.8),
        created_at: t.9,
    }
}

impl SqliteStore {
    /// Record usage of skills (UPSERT: increment count and update timestamp).
    ///
    /// All UPSERTs are issued inside a single write transaction to avoid N
    /// round-trips and WAL contention under concurrent callers.
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    #[tracing::instrument(skip_all, name = "memory.skills.record_skill_usage")]
    pub async fn record_skill_usage(&self, skill_names: &[&str]) -> Result<(), MemoryError> {
        let mut tx = begin_write(&self.pool).await?;
        for name in skill_names {
            zeph_db::query(sql!(
                "INSERT INTO skill_usage (skill_name, invocation_count, last_used_at) \
                 VALUES (?, 1, CURRENT_TIMESTAMP) \
                 ON CONFLICT(skill_name) DO UPDATE SET \
                 invocation_count = invocation_count + 1, \
                 last_used_at = CURRENT_TIMESTAMP"
            ))
            .bind(name)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Load all skill usage statistics.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    #[tracing::instrument(skip_all, name = "memory.skills.load_skill_usage")]
    pub async fn load_skill_usage(&self) -> Result<Vec<SkillUsageRow>, MemoryError> {
        // `last_used_at` is `TIMESTAMPTZ` on Postgres (`TEXT` on SQLite); project through
        // `Dialect::select_as_text` so it decodes into the `String` tuple field below.
        // `invocation_count` is `INTEGER` (`INT4`) on Postgres, so it decodes as `i32`, not
        // `i64`; widened back to `i64` below to keep `SkillUsageRow`'s field type unchanged.
        let last_used_at_sel =
            <zeph_db::ActiveDialect as zeph_db::dialect::Dialect>::select_as_text("last_used_at");
        let raw = format!(
            "SELECT skill_name, invocation_count, {last_used_at_sel} \
             FROM skill_usage ORDER BY invocation_count DESC"
        );
        let query_sql = zeph_db::rewrite_placeholders(&raw);
        let rows: Vec<(String, i32, String)> = zeph_db::query_as(sqlx::AssertSqlSafe(query_sql))
            .fetch_all(&self.pool)
            .await?;

        Ok(rows
            .into_iter()
            .map(
                |(skill_name, invocation_count, last_used_at)| SkillUsageRow {
                    skill_name,
                    invocation_count: i64::from(invocation_count),
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
    #[tracing::instrument(skip_all, name = "memory.skills.record_skill_outcome")]
    pub async fn record_skill_outcome(
        &self,
        skill_name: &str,
        version_id: Option<i64>,
        conversation_id: Option<crate::types::ConversationId>,
        outcome: &str,
        error_context: Option<&str>,
        outcome_detail: Option<&str>,
    ) -> Result<(), MemoryError> {
        zeph_db::query(sql!(
            "INSERT INTO skill_outcomes \
             (skill_name, version_id, conversation_id, outcome, error_context, outcome_detail) \
             VALUES (?, ?, ?, ?, ?, ?)"
        ))
        .bind(skill_name)
        .bind(version_id)
        .bind(conversation_id)
        .bind(outcome)
        .bind(error_context)
        .bind(outcome_detail)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Record outcomes for multiple skills in a single transaction.
    ///
    /// # Errors
    ///
    /// Returns an error if any insert fails (whole batch is rolled back).
    #[tracing::instrument(skip_all, name = "memory.skills.record_skill_outcomes_batch")]
    pub async fn record_skill_outcomes_batch(
        &self,
        skill_names: &[String],
        conversation_id: Option<crate::types::ConversationId>,
        outcome: &str,
        error_context: Option<&str>,
        outcome_detail: Option<&str>,
    ) -> Result<(), MemoryError> {
        // Acquire the write lock up front to avoid DEFERRED read->write upgrades
        // failing with SQLITE_BUSY_SNAPSHOT under concurrent WAL writers.
        let mut tx = begin_write(&self.pool).await?;

        let mut version_map: std::collections::HashMap<String, Option<i64>> =
            std::collections::HashMap::new();
        for name in skill_names {
            let vid: Option<(i64,)> = zeph_db::query_as(sql!(
                "SELECT id FROM skill_versions WHERE skill_name = ? AND is_active = TRUE"
            ))
            .bind(name)
            .fetch_optional(&mut *tx)
            .await?;
            version_map.insert(name.clone(), vid.map(|r| r.0));
        }

        for name in skill_names {
            let version_id = version_map.get(name.as_str()).copied().flatten();
            zeph_db::query(sql!(
                "INSERT INTO skill_outcomes \
                 (skill_name, version_id, conversation_id, outcome, error_context, outcome_detail) \
                 VALUES (?, ?, ?, ?, ?, ?)"
            ))
            .bind(name)
            .bind(version_id)
            .bind(conversation_id)
            .bind(outcome)
            .bind(error_context)
            .bind(outcome_detail)
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
    #[tracing::instrument(skip_all, name = "memory.skills.skill_metrics")]
    pub async fn skill_metrics(
        &self,
        skill_name: &str,
    ) -> Result<Option<SkillMetricsRow>, MemoryError> {
        let row: Option<(String, Option<i64>, i64, i64, i64)> = zeph_db::query_as(sql!(
            "SELECT skill_name, version_id, \
             COUNT(*) as total, \
             SUM(CASE WHEN outcome = 'success' THEN 1 ELSE 0 END) as successes, \
             COUNT(*) - SUM(CASE WHEN outcome = 'success' THEN 1 ELSE 0 END) as failures \
             FROM skill_outcomes WHERE skill_name = ? \
             AND outcome NOT IN ('user_approval', 'user_rejection') \
             GROUP BY skill_name, version_id \
             ORDER BY version_id DESC LIMIT 1"
        ))
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
    #[tracing::instrument(skip_all, name = "memory.skills.load_skill_outcome_stats")]
    pub async fn load_skill_outcome_stats(&self) -> Result<Vec<SkillMetricsRow>, MemoryError> {
        let rows: Vec<(String, Option<i64>, i64, i64, i64)> = zeph_db::query_as(sql!(
            "SELECT skill_name, version_id, \
             COUNT(*) as total, \
             SUM(CASE WHEN outcome = 'success' THEN 1 ELSE 0 END) as successes, \
             COUNT(*) - SUM(CASE WHEN outcome = 'success' THEN 1 ELSE 0 END) as failures \
             FROM skill_outcomes \
             GROUP BY skill_name \
             ORDER BY total DESC"
        ))
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
    // function with many required inputs; a *Params struct would be more verbose without simplifying the call site
    #[tracing::instrument(skip_all, name = "memory.skills.save_skill_version")]
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
        let row: (i64,) = zeph_db::query_as(sql!(
            "INSERT INTO skill_versions \
             (skill_name, version, body, description, source, error_context, predecessor_id) \
             VALUES (?, ?, ?, ?, ?, ?, ?) RETURNING id"
        ))
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

    /// Save a new skill version and atomically activate it within a single `BEGIN IMMEDIATE`
    /// transaction, preventing orphaned saved-but-inactive versions on crash or concurrent access.
    ///
    /// Equivalent to calling [`Self::save_skill_version`] followed by [`Self::activate_skill_version`] but
    /// without the window between the two calls where the DB can be left in an inconsistent state.
    ///
    /// # Errors
    ///
    /// Returns an error if the insert, deactivate, or activate queries fail. The transaction is
    /// rolled back automatically on error.
    #[allow(clippy::too_many_arguments)]
    #[tracing::instrument(skip_all, name = "memory.skills.save_and_activate_skill_version")]
    pub async fn save_and_activate_skill_version(
        &self,
        skill_name: &str,
        version: i64,
        body: &str,
        description: &str,
        source: &str,
        error_context: Option<&str>,
        predecessor_id: Option<i64>,
    ) -> Result<i64, MemoryError> {
        let mut tx = begin_write(&self.pool).await?;

        let row: (i64,) = zeph_db::query_as(sql!(
            "INSERT INTO skill_versions \
             (skill_name, version, body, description, source, error_context, predecessor_id) \
             VALUES (?, ?, ?, ?, ?, ?, ?) RETURNING id"
        ))
        .bind(skill_name)
        .bind(version)
        .bind(body)
        .bind(description)
        .bind(source)
        .bind(error_context)
        .bind(predecessor_id)
        .fetch_one(&mut *tx)
        .await?;
        let id = row.0;

        zeph_db::query(sql!(
            "UPDATE skill_versions SET is_active = FALSE WHERE skill_name = ? AND is_active = TRUE"
        ))
        .bind(skill_name)
        .execute(&mut *tx)
        .await?;

        zeph_db::query(sql!(
            "UPDATE skill_versions SET is_active = TRUE WHERE id = ?"
        ))
        .bind(id)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(id)
    }

    /// Count the number of distinct conversation sessions in which a skill produced an outcome.
    ///
    /// Uses `COUNT(DISTINCT conversation_id)` from `skill_outcomes`. Rows where
    /// `conversation_id IS NULL` are excluded (legacy rows without session tracking).
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    #[tracing::instrument(skip_all, name = "memory.skills.distinct_session_count")]
    pub async fn distinct_session_count(&self, skill_name: &str) -> Result<i64, MemoryError> {
        let row: (i64,) = zeph_db::query_as(sql!(
            "SELECT COUNT(DISTINCT conversation_id) FROM skill_outcomes \
             WHERE skill_name = ? AND conversation_id IS NOT NULL"
        ))
        .bind(skill_name)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0)
    }

    /// Load the active version for a skill.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    #[tracing::instrument(skip_all, name = "memory.skills.active_skill_version")]
    pub async fn active_skill_version(
        &self,
        skill_name: &str,
    ) -> Result<Option<SkillVersionRow>, MemoryError> {
        // `created_at` is `TIMESTAMPTZ` on Postgres (`TEXT` on SQLite); project through
        // `Dialect::select_as_text` so it decodes into the `String` tuple field below.
        let created_at_sel =
            <zeph_db::ActiveDialect as zeph_db::dialect::Dialect>::select_as_text("created_at");
        let raw = format!(
            "SELECT id, skill_name, version, body, description, source, \
                 is_active, success_count, failure_count, {created_at_sel} \
                 FROM skill_versions WHERE skill_name = ? AND is_active = TRUE LIMIT 1"
        );
        let query_sql = zeph_db::rewrite_placeholders(&raw);
        let row: Option<SkillVersionTuple> = zeph_db::query_as(sqlx::AssertSqlSafe(query_sql))
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
    #[tracing::instrument(skip_all, name = "memory.skills.activate_skill_version")]
    pub async fn activate_skill_version(
        &self,
        skill_name: &str,
        version_id: i64,
    ) -> Result<(), MemoryError> {
        let mut tx = begin_write(&self.pool).await?;

        zeph_db::query(sql!(
            "UPDATE skill_versions SET is_active = FALSE WHERE skill_name = ? AND is_active = TRUE"
        ))
        .bind(skill_name)
        .execute(&mut *tx)
        .await?;

        zeph_db::query(sql!(
            "UPDATE skill_versions SET is_active = TRUE WHERE id = ?"
        ))
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
    #[tracing::instrument(skip_all, name = "memory.skills.next_skill_version")]
    pub async fn next_skill_version(&self, skill_name: &str) -> Result<i64, MemoryError> {
        let row: (i64,) = zeph_db::query_as(sql!(
            "SELECT COALESCE(MAX(version), 0) + 1 FROM skill_versions WHERE skill_name = ?"
        ))
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
    #[tracing::instrument(skip_all, name = "memory.skills.last_improvement_time")]
    pub async fn last_improvement_time(
        &self,
        skill_name: &str,
    ) -> Result<Option<String>, MemoryError> {
        // `created_at` is `TIMESTAMPTZ` on Postgres — see `active_skill_version`.
        let created_at_sel =
            <zeph_db::ActiveDialect as zeph_db::dialect::Dialect>::select_as_text("created_at");
        let raw = format!(
            "SELECT {created_at_sel} FROM skill_versions \
             WHERE skill_name = ? AND source = 'auto' \
             ORDER BY id DESC LIMIT 1"
        );
        let query_sql = zeph_db::rewrite_placeholders(&raw);
        let row: Option<(String,)> = zeph_db::query_as(sqlx::AssertSqlSafe(query_sql))
            .bind(skill_name)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.0))
    }

    /// Ensure a base (v1 manual) version exists for a skill. Idempotent.
    ///
    /// The check-then-insert-then-activate sequence runs inside a single write
    /// transaction so that two concurrent callers cannot both observe
    /// `existing.is_none()` and race to insert duplicate v1 rows.
    ///
    /// # Errors
    ///
    /// Returns an error if the DB operation fails.
    #[tracing::instrument(skip_all, name = "memory.skills.ensure_skill_version_exists")]
    pub async fn ensure_skill_version_exists(
        &self,
        skill_name: &str,
        body: &str,
        description: &str,
    ) -> Result<(), MemoryError> {
        let mut tx = begin_write(&self.pool).await?;

        let existing: Option<(i64,)> = zeph_db::query_as(sql!(
            "SELECT id FROM skill_versions WHERE skill_name = ? LIMIT 1"
        ))
        .bind(skill_name)
        .fetch_optional(&mut *tx)
        .await?;

        if existing.is_none() {
            let row: (i64,) = zeph_db::query_as(sql!(
                "INSERT INTO skill_versions \
                 (skill_name, version, body, description, source, error_context, predecessor_id) \
                 VALUES (?, 1, ?, ?, 'manual', NULL, NULL) RETURNING id"
            ))
            .bind(skill_name)
            .bind(body)
            .bind(description)
            .fetch_one(&mut *tx)
            .await?;
            let id = row.0;

            zeph_db::query(sql!(
                "UPDATE skill_versions SET is_active = FALSE WHERE skill_name = ? AND is_active = TRUE"
            ))
            .bind(skill_name)
            .execute(&mut *tx)
            .await?;

            zeph_db::query(sql!(
                "UPDATE skill_versions SET is_active = TRUE WHERE id = ?"
            ))
            .bind(id)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(())
    }

    /// Load all versions for a skill, ordered by version number.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    #[tracing::instrument(skip_all, name = "memory.skills.load_skill_versions")]
    pub async fn load_skill_versions(
        &self,
        skill_name: &str,
    ) -> Result<Vec<SkillVersionRow>, MemoryError> {
        // `created_at` is `TIMESTAMPTZ` on Postgres — see `active_skill_version`.
        let created_at_sel =
            <zeph_db::ActiveDialect as zeph_db::dialect::Dialect>::select_as_text("created_at");
        let raw = format!(
            "SELECT id, skill_name, version, body, description, source, \
                 is_active, success_count, failure_count, {created_at_sel} \
                 FROM skill_versions WHERE skill_name = ? ORDER BY version ASC"
        );
        let query_sql = zeph_db::rewrite_placeholders(&raw);
        let rows: Vec<SkillVersionTuple> = zeph_db::query_as(sqlx::AssertSqlSafe(query_sql))
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
    #[tracing::instrument(skip_all, name = "memory.skills.count_auto_versions")]
    pub async fn count_auto_versions(&self, skill_name: &str) -> Result<i64, MemoryError> {
        let row: (i64,) = zeph_db::query_as(sql!(
            "SELECT COUNT(*) FROM skill_versions WHERE skill_name = ? AND source = 'auto'"
        ))
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
    #[cfg(not(feature = "postgres"))]
    #[tracing::instrument(skip_all, name = "memory.skills.prune_skill_versions")]
    pub async fn prune_skill_versions(
        &self,
        skill_name: &str,
        max_versions: u32,
    ) -> Result<u32, MemoryError> {
        let result = zeph_db::query(sql!(
            "DELETE FROM skill_versions WHERE id IN (\
                SELECT id FROM skill_versions \
                WHERE skill_name = ? AND source = 'auto' AND is_active = FALSE \
                ORDER BY id ASC \
                LIMIT max(0, (SELECT COUNT(*) FROM skill_versions \
                    WHERE skill_name = ? AND source = 'auto') - ?)\
            )"
        ))
        .bind(skill_name)
        .bind(skill_name)
        .bind(max_versions)
        .execute(&self.pool)
        .await?;
        Ok(u32::try_from(result.rows_affected()).unwrap_or(0))
    }

    /// Prune old auto-generated skill versions, keeping only the most recent `max_versions`.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    #[cfg(feature = "postgres")]
    #[tracing::instrument(skip_all, name = "memory.skills.prune_skill_versions")]
    pub async fn prune_skill_versions(
        &self,
        skill_name: &str,
        max_versions: u32,
    ) -> Result<u32, MemoryError> {
        let result = zeph_db::query(sql!(
            "DELETE FROM skill_versions WHERE id IN (\
                SELECT id FROM skill_versions \
                WHERE skill_name = ? AND source = 'auto' AND is_active = FALSE \
                ORDER BY id DESC \
                OFFSET ?\
            )"
        ))
        .bind(skill_name)
        .bind(i64::from(max_versions))
        .execute(&self.pool)
        .await?;
        Ok(u32::try_from(result.rows_affected()).unwrap_or(0))
    }

    /// Get the predecessor version for rollback.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    #[tracing::instrument(skip_all, name = "memory.skills.predecessor_version")]
    pub async fn predecessor_version(
        &self,
        version_id: i64,
    ) -> Result<Option<SkillVersionRow>, MemoryError> {
        let pred_id: Option<(Option<i64>,)> = zeph_db::query_as(sql!(
            "SELECT predecessor_id FROM skill_versions WHERE id = ?"
        ))
        .bind(version_id)
        .fetch_optional(&self.pool)
        .await?;

        let Some((Some(pid),)) = pred_id else {
            return Ok(None);
        };

        // `created_at` is `TIMESTAMPTZ` on Postgres — see `active_skill_version`.
        let created_at_sel =
            <zeph_db::ActiveDialect as zeph_db::dialect::Dialect>::select_as_text("created_at");
        let raw = format!(
            "SELECT id, skill_name, version, body, description, source, \
                 is_active, success_count, failure_count, {created_at_sel} \
                 FROM skill_versions WHERE id = ?"
        );
        let query_sql = zeph_db::rewrite_placeholders(&raw);
        let row: Option<SkillVersionTuple> = zeph_db::query_as(sqlx::AssertSqlSafe(query_sql))
            .bind(pid)
            .fetch_optional(&self.pool)
            .await?;

        Ok(row.map(skill_version_from_tuple))
    }

    /// Return the skill names for all currently active auto-generated versions.
    ///
    /// Used to check rollback eligibility at the start of each agent turn.
    ///
    /// # Errors
    /// Returns [`MemoryError`] on `SQLite` query failure.
    #[tracing::instrument(skip_all, name = "memory.skills.list_active_auto_versions")]
    pub async fn list_active_auto_versions(&self) -> Result<Vec<String>, MemoryError> {
        let rows: Vec<(String,)> = zeph_db::query_as(sql!(
            "SELECT skill_name FROM skill_versions WHERE is_active = TRUE AND source = 'auto'"
        ))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(name,)| name).collect())
    }

    // --- STEM: skill_usage_log queries ---

    /// Insert a tool usage log entry.
    ///
    /// `tool_sequence` must be a normalized compact JSON array (see `stem::normalize_tool_sequence`).
    /// `sequence_hash` is the 16-char blake3 hex of `tool_sequence`.
    ///
    /// # Errors
    /// Returns [`MemoryError`] on insert failure.
    #[tracing::instrument(skip_all, name = "memory.skills.insert_tool_usage_log")]
    pub async fn insert_tool_usage_log(
        &self,
        tool_sequence: &str,
        sequence_hash: &str,
        context_hash: &str,
        outcome: &str,
        conversation_id: Option<crate::types::ConversationId>,
    ) -> Result<(), MemoryError> {
        zeph_db::query(sql!(
            "INSERT INTO skill_usage_log \
             (tool_sequence, sequence_hash, context_hash, outcome, conversation_id) \
             VALUES (?, ?, ?, ?, ?)"
        ))
        .bind(tool_sequence)
        .bind(sequence_hash)
        .bind(context_hash)
        .bind(outcome)
        .bind(conversation_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Find tool sequences that have been seen at least `min_count` times within the last
    /// `window_days` days.
    ///
    /// Returns `(tool_sequence, sequence_hash, occurrence_count, success_count)` tuples.
    ///
    /// # Errors
    /// Returns [`MemoryError`] on query failure.
    #[cfg(not(feature = "postgres"))]
    #[tracing::instrument(skip_all, name = "memory.skills.find_recurring_patterns")]
    pub async fn find_recurring_patterns(
        &self,
        min_count: u32,
        window_days: u32,
    ) -> Result<Vec<(String, String, u32, u32)>, MemoryError> {
        let rows: Vec<(String, String, i64, i64)> = zeph_db::query_as(sql!(
            "SELECT tool_sequence, sequence_hash, \
                    COUNT(*) as occurrence_count, \
                    SUM(CASE WHEN outcome = 'success' THEN 1 ELSE 0 END) as success_count \
             FROM skill_usage_log \
             WHERE created_at > datetime('now', '-' || ? || ' days') \
             GROUP BY sequence_hash \
             HAVING occurrence_count >= ? \
             ORDER BY occurrence_count DESC \
             LIMIT 10"
        ))
        .bind(window_days)
        .bind(min_count)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|(seq, hash, occ, suc)| {
                (
                    seq,
                    hash,
                    u32::try_from(occ).unwrap_or(u32::MAX),
                    u32::try_from(suc).unwrap_or(0),
                )
            })
            .collect())
    }

    /// Find tool+skill combinations used at least `min_count` times within `window_days` days.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    #[cfg(feature = "postgres")]
    #[tracing::instrument(skip_all, name = "memory.skills.find_recurring_patterns")]
    pub async fn find_recurring_patterns(
        &self,
        min_count: u32,
        window_days: u32,
    ) -> Result<Vec<(String, String, u32, u32)>, MemoryError> {
        let rows: Vec<(String, String, i64, i64)> = zeph_db::query_as(sql!(
            "SELECT tool_sequence, sequence_hash, \
                    COUNT(*) as occurrence_count, \
                    SUM(CASE WHEN outcome = 'success' THEN 1 ELSE 0 END) as success_count \
             FROM skill_usage_log \
             WHERE created_at > NOW() - INTERVAL '1 day' * ? \
             GROUP BY sequence_hash, tool_sequence \
             HAVING COUNT(*) >= ? \
             ORDER BY occurrence_count DESC \
             LIMIT 10"
        ))
        .bind(i64::from(window_days))
        .bind(i64::from(min_count))
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|(seq, hash, occ, suc)| {
                (
                    seq,
                    hash,
                    u32::try_from(occ).unwrap_or(u32::MAX),
                    u32::try_from(suc).unwrap_or(0),
                )
            })
            .collect())
    }

    /// Delete `skill_usage_log` rows older than `retention_days` days.
    ///
    /// # Errors
    /// Returns [`MemoryError`] on delete failure.
    #[cfg(not(feature = "postgres"))]
    #[tracing::instrument(skip_all, name = "memory.skills.prune_tool_usage_log")]
    pub async fn prune_tool_usage_log(&self, retention_days: u32) -> Result<u64, MemoryError> {
        let result = zeph_db::query(sql!(
            "DELETE FROM skill_usage_log \
             WHERE created_at < datetime('now', '-' || ? || ' days')"
        ))
        .bind(retention_days)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// Delete tool usage log entries older than `retention_days` days.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    #[cfg(feature = "postgres")]
    #[tracing::instrument(skip_all, name = "memory.skills.prune_tool_usage_log")]
    pub async fn prune_tool_usage_log(&self, retention_days: u32) -> Result<u64, MemoryError> {
        let result = zeph_db::query(sql!(
            "DELETE FROM skill_usage_log \
             WHERE created_at < NOW() - INTERVAL '1 day' * ?"
        ))
        .bind(i64::from(retention_days))
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    // --- ERL: skill_heuristics queries ---

    /// Insert a new heuristic (no dedup — caller must check first).
    ///
    /// # Errors
    /// Returns [`MemoryError`] on insert failure.
    #[tracing::instrument(skip_all, name = "memory.skills.insert_skill_heuristic")]
    pub async fn insert_skill_heuristic(
        &self,
        skill_name: Option<&str>,
        heuristic_text: &str,
        confidence: f64,
    ) -> Result<i64, MemoryError> {
        let row: (i64,) = zeph_db::query_as(sql!(
            "INSERT INTO skill_heuristics (skill_name, heuristic_text, confidence) \
             VALUES (?, ?, ?) RETURNING id"
        ))
        .bind(skill_name)
        .bind(heuristic_text)
        .bind(confidence)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.0)
    }

    /// Increment `use_count` and update `updated_at` for an existing heuristic by ID.
    ///
    /// # Errors
    /// Returns [`MemoryError`] on update failure.
    #[cfg(not(feature = "postgres"))]
    #[tracing::instrument(skip_all, name = "memory.skills.increment_heuristic_use_count")]
    pub async fn increment_heuristic_use_count(&self, id: i64) -> Result<(), MemoryError> {
        zeph_db::query(sql!(
            "UPDATE skill_heuristics \
             SET use_count = use_count + 1, updated_at = datetime('now') \
             WHERE id = ?"
        ))
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Increment `use_count` and update `updated_at` for an existing heuristic by ID.
    ///
    /// # Errors
    /// Returns [`MemoryError`] on update failure.
    #[cfg(feature = "postgres")]
    #[tracing::instrument(skip_all, name = "memory.skills.increment_heuristic_use_count")]
    pub async fn increment_heuristic_use_count(&self, id: i64) -> Result<(), MemoryError> {
        zeph_db::query(sql!(
            "UPDATE skill_heuristics \
             SET use_count = use_count + 1, updated_at = CURRENT_TIMESTAMP \
             WHERE id = $1"
        ))
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Load heuristics for a given skill (exact match + NULL/general), ordered by confidence DESC.
    ///
    /// Returns `(id, heuristic_text, confidence, use_count)` tuples.
    /// At most `limit` rows are returned.
    ///
    /// # Errors
    /// Returns [`MemoryError`] on query failure.
    #[tracing::instrument(skip_all, name = "memory.skills.load_skill_heuristics")]
    pub async fn load_skill_heuristics(
        &self,
        skill_name: &str,
        min_confidence: f64,
        limit: u32,
    ) -> Result<Vec<(i64, String, f64, i64)>, MemoryError> {
        let rows: Vec<(i64, String, f64, i64)> = zeph_db::query_as(sql!(
            "SELECT id, heuristic_text, confidence, use_count \
             FROM skill_heuristics \
             WHERE (skill_name = ? OR skill_name IS NULL) \
               AND confidence >= ? \
             ORDER BY confidence DESC \
             LIMIT ?"
        ))
        .bind(skill_name)
        .bind(min_confidence)
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Load all heuristics for a skill (for dedup checks), without confidence filter.
    ///
    /// Returns `(id, heuristic_text)` tuples.
    ///
    /// # Errors
    /// Returns [`MemoryError`] on query failure.
    #[tracing::instrument(skip_all, name = "memory.skills.load_all_heuristics_for_skill")]
    pub async fn load_all_heuristics_for_skill(
        &self,
        skill_name: Option<&str>,
    ) -> Result<Vec<(i64, String)>, MemoryError> {
        let rows: Vec<(i64, String)> = zeph_db::query_as(sql!(
            "SELECT id, heuristic_text FROM skill_heuristics \
             WHERE (skill_name = ? OR (? IS NULL AND skill_name IS NULL))"
        ))
        .bind(skill_name)
        .bind(skill_name)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    // --- D2Skill: step-level error corrections ---

    /// Find matching step corrections for a tool failure.
    ///
    /// Returns `(id, hint)` pairs ordered by `success_count DESC, use_count DESC`.
    /// Uses `INSTR` instead of LIKE to avoid wildcard injection from `error_context`.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError`] on query failure.
    #[cfg(not(feature = "postgres"))]
    #[tracing::instrument(skip_all, name = "memory.skills.find_step_corrections")]
    pub async fn find_step_corrections(
        &self,
        skill_name: &str,
        failure_kind: &str,
        error_context: &str,
        tool_name: &str,
        limit: u32,
    ) -> Result<Vec<(i64, String)>, MemoryError> {
        let rows: Vec<(i64, String)> = zeph_db::query_as(sql!(
            "SELECT id, hint FROM step_corrections \
             WHERE skill_name = ? \
               AND (failure_kind = '' OR failure_kind = ?) \
               AND (error_substring = '' OR INSTR(?, error_substring) > 0) \
               AND (tool_name = '' OR tool_name = ?) \
             ORDER BY success_count DESC, use_count DESC \
             LIMIT ?"
        ))
        .bind(skill_name)
        .bind(failure_kind)
        .bind(error_context)
        .bind(tool_name)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Find corrections that match the given skill, failure kind, error context, and tool name.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    #[cfg(feature = "postgres")]
    #[tracing::instrument(skip_all, name = "memory.skills.find_step_corrections")]
    pub async fn find_step_corrections(
        &self,
        skill_name: &str,
        failure_kind: &str,
        error_context: &str,
        tool_name: &str,
        limit: u32,
    ) -> Result<Vec<(i64, String)>, MemoryError> {
        let rows: Vec<(i64, String)> = zeph_db::query_as(sql!(
            "SELECT id, hint FROM step_corrections \
             WHERE skill_name = ? \
               AND (failure_kind = '' OR failure_kind = ?) \
               AND (error_substring = '' OR strpos(?, error_substring) > 0) \
               AND (tool_name = '' OR tool_name = ?) \
             ORDER BY success_count DESC, use_count DESC \
             LIMIT ?"
        ))
        .bind(skill_name)
        .bind(failure_kind)
        .bind(error_context)
        .bind(tool_name)
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Insert a new step correction (from ARISE trace analysis).
    ///
    /// Duplicate `(skill_name, failure_kind, error_substring, tool_name)` tuples are silently
    /// ignored (handled by `ON CONFLICT IGNORE` in the schema).
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError`] on query failure.
    #[tracing::instrument(skip_all, name = "memory.skills.insert_step_correction")]
    pub async fn insert_step_correction(
        &self,
        skill_name: &str,
        failure_kind: &str,
        error_substring: &str,
        tool_name: &str,
        hint: &str,
    ) -> Result<(), MemoryError> {
        zeph_db::query(sql!(
            "INSERT INTO step_corrections \
             (skill_name, failure_kind, error_substring, tool_name, hint) \
             VALUES (?, ?, ?, ?, ?) \
             ON CONFLICT (skill_name, failure_kind, error_substring, tool_name) DO NOTHING"
        ))
        .bind(skill_name)
        .bind(failure_kind)
        .bind(error_substring)
        .bind(tool_name)
        .bind(hint)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Increment `use_count` and optionally `success_count` for a step correction.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError`] on query failure.
    #[tracing::instrument(skip_all, name = "memory.skills.record_correction_usage")]
    pub async fn record_correction_usage(
        &self,
        correction_id: i64,
        was_successful: bool,
    ) -> Result<(), MemoryError> {
        if was_successful {
            zeph_db::query(sql!(
                "UPDATE step_corrections \
                 SET use_count = use_count + 1, success_count = success_count + 1 \
                 WHERE id = ?"
            ))
            .bind(correction_id)
            .execute(&self.pool)
            .await?;
        } else {
            zeph_db::query(sql!(
                "UPDATE step_corrections SET use_count = use_count + 1 WHERE id = ?"
            ))
            .bind(correction_id)
            .execute(&self.pool)
            .await?;
        }
        Ok(())
    }

    // --- SkillOrchestra: RL routing head weight persistence ---

    /// Load routing head weights blob from the singleton row.
    ///
    /// Returns `(embed_dim, weights_bytes, baseline, update_count)` or `None` if not yet trained.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError`] on query failure.
    #[tracing::instrument(skip_all, name = "memory.skills.load_routing_head_weights")]
    #[allow(clippy::type_complexity)]
    pub async fn load_routing_head_weights(
        &self,
    ) -> Result<Option<(i64, Vec<u8>, f64, i64)>, MemoryError> {
        let row: Option<(i64, Vec<u8>, f64, i64)> = zeph_db::query_as(sql!(
            "SELECT embed_dim, weights, baseline, update_count \
             FROM routing_head_weights WHERE id = 1"
        ))
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    /// Persist routing head weights (upsert singleton row).
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError`] on query failure.
    #[cfg(not(feature = "postgres"))]
    #[tracing::instrument(skip_all, name = "memory.skills.save_routing_head_weights")]
    pub async fn save_routing_head_weights(
        &self,
        embed_dim: i64,
        weights: &[u8],
        baseline: f64,
        update_count: i64,
    ) -> Result<(), MemoryError> {
        zeph_db::query(sql!(
            "INSERT INTO routing_head_weights (id, embed_dim, weights, baseline, update_count, updated_at) \
             VALUES (1, ?, ?, ?, ?, datetime('now')) \
             ON CONFLICT(id) DO UPDATE SET \
               embed_dim = excluded.embed_dim, \
               weights = excluded.weights, \
               baseline = excluded.baseline, \
               update_count = excluded.update_count, \
               updated_at = datetime('now')"
        ))
        .bind(embed_dim)
        .bind(weights)
        .bind(baseline)
        .bind(update_count)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Persist routing head weights (upsert singleton row).
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError`] on query failure.
    #[cfg(feature = "postgres")]
    #[tracing::instrument(skip_all, name = "memory.skills.save_routing_head_weights")]
    pub async fn save_routing_head_weights(
        &self,
        embed_dim: i64,
        weights: &[u8],
        baseline: f64,
        update_count: i64,
    ) -> Result<(), MemoryError> {
        zeph_db::query(sql!(
            "INSERT INTO routing_head_weights (id, embed_dim, weights, baseline, update_count, updated_at) \
             VALUES (1, $1, $2, $3, $4, CURRENT_TIMESTAMP) \
             ON CONFLICT(id) DO UPDATE SET \
               embed_dim = excluded.embed_dim, \
               weights = excluded.weights, \
               baseline = excluded.baseline, \
               update_count = excluded.update_count, \
               updated_at = CURRENT_TIMESTAMP"
        ))
        .bind(embed_dim)
        .bind(weights)
        .bind(baseline)
        .bind(update_count)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // --- AutoSkill A6: Heuristic promotion helpers (spec 061) ---

    /// Return skills where the count of heuristics (above `min_confidence`) meets
    /// `min_count`. Each entry is `(skill_name, heuristic_count)`.
    ///
    /// Used by the promotion background task to find qualifying skills.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    #[tracing::instrument(skip_all, name = "memory.skills.count_heuristics_by_skill")]
    pub async fn count_heuristics_by_skill(
        &self,
        min_confidence: f64,
        min_count: u32,
    ) -> Result<Vec<(String, i64)>, MemoryError> {
        let rows: Vec<(String, i64)> = zeph_db::query_as(sql!(
            "SELECT skill_name, COUNT(*) AS cnt \
             FROM skill_heuristics \
             WHERE confidence >= ? AND skill_name IS NOT NULL \
             GROUP BY skill_name \
             HAVING cnt >= ?"
        ))
        .bind(min_confidence)
        .bind(i64::from(min_count))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Load all heuristic texts for a specific skill above `min_confidence`.
    ///
    /// Returns heuristic texts sorted alphabetically (deterministic batch hash).
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    #[tracing::instrument(skip_all, name = "memory.skills.load_heuristic_texts_for_promotion")]
    pub async fn load_heuristic_texts_for_promotion(
        &self,
        skill_name: &str,
        min_confidence: f64,
    ) -> Result<Vec<String>, MemoryError> {
        let rows: Vec<(String,)> = zeph_db::query_as(sql!(
            "SELECT heuristic_text \
             FROM skill_heuristics \
             WHERE skill_name = ? AND confidence >= ? \
             ORDER BY heuristic_text ASC"
        ))
        .bind(skill_name)
        .bind(min_confidence)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|r| r.0).collect())
    }

    /// Check whether `(skill_name, batch_hash)` already exists in `skill_heuristic_promotions`.
    ///
    /// Returns `true` when the batch was already evaluated — the promotion loop should skip it.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    #[tracing::instrument(skip_all, name = "memory.skills.promotion_already_evaluated")]
    pub async fn promotion_already_evaluated(
        &self,
        skill_name: &str,
        batch_hash: &str,
    ) -> Result<bool, MemoryError> {
        let count: i64 = zeph_db::query_scalar(sql!(
            "SELECT COUNT(*) FROM skill_heuristic_promotions \
             WHERE skill_name = ? AND batch_hash = ?"
        ))
        .bind(skill_name)
        .bind(batch_hash)
        .fetch_one(&self.pool)
        .await?;
        Ok(count > 0)
    }

    /// Record a promotion evaluation result in `skill_heuristic_promotions`.
    ///
    /// Uses `INSERT OR IGNORE` so concurrent callers are safe (`batch_hash` is part of
    /// the primary key).
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    #[tracing::instrument(skip_all, name = "memory.skills.record_promotion_evaluation")]
    pub async fn record_promotion_evaluation(
        &self,
        skill_name: &str,
        batch_hash: &str,
        recommendation: &str,
        draft_skill_name: Option<&str>,
    ) -> Result<(), MemoryError> {
        let now = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        )
        .unwrap_or(i64::MAX);
        zeph_db::query(sql!(
            "INSERT OR IGNORE INTO skill_heuristic_promotions \
             (skill_name, batch_hash, evaluated_at, recommendation, draft_skill_name) \
             VALUES (?, ?, ?, ?, ?)"
        ))
        .bind(skill_name)
        .bind(batch_hash)
        .bind(now)
        .bind(recommendation)
        .bind(draft_skill_name)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::time::sleep;

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

        let versions: (i64,) = zeph_db::query_as(sql!(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='skill_versions'"
        ))
        .fetch_one(pool)
        .await
        .unwrap();
        assert_eq!(versions.0, 1);

        let outcomes: (i64,) = zeph_db::query_as(sql!(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='skill_outcomes'"
        ))
        .fetch_one(pool)
        .await
        .unwrap();
        assert_eq!(outcomes.0, 1);
    }

    #[tokio::test]
    async fn record_skill_outcome_inserts() {
        let store = test_store().await;

        store
            .record_skill_outcome(
                "git",
                None,
                Some(crate::types::ConversationId(1)),
                "success",
                None,
                None,
            )
            .await
            .unwrap();
        store
            .record_skill_outcome(
                "git",
                None,
                Some(crate::types::ConversationId(1)),
                "tool_failure",
                Some("exit code 1"),
                None,
            )
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
            .record_skill_outcome("git", None, None, "success", None, None)
            .await
            .unwrap();
        store
            .record_skill_outcome("git", None, None, "tool_failure", None, None)
            .await
            .unwrap();
        store
            .record_skill_outcome("docker", None, None, "success", None, None)
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
                Some(crate::types::ConversationId(1)),
                "tool_failure",
                Some("container not found"),
                None,
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

    #[tokio::test]
    async fn record_skill_outcomes_batch_resolves_version_id() {
        let store = test_store().await;

        let vid = store
            .save_skill_version("git", 1, "body", "desc", "manual", None, None)
            .await
            .unwrap();
        store.activate_skill_version("git", vid).await.unwrap();

        store
            .record_skill_outcomes_batch(
                &["git".to_string()],
                None,
                "tool_failure",
                Some("exit code 1"),
                Some("exit_nonzero"),
            )
            .await
            .unwrap();

        let pool = store.pool();
        let row: (Option<i64>, Option<String>) = zeph_db::query_as(sql!(
            "SELECT version_id, outcome_detail FROM skill_outcomes WHERE skill_name = 'git' LIMIT 1"
        ))
        .fetch_one(pool)
        .await
        .unwrap();
        assert_eq!(
            row.0,
            Some(vid),
            "version_id should be resolved to active version"
        );
        assert_eq!(row.1.as_deref(), Some("exit_nonzero"));
    }

    #[tokio::test]
    async fn record_skill_outcome_stores_outcome_detail() {
        let store = test_store().await;

        store
            .record_skill_outcome("docker", None, None, "tool_failure", None, Some("timeout"))
            .await
            .unwrap();

        let pool = store.pool();
        let row: (Option<String>,) = zeph_db::query_as(sql!(
            "SELECT outcome_detail FROM skill_outcomes WHERE skill_name = 'docker' LIMIT 1"
        ))
        .fetch_one(pool)
        .await
        .unwrap();
        assert_eq!(row.0.as_deref(), Some("timeout"));
    }

    #[tokio::test]
    async fn record_skill_outcomes_batch_waits_for_active_writer() {
        let file = tempfile::NamedTempFile::new().expect("tempfile");
        let path = file.path().to_str().expect("valid path").to_owned();
        let store = SqliteStore::with_pool_size(&path, 2)
            .await
            .expect("with_pool_size");

        let vid = store
            .save_skill_version("git", 1, "body", "desc", "manual", None, None)
            .await
            .unwrap();
        store.activate_skill_version("git", vid).await.unwrap();

        let mut writer_tx = begin_write(store.pool()).await.expect("begin immediate");
        zeph_db::query(sql!("INSERT INTO conversations DEFAULT VALUES"))
            .execute(&mut *writer_tx)
            .await
            .expect("hold write lock");

        let batch_store = store.clone();
        let batch_fut = async move {
            batch_store
                .record_skill_outcomes_batch(
                    &["git".to_string()],
                    None,
                    "success",
                    None,
                    Some("waited_for_writer"),
                )
                .await
        };
        let batch = tokio::spawn(batch_fut); // EXEMPT: test-only concurrent writer to exercise BEGIN IMMEDIATE serialization

        sleep(Duration::from_millis(100)).await;
        writer_tx.commit().await.expect("commit writer");

        batch
            .await
            .expect("join batch task")
            .expect("record outcomes");

        let count: i64 = zeph_db::query_scalar(sql!(
            "SELECT COUNT(*) FROM skill_outcomes WHERE skill_name = 'git'"
        ))
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert_eq!(
            count, 1,
            "expected batch insert to succeed after writer commits"
        );
    }

    #[tokio::test]
    async fn distinct_session_count_empty() {
        let store = test_store().await;
        let count = store.distinct_session_count("unknown-skill").await.unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn distinct_session_count_single_session() {
        let store = test_store().await;
        let cid = crate::types::ConversationId(1);
        store
            .record_skill_outcome("git", None, Some(cid), "success", None, None)
            .await
            .unwrap();
        store
            .record_skill_outcome("git", None, Some(cid), "tool_failure", None, None)
            .await
            .unwrap();
        let count = store.distinct_session_count("git").await.unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn distinct_session_count_multiple_sessions() {
        let store = test_store().await;
        for i in 0..3i64 {
            store
                .record_skill_outcome(
                    "git",
                    None,
                    Some(crate::types::ConversationId(i)),
                    "success",
                    None,
                    None,
                )
                .await
                .unwrap();
        }
        let count = store.distinct_session_count("git").await.unwrap();
        assert_eq!(count, 3);
    }

    #[tokio::test]
    async fn distinct_session_count_null_conversation_ids_excluded() {
        let store = test_store().await;
        // Insert outcomes with NULL conversation_id (legacy rows).
        store
            .record_skill_outcome("git", None, None, "success", None, None)
            .await
            .unwrap();
        store
            .record_skill_outcome("git", None, None, "success", None, None)
            .await
            .unwrap();
        let count = store.distinct_session_count("git").await.unwrap();
        assert_eq!(count, 0, "NULL conversation_ids must not be counted");
    }

    // --- STEM: skill_usage_log ---

    #[tokio::test]
    async fn insert_and_find_recurring_patterns() {
        let store = test_store().await;
        let seq = r#"["shell","web_scrape"]"#;
        let hash = "abcdef0123456789";
        let ctx = "ctxhash000000000";

        for _ in 0..3 {
            store
                .insert_tool_usage_log(seq, hash, ctx, "success", None)
                .await
                .unwrap();
        }
        store
            .insert_tool_usage_log(seq, hash, ctx, "failure", None)
            .await
            .unwrap();

        let patterns = store.find_recurring_patterns(3, 90).await.unwrap();
        assert_eq!(patterns.len(), 1);
        let (s, h, occ, suc) = &patterns[0];
        assert_eq!(s, seq);
        assert_eq!(h, hash);
        assert_eq!(*occ, 4);
        assert_eq!(*suc, 3);
    }

    #[tokio::test]
    async fn find_recurring_patterns_below_threshold_returns_empty() {
        let store = test_store().await;
        let seq = r#"["shell"]"#;
        let hash = "0000000000000001";
        let ctx = "0000000000000001";

        store
            .insert_tool_usage_log(seq, hash, ctx, "success", None)
            .await
            .unwrap();

        let patterns = store.find_recurring_patterns(3, 90).await.unwrap();
        assert!(patterns.is_empty());
    }

    #[tokio::test]
    async fn prune_tool_usage_log_removes_old_rows() {
        let store = test_store().await;
        // Insert a row with an artificially old timestamp so it falls within 0-day window.
        zeph_db::query(sql!(
            "INSERT INTO skill_usage_log \
             (tool_sequence, sequence_hash, context_hash, outcome, created_at) \
             VALUES (?, ?, ?, ?, datetime('now', '-2 days'))"
        ))
        .bind(r#"["shell"]"#)
        .bind("hash0000000000001")
        .bind("ctx00000000000001")
        .bind("success")
        .execute(store.pool())
        .await
        .unwrap();

        // Prune rows older than 1 day — the row above is 2 days old so it must be removed.
        let removed = store.prune_tool_usage_log(1).await.unwrap();
        assert_eq!(removed, 1);
    }

    // --- ERL: skill_heuristics ---

    #[tokio::test]
    async fn insert_and_load_skill_heuristics() {
        let store = test_store().await;

        let id = store
            .insert_skill_heuristic(Some("git"), "always commit in small chunks", 0.9)
            .await
            .unwrap();
        assert!(id > 0);

        let rows = store.load_skill_heuristics("git", 0.5, 10).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1, "always commit in small chunks");
        assert!((rows[0].2 - 0.9).abs() < 1e-6);
    }

    #[tokio::test]
    async fn load_skill_heuristics_includes_general() {
        let store = test_store().await;

        store
            .insert_skill_heuristic(None, "general tip", 0.7)
            .await
            .unwrap();
        store
            .insert_skill_heuristic(Some("git"), "git tip", 0.8)
            .await
            .unwrap();

        // querying for "git" should include both the git-specific and the general (NULL) heuristic
        let rows = store.load_skill_heuristics("git", 0.5, 10).await.unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[tokio::test]
    async fn load_skill_heuristics_filters_by_min_confidence() {
        let store = test_store().await;

        store
            .insert_skill_heuristic(Some("git"), "low confidence tip", 0.3)
            .await
            .unwrap();
        store
            .insert_skill_heuristic(Some("git"), "high confidence tip", 0.9)
            .await
            .unwrap();

        let rows = store.load_skill_heuristics("git", 0.5, 10).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1, "high confidence tip");
    }

    #[tokio::test]
    async fn increment_heuristic_use_count_works() {
        let store = test_store().await;

        let id = store
            .insert_skill_heuristic(Some("git"), "tip", 0.8)
            .await
            .unwrap();

        store.increment_heuristic_use_count(id).await.unwrap();
        store.increment_heuristic_use_count(id).await.unwrap();

        let rows = store.load_skill_heuristics("git", 0.0, 10).await.unwrap();
        assert_eq!(rows[0].3, 2); // use_count
    }

    #[tokio::test]
    async fn load_all_heuristics_for_skill_exact_match() {
        let store = test_store().await;

        store
            .insert_skill_heuristic(Some("git"), "git tip", 0.8)
            .await
            .unwrap();
        store
            .insert_skill_heuristic(Some("docker"), "docker tip", 0.8)
            .await
            .unwrap();

        let rows = store
            .load_all_heuristics_for_skill(Some("git"))
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1, "git tip");
    }

    #[tokio::test]
    async fn load_all_heuristics_for_skill_null() {
        let store = test_store().await;

        store
            .insert_skill_heuristic(None, "general", 0.8)
            .await
            .unwrap();
        store
            .insert_skill_heuristic(Some("git"), "git tip", 0.8)
            .await
            .unwrap();

        let rows = store.load_all_heuristics_for_skill(None).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1, "general");
    }

    #[tokio::test]
    async fn skill_trust_default_is_quarantined() {
        // Verify the DB schema default for skill_trust.trust_level is 'quarantined'.
        // ARISE-generated versions do not call set_skill_trust_level, so they inherit
        // this default when the trust row is first created by the scanner.
        let store = test_store().await;

        // Insert a trust row without specifying trust_level to exercise the DEFAULT.
        zeph_db::query(sql!(
            "INSERT INTO skill_trust (skill_name, blake3_hash) VALUES ('test-arise', 'abc')"
        ))
        .execute(store.pool())
        .await
        .unwrap();

        let trust: (String,) = zeph_db::query_as(sql!(
            "SELECT trust_level FROM skill_trust WHERE skill_name = 'test-arise'"
        ))
        .fetch_one(store.pool())
        .await
        .unwrap();

        assert_eq!(
            trust.0, "quarantined",
            "schema default for skill_trust.trust_level must be 'quarantined'"
        );
    }

    #[tokio::test]
    async fn save_and_activate_skill_version_activates_new() {
        let store = test_store().await;

        let id = store
            .save_and_activate_skill_version(
                "git",
                1,
                "body v1",
                "Git helper",
                "manual",
                None,
                None,
            )
            .await
            .unwrap();
        assert!(id > 0);

        let active = store.active_skill_version("git").await.unwrap().unwrap();
        assert_eq!(active.id, id);
        assert_eq!(active.version, 1);
        assert_eq!(active.body, "body v1");
        assert!(active.is_active);
    }

    #[tokio::test]
    async fn save_and_activate_skill_version_deactivates_previous() {
        let store = test_store().await;

        let v1 = store
            .save_and_activate_skill_version("git", 1, "v1", "desc", "manual", None, None)
            .await
            .unwrap();

        let v2 = store
            .save_and_activate_skill_version("git", 2, "v2", "desc", "auto", None, Some(v1))
            .await
            .unwrap();

        let versions = store.load_skill_versions("git").await.unwrap();
        assert_eq!(versions.len(), 2);

        let old = versions.iter().find(|v| v.id == v1).unwrap();
        let new = versions.iter().find(|v| v.id == v2).unwrap();
        assert!(!old.is_active, "previous version must be deactivated");
        assert!(new.is_active, "new version must be active");
    }

    #[tokio::test]
    async fn save_and_activate_skill_version_only_one_active() {
        let store = test_store().await;

        let mut last_id = 0i64;
        for i in 1i64..=4 {
            last_id = store
                .save_and_activate_skill_version(
                    "git",
                    i,
                    &format!("v{i}"),
                    "desc",
                    "auto",
                    None,
                    None,
                )
                .await
                .unwrap();
        }

        let versions = store.load_skill_versions("git").await.unwrap();
        let active_count = versions.iter().filter(|v| v.is_active).count();
        assert_eq!(active_count, 1, "exactly one version must be active");
        assert_eq!(
            versions.iter().find(|v| v.is_active).unwrap().id,
            last_id,
            "the last saved version must be the active one"
        );
    }

    // ── AutoSkill A6: heuristic promotion helpers ─────────────────────────

    async fn insert_heuristic(store: &SqliteStore, skill: &str, text: &str, confidence: f64) {
        zeph_db::query(sql!(
            "INSERT INTO skill_heuristics (skill_name, heuristic_text, confidence) VALUES (?, ?, ?)"
        ))
        .bind(skill)
        .bind(text)
        .bind(confidence)
        .execute(store.pool())
        .await
        .unwrap();
    }

    /// Regression test for #4531: `NULL` `skill_name` heuristics must be excluded from
    /// `count_heuristics_by_skill` so sqlx does not encounter a non-null column mismatch.
    #[tokio::test]
    async fn count_heuristics_by_skill_excludes_null_skill_name() {
        let store = test_store().await;

        // Insert heuristics with NULL skill_name (general/unattributed).
        store
            .insert_skill_heuristic(None, "general tip 1", 0.9)
            .await
            .unwrap();
        store
            .insert_skill_heuristic(None, "general tip 2", 0.8)
            .await
            .unwrap();
        store
            .insert_skill_heuristic(None, "general tip 3", 0.7)
            .await
            .unwrap();

        // NULL rows alone must not produce any results (no skill_name to group by).
        let results = store.count_heuristics_by_skill(0.5, 2).await.unwrap();
        assert!(
            results.is_empty(),
            "NULL skill_name heuristics must be excluded from count_heuristics_by_skill results"
        );

        // Verify that named skills are still counted correctly alongside NULL rows.
        insert_heuristic(&store, "git", "use --no-pager", 0.8).await;
        insert_heuristic(&store, "git", "check status first", 0.9).await;

        let results = store.count_heuristics_by_skill(0.5, 2).await.unwrap();
        assert_eq!(results.len(), 1, "only git should qualify");
        assert_eq!(results[0].0, "git");
        assert_eq!(results[0].1, 2, "git has exactly 2 qualifying heuristics");
    }

    #[tokio::test]
    async fn count_heuristics_by_skill_threshold_and_confidence() {
        let store = test_store().await;

        // "git" has 3 heuristics: 2 above min_confidence, 1 below
        insert_heuristic(&store, "git", "use --no-pager", 0.8).await;
        insert_heuristic(&store, "git", "check status first", 0.9).await;
        insert_heuristic(&store, "git", "low confidence hint", 0.3).await;

        // "docker" has 1 heuristic above confidence — below min_count=2
        insert_heuristic(&store, "docker", "use --rm", 0.7).await;

        let results = store.count_heuristics_by_skill(0.5, 2).await.unwrap();
        assert_eq!(results.len(), 1, "only git meets threshold");
        assert_eq!(results[0].0, "git");
        assert_eq!(results[0].1, 2, "only heuristics >= 0.5 confidence counted");
    }

    #[tokio::test]
    async fn load_heuristic_texts_sorted_for_deterministic_hash() {
        let store = test_store().await;

        insert_heuristic(&store, "git", "zebra", 0.8).await;
        insert_heuristic(&store, "git", "alpha", 0.9).await;
        insert_heuristic(&store, "git", "middle", 0.7).await;
        insert_heuristic(&store, "git", "skipped", 0.2).await; // below min_confidence

        let texts = store
            .load_heuristic_texts_for_promotion("git", 0.5)
            .await
            .unwrap();

        assert_eq!(texts.len(), 3);
        assert_eq!(
            texts,
            vec!["alpha", "middle", "zebra"],
            "must be sorted ASC"
        );
    }

    #[tokio::test]
    async fn promotion_already_evaluated_idempotency() {
        let store = test_store().await;

        // Not yet evaluated.
        let found = store
            .promotion_already_evaluated("git", "abc123")
            .await
            .unwrap();
        assert!(!found);

        store
            .record_promotion_evaluation("git", "abc123", "none", None)
            .await
            .unwrap();

        // Now it should be found.
        let found = store
            .promotion_already_evaluated("git", "abc123")
            .await
            .unwrap();
        assert!(found);
    }

    #[tokio::test]
    async fn record_promotion_evaluation_insert_or_ignore() {
        let store = test_store().await;

        // First insert — succeeds.
        store
            .record_promotion_evaluation("git", "hash1", "body_enrichment", Some("git-v2"))
            .await
            .unwrap();

        // Duplicate insert — should not error (INSERT OR IGNORE).
        store
            .record_promotion_evaluation("git", "hash1", "new_skill", Some("git-extra"))
            .await
            .unwrap();

        // Only one row should exist; recommendation is from the first insert.
        let rec: (String,) = zeph_db::query_as(sql!(
            "SELECT recommendation FROM skill_heuristic_promotions WHERE skill_name = ? AND batch_hash = ?"
        ))
        .bind("git")
        .bind("hash1")
        .fetch_one(store.pool())
        .await
        .unwrap();
        assert_eq!(
            rec.0, "body_enrichment",
            "first insert wins (INSERT OR IGNORE)"
        );
    }
}
