// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use zeph_db::{query, query_as, query_scalar, sql};

use super::DbStore;
use crate::error::MemoryError;

/// A single persona fact row from the `persona_memory` table.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct PersonaFactRow {
    pub id: i64,
    pub category: String,
    pub content: String,
    pub confidence: f64,
    pub evidence_count: i64,
    pub source_conversation_id: Option<i64>,
    pub supersedes_id: Option<i64>,
    pub created_at: String,
    pub updated_at: String,
}

impl DbStore {
    /// Upsert a persona fact.
    ///
    /// On exact-content conflict within the same category: increments `evidence_count`
    /// and updates `confidence` and `updated_at`.
    ///
    /// When `supersedes_id` is provided, the referenced older fact is logically
    /// replaced — it will be excluded from context assembly via the NOT IN filter.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn upsert_persona_fact(
        &self,
        category: &str,
        content: &str,
        confidence: f64,
        source_conversation_id: Option<i64>,
        supersedes_id: Option<i64>,
    ) -> Result<i64, MemoryError> {
        let (id,): (i64,) = query_as(sql!(
            "INSERT INTO persona_memory
                (category, content, confidence, evidence_count, source_conversation_id,
                 supersedes_id, updated_at)
             VALUES
                (?, ?, ?, 1, ?, ?, datetime('now'))
             ON CONFLICT(category, content) DO UPDATE SET
                evidence_count = evidence_count + 1,
                confidence     = excluded.confidence,
                supersedes_id  = COALESCE(excluded.supersedes_id, persona_memory.supersedes_id),
                updated_at     = datetime('now')
             RETURNING id"
        ))
        .bind(category)
        .bind(content)
        .bind(confidence)
        .bind(source_conversation_id)
        .bind(supersedes_id)
        .fetch_one(self.pool())
        .await?;

        Ok(id)
    }

    /// Load all persona facts above `min_confidence`, excluding superseded facts.
    ///
    /// Results are ordered by confidence DESC so the most reliable facts come first.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn load_persona_facts(
        &self,
        min_confidence: f64,
    ) -> Result<Vec<PersonaFactRow>, MemoryError> {
        // Facts that appear in any other row's supersedes_id column are excluded:
        // they have been replaced by a newer, contradicting fact.
        let rows: Vec<PersonaFactRow> = query_as(sql!(
            "SELECT id, category, content, confidence, evidence_count,
                    source_conversation_id, supersedes_id, created_at, updated_at
             FROM persona_memory
             WHERE confidence >= ?
               AND id NOT IN (
                   SELECT supersedes_id FROM persona_memory
                   WHERE supersedes_id IS NOT NULL
               )
             ORDER BY confidence DESC"
        ))
        .bind(min_confidence)
        .fetch_all(self.pool())
        .await?;

        Ok(rows)
    }

    /// Delete a persona fact by id (for user-initiated corrections).
    ///
    /// Returns `true` if a row was deleted, `false` if the id was not found.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn delete_persona_fact(&self, id: i64) -> Result<bool, MemoryError> {
        let affected = query(sql!("DELETE FROM persona_memory WHERE id = ?"))
            .bind(id)
            .execute(self.pool())
            .await?
            .rows_affected();

        Ok(affected > 0)
    }

    /// Count total persona facts (for metrics/TUI).
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn count_persona_facts(&self) -> Result<i64, MemoryError> {
        let count: i64 = query_scalar(sql!("SELECT COUNT(*) FROM persona_memory"))
            .fetch_one(self.pool())
            .await?;

        Ok(count)
    }

    /// Read the last extracted message id from the `persona_meta` singleton.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn persona_last_extracted_message_id(&self) -> Result<i64, MemoryError> {
        let id: i64 = query_scalar(sql!(
            "SELECT last_extracted_message_id FROM persona_meta WHERE id = 1"
        ))
        .fetch_one(self.pool())
        .await?;

        Ok(id)
    }

    /// Update the last extracted message id in the `persona_meta` singleton.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn set_persona_last_extracted_message_id(
        &self,
        message_id: i64,
    ) -> Result<(), MemoryError> {
        query(sql!(
            "UPDATE persona_meta
             SET last_extracted_message_id = ?, updated_at = datetime('now')
             WHERE id = 1"
        ))
        .bind(message_id)
        .execute(self.pool())
        .await?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn make_store() -> DbStore {
        DbStore::with_pool_size(":memory:", 1)
            .await
            .expect("in-memory store")
    }

    #[tokio::test]
    async fn upsert_persona_fact_basic_insert() {
        let store = make_store().await;
        let id = store
            .upsert_persona_fact("preference", "I prefer dark mode", 0.9, None, None)
            .await
            .expect("upsert");
        assert!(id > 0);
        assert_eq!(store.count_persona_facts().await.expect("count"), 1);
    }

    #[tokio::test]
    async fn upsert_persona_fact_increments_evidence_count() {
        let store = make_store().await;
        let id1 = store
            .upsert_persona_fact("preference", "I prefer dark mode", 0.9, None, None)
            .await
            .expect("first upsert");
        let id2 = store
            .upsert_persona_fact("preference", "I prefer dark mode", 0.95, None, None)
            .await
            .expect("second upsert");
        // Same row on conflict — same id returned.
        assert_eq!(id1, id2);

        let facts = store.load_persona_facts(0.0).await.expect("load");
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].evidence_count, 2);
        // Confidence updated to the latest value.
        assert!((facts[0].confidence - 0.95).abs() < 1e-9);
    }

    #[tokio::test]
    async fn upsert_persona_fact_supersedes_id_propagated() {
        let store = make_store().await;
        let old_id = store
            .upsert_persona_fact("preference", "I prefer light mode", 0.8, None, None)
            .await
            .expect("old fact");

        let _new_id = store
            .upsert_persona_fact("preference", "I prefer dark mode", 0.9, None, Some(old_id))
            .await
            .expect("new fact");

        // Old fact should be excluded because it appears in another row's supersedes_id.
        let facts = store.load_persona_facts(0.0).await.expect("load");
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].content, "I prefer dark mode");
    }

    #[tokio::test]
    async fn load_persona_facts_excludes_superseded() {
        let store = make_store().await;
        let old_id = store
            .upsert_persona_fact("domain_knowledge", "I know Python", 0.7, None, None)
            .await
            .expect("old");
        store
            .upsert_persona_fact(
                "domain_knowledge",
                "I know Python and Rust",
                0.85,
                None,
                Some(old_id),
            )
            .await
            .expect("new");

        let facts = store.load_persona_facts(0.0).await.expect("load");
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].content, "I know Python and Rust");
    }

    #[tokio::test]
    async fn load_persona_facts_min_confidence_filter() {
        let store = make_store().await;
        store
            .upsert_persona_fact("background", "Senior engineer", 0.9, None, None)
            .await
            .expect("high confidence");
        store
            .upsert_persona_fact("background", "Works remotely", 0.3, None, None)
            .await
            .expect("low confidence");

        let facts = store.load_persona_facts(0.5).await.expect("load");
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].content, "Senior engineer");
    }

    #[tokio::test]
    async fn delete_persona_fact_returns_true_when_found() {
        let store = make_store().await;
        let id = store
            .upsert_persona_fact("working_style", "I prefer async comms", 0.8, None, None)
            .await
            .expect("upsert");
        let deleted = store.delete_persona_fact(id).await.expect("delete");
        assert!(deleted);
        assert_eq!(store.count_persona_facts().await.expect("count"), 0);
    }

    #[tokio::test]
    async fn delete_persona_fact_returns_false_when_not_found() {
        let store = make_store().await;
        let deleted = store.delete_persona_fact(9999).await.expect("delete");
        assert!(!deleted);
    }

    #[tokio::test]
    async fn count_persona_facts_is_zero_initially() {
        let store = make_store().await;
        assert_eq!(store.count_persona_facts().await.expect("count"), 0);
    }

    #[tokio::test]
    async fn persona_meta_singleton_initial_value() {
        let store = make_store().await;
        let id = store
            .persona_last_extracted_message_id()
            .await
            .expect("meta");
        assert_eq!(id, 0);
    }

    #[tokio::test]
    async fn set_persona_last_extracted_message_id_round_trip() {
        let store = make_store().await;
        store
            .set_persona_last_extracted_message_id(42)
            .await
            .expect("set");
        let id = store
            .persona_last_extracted_message_id()
            .await
            .expect("get");
        assert_eq!(id, 42);
    }
}
