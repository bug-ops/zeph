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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{CompressionRule, CompressionRuleStore};

    async fn make_store() -> (CompressionRuleStore, sqlx::SqlitePool) {
        let pool = sqlx::SqlitePool::connect(":memory:").await.unwrap();
        sqlx::query(
            "CREATE TABLE compression_rules (\
             id TEXT PRIMARY KEY, tool_glob TEXT, pattern TEXT NOT NULL, \
             replacement_template TEXT NOT NULL, hit_count INTEGER NOT NULL DEFAULT 0, \
             source TEXT NOT NULL DEFAULT 'operator', created_at TEXT NOT NULL, \
             UNIQUE(tool_glob, pattern))",
        )
        .execute(&pool)
        .await
        .unwrap();
        let store = CompressionRuleStore::new(Arc::new(pool.clone()));
        (store, pool)
    }

    fn rule(
        id: &str,
        tool_glob: Option<&str>,
        pattern: &str,
        replacement: &str,
        hits: i64,
        source: &str,
    ) -> CompressionRule {
        CompressionRule {
            id: id.to_owned(),
            tool_glob: tool_glob.map(ToOwned::to_owned),
            pattern: pattern.to_owned(),
            replacement_template: replacement.to_owned(),
            hit_count: hits,
            source: source.to_owned(),
            created_at: "2026-01-01T00:00:00Z".to_owned(),
        }
    }

    // --- list_active ---

    #[tokio::test]
    async fn list_active_empty() {
        let (store, _pool) = make_store().await;
        let rules = store.list_active().await.unwrap();
        assert!(rules.is_empty());
    }

    #[tokio::test]
    async fn list_active_returns_ordered_by_hits_asc() {
        // Distinct hit counts are intentional: ORDER BY hit_count ASC has no tiebreaker,
        // so equal counts would produce non-deterministic ordering.
        let (store, _pool) = make_store().await;
        store
            .upsert(&rule("a", None, "pa", "ra", 10, "operator"))
            .await
            .unwrap();
        store
            .upsert(&rule("b", None, "pb", "rb", 0, "operator"))
            .await
            .unwrap();
        store
            .upsert(&rule("c", None, "pc", "rc", 5, "operator"))
            .await
            .unwrap();

        let rules = store.list_active().await.unwrap();
        assert_eq!(rules.len(), 3);
        assert_eq!(rules[0].hit_count, 0);
        assert_eq!(rules[1].hit_count, 5);
        assert_eq!(rules[2].hit_count, 10);
    }

    // --- upsert ---

    #[tokio::test]
    async fn upsert_inserts_new_rule() {
        let (store, _pool) = make_store().await;
        store
            .upsert(&rule("r1", Some("shell"), "pat", "tmpl", 0, "operator"))
            .await
            .unwrap();

        let rules = store.list_active().await.unwrap();
        assert_eq!(rules.len(), 1);
        let r = &rules[0];
        assert_eq!(r.id, "r1");
        assert_eq!(r.tool_glob.as_deref(), Some("shell"));
        assert_eq!(r.pattern, "pat");
        assert_eq!(r.replacement_template, "tmpl");
        assert_eq!(r.source, "operator");
    }

    #[tokio::test]
    async fn upsert_conflict_updates_template_and_source() {
        // Exercises the common-case conflict path where tool_glob = Some("shell").
        let (store, _pool) = make_store().await;
        store
            .upsert(&rule("r1", Some("shell"), "pat", "old-tmpl", 5, "operator"))
            .await
            .unwrap();
        store
            .upsert(&rule(
                "r2",
                Some("shell"),
                "pat",
                "new-tmpl",
                0,
                "llm-evolved",
            ))
            .await
            .unwrap();

        let rules = store.list_active().await.unwrap();
        assert_eq!(rules.len(), 1);
        // id must be preserved from the original insert, not overwritten by ON CONFLICT
        assert_eq!(rules[0].id, "r1");
        assert_eq!(rules[0].replacement_template, "new-tmpl");
        assert_eq!(rules[0].source, "llm-evolved");
        // hit_count must not be overwritten by ON CONFLICT
        assert_eq!(rules[0].hit_count, 5);
    }

    #[tokio::test]
    async fn upsert_null_tool_glob_distinct() {
        // SQLite treats each NULL as distinct in UNIQUE constraints: (NULL, "pat") and
        // (NULL, "pat") are NOT considered equal, so both rows can coexist even though
        // they share the same pattern. This is the key SQLite NULL-in-UNIQUE behavior.
        let (store, _pool) = make_store().await;
        store
            .upsert(&rule("r1", None, "same-pat", "ra", 0, "operator"))
            .await
            .unwrap();
        store
            .upsert(&rule("r2", None, "same-pat", "rb", 0, "operator"))
            .await
            .unwrap();

        let rules = store.list_active().await.unwrap();
        assert_eq!(rules.len(), 2);
    }

    #[tokio::test]
    async fn upsert_preserves_hit_count_on_conflict() {
        // Use non-NULL tool_glob so the UNIQUE(tool_glob, pattern) constraint fires.
        // NULL tool_glob is treated as distinct by SQLite (each NULL ≠ NULL in UNIQUE),
        // so (NULL, "pat") never produces a conflict — only non-NULL values do.
        let (store, _pool) = make_store().await;
        store
            .upsert(&rule("r1", Some("shell"), "pat", "tmpl", 5, "operator"))
            .await
            .unwrap();
        // Same key (Some("shell"), "pat"), but hit_count=0 in the new row.
        store
            .upsert(&rule("r2", Some("shell"), "pat", "tmpl2", 0, "operator"))
            .await
            .unwrap();

        let rules = store.list_active().await.unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(
            rules[0].hit_count, 5,
            "hit_count must not be reset by ON CONFLICT"
        );
    }

    // --- increment_hits ---

    #[tokio::test]
    async fn increment_hits_single() {
        let (store, _pool) = make_store().await;
        store
            .upsert(&rule("r1", None, "pat", "tmpl", 0, "operator"))
            .await
            .unwrap();

        store.increment_hits(&[("r1".to_owned(), 3)]).await.unwrap();

        let rules = store.list_active().await.unwrap();
        assert_eq!(rules[0].hit_count, 3);
    }

    #[tokio::test]
    async fn increment_hits_batch() {
        let (store, _pool) = make_store().await;
        store
            .upsert(&rule("r1", None, "p1", "t1", 0, "operator"))
            .await
            .unwrap();
        store
            .upsert(&rule("r2", None, "p2", "t2", 10, "operator"))
            .await
            .unwrap();
        store
            .upsert(&rule("r3", None, "p3", "t3", 0, "operator"))
            .await
            .unwrap();

        store
            .increment_hits(&[
                ("r1".to_owned(), 2),
                ("r2".to_owned(), 5),
                ("r3".to_owned(), 1),
            ])
            .await
            .unwrap();

        let rules = store.list_active().await.unwrap();
        let by_id = |id: &str| rules.iter().find(|r| r.id == id).unwrap().hit_count;
        assert_eq!(by_id("r1"), 2);
        assert_eq!(by_id("r2"), 15);
        assert_eq!(by_id("r3"), 1);
    }

    #[tokio::test]
    async fn increment_hits_nonexistent_id() {
        let (store, _pool) = make_store().await;
        // UPDATE WHERE id = 'ghost' matches 0 rows — must succeed silently.
        store
            .increment_hits(&[("ghost".to_owned(), 1)])
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn increment_hits_empty_batch() {
        let (store, _pool) = make_store().await;
        store
            .upsert(&rule("r1", None, "pat", "tmpl", 7, "operator"))
            .await
            .unwrap();

        store.increment_hits(&[]).await.unwrap();

        let rules = store.list_active().await.unwrap();
        assert_eq!(
            rules[0].hit_count, 7,
            "empty batch must not modify existing rules"
        );
    }

    // --- delete ---

    #[tokio::test]
    async fn delete_removes_rule() {
        let (store, _pool) = make_store().await;
        store
            .upsert(&rule("r1", None, "pat", "tmpl", 0, "operator"))
            .await
            .unwrap();

        store.delete("r1").await.unwrap();

        let rules = store.list_active().await.unwrap();
        assert!(rules.is_empty());
    }

    #[tokio::test]
    async fn delete_nonexistent_is_noop() {
        let (store, _pool) = make_store().await;
        // Must succeed even when no row with this ID exists.
        store.delete("ghost").await.unwrap();
    }

    // --- prune_lowest_hits ---

    #[tokio::test]
    async fn prune_fast_path_no_deletion() {
        let (store, _pool) = make_store().await;
        store
            .upsert(&rule("r1", None, "p1", "t1", 1, "operator"))
            .await
            .unwrap();
        store
            .upsert(&rule("r2", None, "p2", "t2", 2, "operator"))
            .await
            .unwrap();

        let deleted = store.prune_lowest_hits(5).await.unwrap();
        assert_eq!(deleted, 0);
        assert_eq!(store.list_active().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn prune_deletes_lowest_hit_rules() {
        let (store, _pool) = make_store().await;
        for (i, hits) in [1i64, 2, 3, 4, 5].iter().enumerate() {
            store
                .upsert(&rule(
                    &format!("r{i}"),
                    None,
                    &format!("p{i}"),
                    "t",
                    *hits,
                    "operator",
                ))
                .await
                .unwrap();
        }

        let deleted = store.prune_lowest_hits(3).await.unwrap();
        assert_eq!(deleted, 2);

        let remaining = store.list_active().await.unwrap();
        assert_eq!(remaining.len(), 3);
        assert!(remaining.iter().all(|r| r.hit_count >= 3));
    }

    #[tokio::test]
    async fn prune_exact_boundary() {
        let (store, _pool) = make_store().await;
        store
            .upsert(&rule("r1", None, "p1", "t1", 1, "operator"))
            .await
            .unwrap();
        store
            .upsert(&rule("r2", None, "p2", "t2", 2, "operator"))
            .await
            .unwrap();
        store
            .upsert(&rule("r3", None, "p3", "t3", 3, "operator"))
            .await
            .unwrap();

        // count == max_rules → fast path, 0 deleted
        let deleted = store.prune_lowest_hits(3).await.unwrap();
        assert_eq!(deleted, 0);
        assert_eq!(store.list_active().await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn prune_max_rules_zero_deletes_all() {
        let (store, _pool) = make_store().await;
        store
            .upsert(&rule("r1", None, "p1", "t1", 1, "operator"))
            .await
            .unwrap();
        store
            .upsert(&rule("r2", None, "p2", "t2", 2, "operator"))
            .await
            .unwrap();
        store
            .upsert(&rule("r3", None, "p3", "t3", 3, "operator"))
            .await
            .unwrap();

        let deleted = store.prune_lowest_hits(0).await.unwrap();
        assert_eq!(deleted, 3);
        assert!(store.list_active().await.unwrap().is_empty());
    }
}
