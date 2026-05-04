// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! SQLite/Postgres-backed storage for TACO compression rules.

use std::sync::Arc;

use zeph_db::DbPool;

/// A single compression rule stored in the database.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct CompressionRule {
    /// UUID v4 string identifier.
    pub id: String,
    /// Optional glob pattern matching tool names (e.g., `"shell"`, `"web_*"`).
    pub tool_glob: Option<String>,
    /// Regex pattern applied to tool output.
    pub pattern: String,
    /// Replacement template (may reference capture groups, e.g. `"$1"`).
    pub replacement_template: String,
    /// Number of times this rule has matched. Updated by [`CompressionRuleStore::increment_hits`].
    pub hit_count: i64,
    /// Origin of this rule: `"operator"` (config-inserted) or `"llm-evolved"` (auto-generated).
    pub source: String,
    /// RFC 3339 creation timestamp.
    pub created_at: String,
}

/// Persistence layer for TACO compression rules.
///
/// All rules are loaded at startup via [`CompressionRuleStore::list_active`] and cached in
/// [`super::RuleBasedCompressor`]. Hit counts are flushed in batches via
/// [`CompressionRuleStore::increment_hits`] during the `maybe_autodream` maintenance pass.
#[derive(Clone)]
pub struct CompressionRuleStore {
    pool: Arc<DbPool>,
}

impl CompressionRuleStore {
    /// Construct a store backed by the given pool.
    #[must_use]
    pub fn new(pool: Arc<DbPool>) -> Self {
        Self { pool }
    }

    /// Return all rules, ordered by ascending hit count (least-used first for pruning).
    ///
    /// # Errors
    ///
    /// Returns a database error on failure.
    pub async fn list_active(&self) -> Result<Vec<CompressionRule>, zeph_db::SqlxError> {
        sqlx::query_as(zeph_db::sql!(
            "SELECT id, tool_glob, pattern, replacement_template, hit_count, source, created_at \
             FROM compression_rules ORDER BY hit_count ASC"
        ))
        .fetch_all(self.pool.as_ref())
        .await
    }

    /// Insert or update a rule (keyed by `(tool_glob, pattern)`).
    ///
    /// # Errors
    ///
    /// Returns a database error on failure.
    pub async fn upsert(&self, rule: &CompressionRule) -> Result<(), zeph_db::SqlxError> {
        sqlx::query(zeph_db::sql!(
            "INSERT INTO compression_rules \
             (id, tool_glob, pattern, replacement_template, hit_count, source, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(tool_glob, pattern) DO UPDATE SET \
             replacement_template = excluded.replacement_template, \
             source = excluded.source"
        ))
        .bind(&rule.id)
        .bind(&rule.tool_glob)
        .bind(&rule.pattern)
        .bind(&rule.replacement_template)
        .bind(rule.hit_count)
        .bind(&rule.source)
        .bind(&rule.created_at)
        .execute(self.pool.as_ref())
        .await?;
        Ok(())
    }

    /// Batch-increment hit counts for a set of rule IDs.
    ///
    /// Called during the `maybe_autodream` maintenance pass. Uses individual
    /// UPDATE statements rather than a batch because the count of rules is small
    /// and cross-backend portability is preferred.
    ///
    /// # Errors
    ///
    /// Returns a database error on failure.
    pub async fn increment_hits(&self, batch: &[(String, u64)]) -> Result<(), zeph_db::SqlxError> {
        for (id, delta) in batch {
            sqlx::query(zeph_db::sql!(
                "UPDATE compression_rules SET hit_count = hit_count + ? WHERE id = ?"
            ))
            .bind((*delta).cast_signed())
            .bind(id.as_str())
            .execute(self.pool.as_ref())
            .await?;
        }
        Ok(())
    }

    /// Delete a rule by ID.
    ///
    /// # Errors
    ///
    /// Returns a database error on failure.
    pub async fn delete(&self, id: &str) -> Result<(), zeph_db::SqlxError> {
        sqlx::query(zeph_db::sql!("DELETE FROM compression_rules WHERE id = ?"))
            .bind(id)
            .execute(self.pool.as_ref())
            .await?;
        Ok(())
    }

    /// Prune the lowest-hit rules to keep the table below `max_rules`.
    ///
    /// Returns the number of rules deleted.
    ///
    /// # Errors
    ///
    /// Returns a database error on failure.
    pub async fn prune_lowest_hits(&self, max_rules: u32) -> Result<u64, zeph_db::SqlxError> {
        let count: i64 =
            sqlx::query_scalar(zeph_db::sql!("SELECT COUNT(*) FROM compression_rules"))
                .fetch_one(self.pool.as_ref())
                .await?;

        if count <= i64::from(max_rules) {
            return Ok(0);
        }

        let to_delete = count - i64::from(max_rules);
        let result = sqlx::query(zeph_db::sql!(
            "DELETE FROM compression_rules WHERE id IN \
             (SELECT id FROM compression_rules ORDER BY hit_count ASC LIMIT ?)"
        ))
        .bind(to_delete)
        .execute(self.pool.as_ref())
        .await?;

        Ok(result.rows_affected())
    }
}
