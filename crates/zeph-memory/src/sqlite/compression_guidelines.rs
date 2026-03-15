// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! SQLite-backed store for ACON compression guidelines and failure pairs.

use crate::error::MemoryError;
use crate::sqlite::SqliteStore;
use crate::types::ConversationId;

/// A recorded compression failure pair: the compressed context and the response
/// that indicated context was lost.
#[derive(Debug, Clone)]
pub struct CompressionFailurePair {
    pub id: i64,
    pub conversation_id: ConversationId,
    pub compressed_context: String,
    pub failure_reason: String,
    pub created_at: String,
}

/// Maximum characters stored per `compressed_context` or `failure_reason` field.
const MAX_FIELD_CHARS: usize = 4096;

fn truncate_field(s: &str) -> &str {
    let mut idx = MAX_FIELD_CHARS;
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    &s[..idx.min(s.len())]
}

impl SqliteStore {
    /// Load the latest active compression guidelines (global scope).
    ///
    /// Returns `(version, guidelines_text)`. Returns `(0, "")` if no guidelines exist yet.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn load_compression_guidelines(&self) -> Result<(i64, String), MemoryError> {
        let row = sqlx::query_as::<_, (i64, String)>(
            "SELECT version, guidelines FROM compression_guidelines ORDER BY version DESC LIMIT 1",
        )
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.unwrap_or((0, String::new())))
    }

    /// Save a new version of the compression guidelines (global scope).
    ///
    /// Inserts a new row; older versions are retained for audit.
    /// Returns the new version number.
    ///
    /// # Errors
    ///
    /// Returns an error if the database insert fails.
    pub async fn save_compression_guidelines(
        &self,
        guidelines: &str,
        token_count: i64,
    ) -> Result<i64, MemoryError> {
        let (current_version, _) = self.load_compression_guidelines().await?;
        let new_version = current_version + 1;
        sqlx::query(
            "INSERT INTO compression_guidelines (version, guidelines, token_count) VALUES (?, ?, ?)",
        )
        .bind(new_version)
        .bind(guidelines)
        .bind(token_count)
        .execute(&self.pool)
        .await?;
        Ok(new_version)
    }

    /// Log a compression failure pair.
    ///
    /// Both `compressed_context` and `failure_reason` are truncated to 4096 chars.
    /// Returns the inserted row id.
    ///
    /// # Errors
    ///
    /// Returns an error if the database insert fails.
    pub async fn log_compression_failure(
        &self,
        conversation_id: ConversationId,
        compressed_context: &str,
        failure_reason: &str,
    ) -> Result<i64, MemoryError> {
        let ctx = truncate_field(compressed_context);
        let reason = truncate_field(failure_reason);
        let id = sqlx::query_scalar(
            "INSERT INTO compression_failure_pairs \
             (conversation_id, compressed_context, failure_reason) \
             VALUES (?, ?, ?) RETURNING id",
        )
        .bind(conversation_id.0)
        .bind(ctx)
        .bind(reason)
        .fetch_one(&self.pool)
        .await?;
        Ok(id)
    }

    /// Get unused failure pairs (oldest first), up to `limit`.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn get_unused_failure_pairs(
        &self,
        limit: usize,
    ) -> Result<Vec<CompressionFailurePair>, MemoryError> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = sqlx::query_as::<_, (i64, i64, String, String, String)>(
            "SELECT id, conversation_id, compressed_context, failure_reason, created_at \
             FROM compression_failure_pairs \
             WHERE used_in_update = 0 \
             ORDER BY created_at ASC \
             LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(
                |(id, cid, ctx, reason, created_at)| CompressionFailurePair {
                    id,
                    conversation_id: ConversationId(cid),
                    compressed_context: ctx,
                    failure_reason: reason,
                    created_at,
                },
            )
            .collect())
    }

    /// Mark failure pairs as consumed by the updater.
    ///
    /// # Errors
    ///
    /// Returns an error if the database update fails.
    pub async fn mark_failure_pairs_used(&self, ids: &[i64]) -> Result<(), MemoryError> {
        if ids.is_empty() {
            return Ok(());
        }
        let placeholders: String = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let query = format!(
            "UPDATE compression_failure_pairs SET used_in_update = 1 WHERE id IN ({placeholders})"
        );
        let mut q = sqlx::query(&query);
        for id in ids {
            q = q.bind(id);
        }
        q.execute(&self.pool).await?;
        Ok(())
    }

    /// Count unused failure pairs.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn count_unused_failure_pairs(&self) -> Result<i64, MemoryError> {
        let count = sqlx::query_scalar(
            "SELECT COUNT(*) FROM compression_failure_pairs WHERE used_in_update = 0",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(count)
    }

    /// Delete old used failure pairs, keeping the most recent `keep_recent` unused pairs.
    ///
    /// Removes all rows where `used_in_update = 1`. Unused rows are managed by the
    /// `max_stored_pairs` enforcement below: if there are more than `keep_recent` unused pairs,
    /// the oldest excess rows are deleted.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn cleanup_old_failure_pairs(&self, keep_recent: usize) -> Result<(), MemoryError> {
        // Delete all used pairs (they've already been processed).
        sqlx::query("DELETE FROM compression_failure_pairs WHERE used_in_update = 1")
            .execute(&self.pool)
            .await?;

        // Keep only the most recent `keep_recent` unused pairs.
        let keep = i64::try_from(keep_recent).unwrap_or(i64::MAX);
        sqlx::query(
            "DELETE FROM compression_failure_pairs \
             WHERE used_in_update = 0 \
             AND id NOT IN ( \
                 SELECT id FROM compression_failure_pairs \
                 WHERE used_in_update = 0 \
                 ORDER BY created_at DESC \
                 LIMIT ? \
             )",
        )
        .bind(keep)
        .execute(&self.pool)
        .await?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn make_store() -> SqliteStore {
        let store = SqliteStore::with_pool_size(":memory:", 1)
            .await
            .expect("in-memory SqliteStore");
        store
    }

    #[tokio::test]
    async fn load_guidelines_returns_defaults_when_empty() {
        let store = make_store().await;
        let (version, text) = store.load_compression_guidelines().await.unwrap();
        assert_eq!(version, 0);
        assert!(text.is_empty());
    }

    #[tokio::test]
    async fn save_and_load_guidelines() {
        let store = make_store().await;
        let v1 = store
            .save_compression_guidelines("always preserve file paths", 4)
            .await
            .unwrap();
        assert_eq!(v1, 1);
        let v2 = store
            .save_compression_guidelines("always preserve file paths\nalways preserve errors", 8)
            .await
            .unwrap();
        assert_eq!(v2, 2);
        // Loading should return the latest version.
        let (v, text) = store.load_compression_guidelines().await.unwrap();
        assert_eq!(v, 2);
        assert!(text.contains("errors"));
    }

    #[tokio::test]
    async fn log_and_count_failure_pairs() {
        let store = make_store().await;
        let cid = ConversationId(store.create_conversation().await.unwrap().0);
        store
            .log_compression_failure(cid, "compressed ctx", "i don't recall that")
            .await
            .unwrap();
        let count = store.count_unused_failure_pairs().await.unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn get_unused_pairs_sorted_oldest_first() {
        let store = make_store().await;
        let cid = ConversationId(store.create_conversation().await.unwrap().0);
        store
            .log_compression_failure(cid, "ctx A", "reason A")
            .await
            .unwrap();
        store
            .log_compression_failure(cid, "ctx B", "reason B")
            .await
            .unwrap();
        let pairs = store.get_unused_failure_pairs(10).await.unwrap();
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0].compressed_context, "ctx A");
    }

    #[tokio::test]
    async fn mark_pairs_used_reduces_count() {
        let store = make_store().await;
        let cid = ConversationId(store.create_conversation().await.unwrap().0);
        let id = store
            .log_compression_failure(cid, "ctx", "reason")
            .await
            .unwrap();
        store.mark_failure_pairs_used(&[id]).await.unwrap();
        let count = store.count_unused_failure_pairs().await.unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn cleanup_deletes_used_and_trims_unused() {
        let store = make_store().await;
        let cid = ConversationId(store.create_conversation().await.unwrap().0);
        // Add 3 pairs and mark 1 used.
        let id1 = store
            .log_compression_failure(cid, "ctx1", "r1")
            .await
            .unwrap();
        store
            .log_compression_failure(cid, "ctx2", "r2")
            .await
            .unwrap();
        store
            .log_compression_failure(cid, "ctx3", "r3")
            .await
            .unwrap();
        store.mark_failure_pairs_used(&[id1]).await.unwrap();
        // Cleanup: keep at most 1 unused.
        store.cleanup_old_failure_pairs(1).await.unwrap();
        let count = store.count_unused_failure_pairs().await.unwrap();
        assert_eq!(count, 1, "only 1 unused pair should remain");
    }

    #[tokio::test]
    async fn truncate_field_respects_char_boundary() {
        let s = "а".repeat(5000); // Cyrillic 'а', 2 bytes each
        let truncated = truncate_field(&s);
        assert!(truncated.len() <= MAX_FIELD_CHARS);
        assert!(s.is_char_boundary(truncated.len()));
    }
}
