// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use sqlx::SqlitePool;

use crate::error::MemoryError;

pub struct ResponseCache {
    pool: SqlitePool,
    ttl_secs: u64,
}

impl ResponseCache {
    #[must_use]
    pub fn new(pool: SqlitePool, ttl_secs: u64) -> Self {
        Self { pool, ttl_secs }
    }

    /// Look up a cached response by key. Returns `None` if not found or expired.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn get(&self, key: &str) -> Result<Option<String>, MemoryError> {
        let now = unix_now();
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT response FROM response_cache WHERE cache_key = ? AND expires_at > ?",
        )
        .bind(key)
        .bind(now)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(r,)| r))
    }

    /// Store a response in the cache with TTL.
    ///
    /// # Errors
    ///
    /// Returns an error if the database insert fails.
    pub async fn put(&self, key: &str, response: &str, model: &str) -> Result<(), MemoryError> {
        let now = unix_now();
        // Cap TTL at 1 year (31_536_000 s) to prevent i64 overflow for extreme values.
        let expires_at = now.saturating_add(self.ttl_secs.min(31_536_000).cast_signed());
        sqlx::query(
            "INSERT OR REPLACE INTO response_cache (cache_key, response, model, created_at, expires_at) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(key)
        .bind(response)
        .bind(model)
        .bind(now)
        .bind(expires_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Semantic similarity-based cache lookup.
    ///
    /// Fetches up to `max_candidates` non-expired rows with matching `embedding_model`,
    /// deserializes each embedding, computes cosine similarity against the query vector,
    /// and returns the response with the highest score if it meets `similarity_threshold`.
    ///
    /// Returns `(response_text, score)` on hit, `None` on miss.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn get_semantic(
        &self,
        embedding: &[f32],
        embedding_model: &str,
        similarity_threshold: f32,
        max_candidates: u32,
    ) -> Result<Option<(String, f32)>, MemoryError> {
        let now = unix_now();
        let rows: Vec<(String, Vec<u8>)> = sqlx::query_as(
            "SELECT response, embedding FROM response_cache \
             WHERE embedding_model = ? AND embedding IS NOT NULL AND expires_at > ? \
             ORDER BY embedding_ts DESC LIMIT ?",
        )
        .bind(embedding_model)
        .bind(now)
        .bind(max_candidates)
        .fetch_all(&self.pool)
        .await?;

        let mut best_score = -1.0_f32;
        let mut best_response: Option<String> = None;

        for (response, blob) in rows {
            // bytemuck::try_cast_slice handles corrupt BLOBs (non-multiple-of-4 length) safely.
            match bytemuck::try_cast_slice::<u8, f32>(&blob) {
                Ok(stored) => {
                    let score = crate::math::cosine_similarity(embedding, stored);
                    if score > best_score {
                        best_score = score;
                        best_response = Some(response);
                    }
                }
                Err(e) => {
                    tracing::warn!("semantic cache: failed to deserialize embedding blob: {e}");
                }
            }
        }

        if best_score >= similarity_threshold {
            Ok(best_response.map(|r| (r, best_score)))
        } else {
            Ok(None)
        }
    }

    /// Store a response with an embedding vector for future semantic matching.
    ///
    /// Uses `INSERT OR REPLACE` — updates the embedding on existing rows.
    ///
    /// # Errors
    ///
    /// Returns an error if the database insert fails.
    pub async fn put_with_embedding(
        &self,
        key: &str,
        response: &str,
        model: &str,
        embedding: &[f32],
        embedding_model: &str,
    ) -> Result<(), MemoryError> {
        let now = unix_now();
        let expires_at = now.saturating_add(self.ttl_secs.min(31_536_000).cast_signed());
        // Zero-copy serialization: &[f32] → &[u8] via bytemuck.
        let blob: &[u8] = bytemuck::cast_slice(embedding);
        sqlx::query(
            "INSERT OR REPLACE INTO response_cache \
             (cache_key, response, model, created_at, expires_at, embedding, embedding_model, embedding_ts) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(key)
        .bind(response)
        .bind(model)
        .bind(now)
        .bind(expires_at)
        .bind(blob)
        .bind(embedding_model)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Set `embedding = NULL` for all rows with the given `embedding_model`.
    ///
    /// Called when the embedding model changes to prevent cross-model false hits.
    /// Returns the number of rows updated.
    ///
    /// # Errors
    ///
    /// Returns an error if the database update fails.
    pub async fn invalidate_embeddings_for_model(
        &self,
        old_model: &str,
    ) -> Result<u64, MemoryError> {
        let result = sqlx::query(
            "UPDATE response_cache \
             SET embedding = NULL, embedding_model = NULL, embedding_ts = NULL \
             WHERE embedding_model = ?",
        )
        .bind(old_model)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// Two-phase cleanup: delete expired rows, then NULL-ify stale embeddings.
    ///
    /// Phase 1: DELETE rows where `expires_at <= now`.
    /// Phase 2: UPDATE rows where `embedding_model != current_embedding_model` to NULL out
    ///          the embedding columns. Exact-match data (`cache_key`, `response`) is preserved.
    ///
    /// Returns the total number of rows affected (deleted + updated).
    ///
    /// # Errors
    ///
    /// Returns an error if either database operation fails.
    pub async fn cleanup(&self, current_embedding_model: &str) -> Result<u64, MemoryError> {
        let now = unix_now();
        let deleted = sqlx::query("DELETE FROM response_cache WHERE expires_at <= ?")
            .bind(now)
            .execute(&self.pool)
            .await?
            .rows_affected();

        let updated = sqlx::query(
            "UPDATE response_cache \
             SET embedding = NULL, embedding_model = NULL, embedding_ts = NULL \
             WHERE embedding IS NOT NULL AND embedding_model != ?",
        )
        .bind(current_embedding_model)
        .execute(&self.pool)
        .await?
        .rows_affected();

        Ok(deleted + updated)
    }

    /// Delete expired cache entries. Returns the number of rows deleted.
    ///
    /// # Errors
    ///
    /// Returns an error if the database delete fails.
    pub async fn cleanup_expired(&self) -> Result<u64, MemoryError> {
        let now = unix_now();
        let result = sqlx::query("DELETE FROM response_cache WHERE expires_at <= ?")
            .bind(now)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }

    /// Compute a deterministic cache key from the last user message and model name using blake3.
    ///
    /// The key intentionally ignores conversation history so that identical user messages
    /// produce cache hits regardless of what preceded them. This is the desired behavior for
    /// a short-TTL response cache, but it means context-dependent questions (e.g. "Explain
    /// this") may return a cached response from a different context. The TTL bounds staleness.
    #[must_use]
    pub fn compute_key(last_user_message: &str, model: &str) -> String {
        let mut hasher = blake3::Hasher::new();
        let content = last_user_message.as_bytes();
        hasher.update(&(content.len() as u64).to_le_bytes());
        hasher.update(content);
        let model_bytes = model.as_bytes();
        hasher.update(&(model_bytes.len() as u64).to_le_bytes());
        hasher.update(model_bytes);
        hasher.finalize().to_hex().to_string()
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .cast_signed()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sqlite::SqliteStore;

    async fn test_cache() -> ResponseCache {
        let store = SqliteStore::new(":memory:").await.unwrap();
        ResponseCache::new(store.pool().clone(), 3600)
    }

    #[tokio::test]
    async fn cache_miss_returns_none() {
        let cache = test_cache().await;
        let result = cache.get("nonexistent").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn cache_put_and_get_roundtrip() {
        let cache = test_cache().await;
        cache.put("key1", "response text", "gpt-4").await.unwrap();
        let result = cache.get("key1").await.unwrap();
        assert_eq!(result.as_deref(), Some("response text"));
    }

    #[tokio::test]
    async fn cache_expired_entry_returns_none() {
        let store = SqliteStore::new(":memory:").await.unwrap();
        let cache = ResponseCache::new(store.pool().clone(), 0);
        // ttl=0 means expires_at == now, which fails the > check
        cache.put("key1", "response", "model").await.unwrap();
        // Immediately expired (expires_at = now + 0 = now, query checks > now)
        let result = cache.get("key1").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn cleanup_expired_removes_entries() {
        let store = SqliteStore::new(":memory:").await.unwrap();
        let cache = ResponseCache::new(store.pool().clone(), 0);
        cache.put("key1", "response", "model").await.unwrap();
        let deleted = cache.cleanup_expired().await.unwrap();
        assert!(deleted > 0);
    }

    #[tokio::test]
    async fn cleanup_does_not_remove_valid_entries() {
        let cache = test_cache().await;
        cache.put("key1", "response", "model").await.unwrap();
        let deleted = cache.cleanup_expired().await.unwrap();
        assert_eq!(deleted, 0);
        let result = cache.get("key1").await.unwrap();
        assert!(result.is_some());
    }

    #[test]
    fn compute_key_deterministic() {
        let k1 = ResponseCache::compute_key("hello", "gpt-4");
        let k2 = ResponseCache::compute_key("hello", "gpt-4");
        assert_eq!(k1, k2);
    }

    #[test]
    fn compute_key_different_for_different_content() {
        assert_ne!(
            ResponseCache::compute_key("hello", "gpt-4"),
            ResponseCache::compute_key("world", "gpt-4")
        );
    }

    #[test]
    fn compute_key_different_for_different_model() {
        assert_ne!(
            ResponseCache::compute_key("hello", "gpt-4"),
            ResponseCache::compute_key("hello", "gpt-3.5")
        );
    }

    #[test]
    fn compute_key_empty_message() {
        let k = ResponseCache::compute_key("", "model");
        assert!(!k.is_empty());
    }

    #[tokio::test]
    async fn ttl_extreme_value_does_not_overflow() {
        let store = SqliteStore::new(":memory:").await.unwrap();
        // Use u64::MAX - 1 as TTL; without capping this would overflow i64.
        let cache = ResponseCache::new(store.pool().clone(), u64::MAX - 1);
        // Should not panic or produce a negative expires_at.
        cache.put("key1", "response", "model").await.unwrap();
        // Entry should be retrievable (far-future expiry).
        let result = cache.get("key1").await.unwrap();
        assert_eq!(result.as_deref(), Some("response"));
    }

    #[tokio::test]
    async fn insert_or_replace_updates_existing_entry() {
        let cache = test_cache().await;
        cache.put("key1", "first response", "gpt-4").await.unwrap();
        cache.put("key1", "second response", "gpt-4").await.unwrap();
        let result = cache.get("key1").await.unwrap();
        assert_eq!(result.as_deref(), Some("second response"));
    }

    // --- Semantic cache tests ---

    #[tokio::test]
    async fn test_semantic_get_empty_cache() {
        let cache = test_cache().await;
        let result = cache
            .get_semantic(&[1.0, 0.0], "model-a", 0.9, 10)
            .await
            .unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_semantic_get_identical_embedding() {
        let cache = test_cache().await;
        let embedding = vec![1.0_f32, 0.0, 0.0];
        cache
            .put_with_embedding("k1", "response-a", "m1", &embedding, "model-a")
            .await
            .unwrap();
        let result = cache
            .get_semantic(&embedding, "model-a", 0.9, 10)
            .await
            .unwrap();
        assert!(result.is_some());
        let (resp, score) = result.unwrap();
        assert_eq!(resp, "response-a");
        assert!(
            (score - 1.0).abs() < 1e-5,
            "expected score ~1.0, got {score}"
        );
    }

    #[tokio::test]
    async fn test_semantic_get_orthogonal_vectors() {
        let cache = test_cache().await;
        // Store [1, 0, 0]
        cache
            .put_with_embedding("k1", "response-a", "m1", &[1.0, 0.0, 0.0], "model-a")
            .await
            .unwrap();
        // Query with [0, 1, 0] — perpendicular, similarity ~0.0
        let result = cache
            .get_semantic(&[0.0, 1.0, 0.0], "model-a", 0.5, 10)
            .await
            .unwrap();
        assert!(result.is_none(), "orthogonal vectors should not hit");
    }

    #[tokio::test]
    async fn test_semantic_get_similar_above_threshold() {
        let cache = test_cache().await;
        let stored = vec![1.0_f32, 0.1, 0.0];
        cache
            .put_with_embedding("k1", "response-a", "m1", &stored, "model-a")
            .await
            .unwrap();
        // Very similar vector — should exceed 0.9 threshold
        let query = vec![1.0_f32, 0.05, 0.0];
        let result = cache
            .get_semantic(&query, "model-a", 0.9, 10)
            .await
            .unwrap();
        assert!(
            result.is_some(),
            "similar vector should hit at threshold 0.9"
        );
    }

    #[tokio::test]
    async fn test_semantic_get_similar_below_threshold() {
        let cache = test_cache().await;
        // Store [1, 0, 0]
        cache
            .put_with_embedding("k1", "response-a", "m1", &[1.0, 0.0, 0.0], "model-a")
            .await
            .unwrap();
        // Store [0.7, 0.7, 0] — ~45 degrees off, cosine ~0.7
        let query = vec![0.0_f32, 1.0, 0.0];
        let result = cache
            .get_semantic(&query, "model-a", 0.95, 10)
            .await
            .unwrap();
        assert!(
            result.is_none(),
            "dissimilar vector should not hit at high threshold"
        );
    }

    #[tokio::test]
    async fn test_semantic_get_max_candidates_limit() {
        let cache = test_cache().await;
        // Insert 5 entries with identical embeddings
        for i in 0..5_u8 {
            cache
                .put_with_embedding(
                    &format!("k{i}"),
                    &format!("response-{i}"),
                    "m1",
                    &[1.0, 0.0],
                    "model-a",
                )
                .await
                .unwrap();
        }
        // With max_candidates=2, we only see 2 rows, but still get a hit since they match.
        let result = cache
            .get_semantic(&[1.0, 0.0], "model-a", 0.9, 2)
            .await
            .unwrap();
        assert!(result.is_some());
    }

    #[tokio::test]
    async fn test_semantic_get_ignores_expired() {
        let store = crate::sqlite::SqliteStore::new(":memory:").await.unwrap();
        // TTL=0 → immediately expired
        let cache = ResponseCache::new(store.pool().clone(), 0);
        cache
            .put_with_embedding("k1", "response-a", "m1", &[1.0, 0.0], "model-a")
            .await
            .unwrap();
        let result = cache
            .get_semantic(&[1.0, 0.0], "model-a", 0.9, 10)
            .await
            .unwrap();
        assert!(result.is_none(), "expired entries should not be returned");
    }

    #[tokio::test]
    async fn test_semantic_get_filters_by_embedding_model() {
        let cache = test_cache().await;
        // Store entry with model-a
        cache
            .put_with_embedding("k1", "response-a", "m1", &[1.0, 0.0], "model-a")
            .await
            .unwrap();
        // Query with model-b — should not find it
        let result = cache
            .get_semantic(&[1.0, 0.0], "model-b", 0.9, 10)
            .await
            .unwrap();
        assert!(result.is_none(), "wrong embedding model should not match");
    }

    #[tokio::test]
    async fn test_put_with_embedding_roundtrip() {
        let cache = test_cache().await;
        let embedding = vec![0.5_f32, 0.5, 0.707];
        cache
            .put_with_embedding(
                "key1",
                "semantic response",
                "gpt-4",
                &embedding,
                "embed-model",
            )
            .await
            .unwrap();
        // Exact-match still works
        let exact = cache.get("key1").await.unwrap();
        assert_eq!(exact.as_deref(), Some("semantic response"));
        // Semantic lookup works too
        let semantic = cache
            .get_semantic(&embedding, "embed-model", 0.99, 10)
            .await
            .unwrap();
        assert!(semantic.is_some());
        let (resp, score) = semantic.unwrap();
        assert_eq!(resp, "semantic response");
        assert!((score - 1.0).abs() < 1e-5);
    }

    #[tokio::test]
    async fn test_invalidate_embeddings_for_model() {
        let cache = test_cache().await;
        cache
            .put_with_embedding("k1", "resp", "m1", &[1.0, 0.0], "model-a")
            .await
            .unwrap();
        let updated = cache
            .invalidate_embeddings_for_model("model-a")
            .await
            .unwrap();
        assert_eq!(updated, 1);
        // Exact match still works after invalidation
        let exact = cache.get("k1").await.unwrap();
        assert_eq!(exact.as_deref(), Some("resp"));
        // Semantic lookup should return nothing
        let semantic = cache
            .get_semantic(&[1.0, 0.0], "model-a", 0.9, 10)
            .await
            .unwrap();
        assert!(semantic.is_none());
    }

    #[tokio::test]
    async fn test_cleanup_nulls_stale_embeddings() {
        let cache = test_cache().await;
        cache
            .put_with_embedding("k1", "resp", "m1", &[1.0, 0.0], "model-old")
            .await
            .unwrap();
        let affected = cache.cleanup("model-new").await.unwrap();
        assert!(affected > 0, "should have updated stale embedding row");
        // Row survives (exact match preserved)
        let exact = cache.get("k1").await.unwrap();
        assert_eq!(exact.as_deref(), Some("resp"));
        // Semantic lookup with old model returns nothing
        let semantic = cache
            .get_semantic(&[1.0, 0.0], "model-old", 0.9, 10)
            .await
            .unwrap();
        assert!(semantic.is_none());
    }

    #[tokio::test]
    async fn test_cleanup_deletes_expired() {
        let store = crate::sqlite::SqliteStore::new(":memory:").await.unwrap();
        let cache = ResponseCache::new(store.pool().clone(), 0);
        cache.put("k1", "resp", "m1").await.unwrap();
        let affected = cache.cleanup("model-a").await.unwrap();
        assert!(affected > 0);
        let result = cache.get("k1").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_cleanup_preserves_valid() {
        let cache = test_cache().await;
        cache
            .put_with_embedding("k1", "resp", "m1", &[1.0, 0.0], "model-current")
            .await
            .unwrap();
        let affected = cache.cleanup("model-current").await.unwrap();
        assert_eq!(affected, 0, "valid entries should not be affected");
        let semantic = cache
            .get_semantic(&[1.0, 0.0], "model-current", 0.9, 10)
            .await
            .unwrap();
        assert!(semantic.is_some());
    }
}
