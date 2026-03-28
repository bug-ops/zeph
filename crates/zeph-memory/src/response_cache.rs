// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use zeph_db::DbPool;
#[allow(unused_imports)]
use zeph_db::sql;

use crate::error::MemoryError;

pub struct ResponseCache {
    pool: DbPool,
    ttl_secs: u64,
}

impl ResponseCache {
    #[must_use]
    pub fn new(pool: DbPool, ttl_secs: u64) -> Self {
        Self { pool, ttl_secs }
    }

    /// Look up a cached response by key. Returns `None` if not found or expired.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn get(&self, key: &str) -> Result<Option<String>, MemoryError> {
        let now = unix_now();
        let row: Option<(String,)> = sqlx::query_as(sql!(
            "SELECT response FROM response_cache WHERE cache_key = ? AND expires_at > ?"
        ))
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
        sqlx::query(sql!(
            "INSERT INTO response_cache (cache_key, response, model, created_at, expires_at) \
             VALUES (?, ?, ?, ?, ?) \
             ON CONFLICT(cache_key) DO UPDATE SET \
               response = excluded.response, model = excluded.model, \
               created_at = excluded.created_at, expires_at = excluded.expires_at"
        ))
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
        let rows: Vec<(String, Vec<u8>)> = sqlx::query_as(sql!(
            "SELECT response, embedding FROM response_cache \
             WHERE embedding_model = ? AND embedding IS NOT NULL AND expires_at > ? \
             ORDER BY embedding_ts DESC LIMIT ?"
        ))
        .bind(embedding_model)
        .bind(now)
        .bind(max_candidates)
        .fetch_all(&self.pool)
        .await?;

        let mut best_score = -1.0_f32;
        let mut best_response: Option<String> = None;

        for (response, blob) in &rows {
            if blob.len() % 4 != 0 {
                continue;
            }
            let stored: Vec<f32> = blob
                .chunks_exact(4)
                .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                .collect();
            let score = crate::math::cosine_similarity(embedding, &stored);
            tracing::debug!(
                score,
                threshold = similarity_threshold,
                "semantic cache candidate evaluated",
            );
            if score > best_score {
                best_score = score;
                best_response = Some(response.clone());
            }
        }

        tracing::debug!(
            examined = rows.len(),
            best_score,
            threshold = similarity_threshold,
            hit = best_score >= similarity_threshold,
            "semantic cache scan complete",
        );

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
        let blob: Vec<u8> = embedding.iter().flat_map(|f| f.to_le_bytes()).collect();
        sqlx::query(
            sql!("INSERT INTO response_cache \
             (cache_key, response, model, created_at, expires_at, embedding, embedding_model, embedding_ts) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(cache_key) DO UPDATE SET \
               response = excluded.response, model = excluded.model, \
               created_at = excluded.created_at, expires_at = excluded.expires_at, \
               embedding = excluded.embedding, embedding_model = excluded.embedding_model, \
               embedding_ts = excluded.embedding_ts"),
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
        let result = sqlx::query(sql!(
            "UPDATE response_cache \
             SET embedding = NULL, embedding_model = NULL, embedding_ts = NULL \
             WHERE embedding_model = ?"
        ))
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
        let deleted = sqlx::query(sql!("DELETE FROM response_cache WHERE expires_at <= ?"))
            .bind(now)
            .execute(&self.pool)
            .await?
            .rows_affected();

        let updated = sqlx::query(sql!(
            "UPDATE response_cache \
             SET embedding = NULL, embedding_model = NULL, embedding_ts = NULL \
             WHERE embedding IS NOT NULL AND embedding_model != ?"
        ))
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
        let result = sqlx::query(sql!("DELETE FROM response_cache WHERE expires_at <= ?"))
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
    use crate::store::SqliteStore;

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
        let store = crate::store::SqliteStore::new(":memory:").await.unwrap();
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
        let store = crate::store::SqliteStore::new(":memory:").await.unwrap();
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

    // --- Corrupted BLOB tests ---
    // These tests verify that get_semantic() gracefully handles corrupt embedding BLOBs
    // stored directly in the database (bypassing put_with_embedding), simulating real-world
    // scenarios such as disk errors, interrupted writes, or migration bugs.
    //
    // Note: NaN f32 values from garbage-but-valid-length BLOBs (length divisible by 4) are
    // handled safely by IEEE 754 semantics — NaN > x is always false, so best_score is never
    // updated and the row is silently skipped without panic.

    /// Helper: insert a row with a raw (potentially corrupt) embedding BLOB via SQL.
    async fn insert_corrupt_blob(pool: &DbPool, key: &str, blob: &[u8]) {
        let now = unix_now();
        let expires_at = now + 3600;
        sqlx::query(
            sql!("INSERT INTO response_cache \
             (cache_key, response, model, created_at, expires_at, embedding, embedding_model, embedding_ts) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)"),
        )
        .bind(key)
        .bind("corrupt-response")
        .bind("m1")
        .bind(now)
        .bind(expires_at)
        .bind(blob)
        .bind("model-a")
        .bind(now)
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn test_semantic_get_corrupted_blob_odd_length() {
        // A BLOB of 5 bytes is not a multiple of 4 and is skipped by the length guard.
        // Verify that get_semantic returns Ok(None) without panicking.
        let store = SqliteStore::new(":memory:").await.unwrap();
        let pool = store.pool().clone();
        let cache = ResponseCache::new(pool.clone(), 3600);

        insert_corrupt_blob(&pool, "corrupt-key", &[0xAB, 0xCD, 0xEF, 0x01, 0x02]).await;

        let result = cache
            .get_semantic(&[1.0, 0.0, 0.0], "model-a", 0.9, 10)
            .await
            .unwrap();
        assert!(
            result.is_none(),
            "corrupt odd-length BLOB must yield Ok(None)"
        );
    }

    #[tokio::test]
    async fn test_semantic_get_corrupted_blob_skips_to_valid() {
        // Insert one corrupt row (5 bytes) and one valid row with an embedding identical to
        // the query. Verify that the corrupt row is silently skipped and the valid row is
        // returned, proving the for loop continues after a deserialization failure.
        let store = SqliteStore::new(":memory:").await.unwrap();
        let pool = store.pool().clone();
        let cache = ResponseCache::new(pool.clone(), 3600);

        // Corrupt row — odd-length BLOB
        insert_corrupt_blob(&pool, "corrupt-key", &[0x01, 0x02, 0x03]).await;

        // Valid row — embedding [1.0, 0.0, 0.0] stored via the normal path
        let valid_embedding = vec![1.0_f32, 0.0, 0.0];
        cache
            .put_with_embedding(
                "valid-key",
                "valid-response",
                "m1",
                &valid_embedding,
                "model-a",
            )
            .await
            .unwrap();

        let result = cache
            .get_semantic(&valid_embedding, "model-a", 0.9, 10)
            .await
            .unwrap();
        assert!(
            result.is_some(),
            "valid row must be returned despite corrupt sibling"
        );
        let (resp, score) = result.unwrap();
        assert_eq!(resp, "valid-response");
        assert!(
            (score - 1.0).abs() < 1e-5,
            "identical vectors must yield score ~1.0, got {score}"
        );
    }

    #[tokio::test]
    async fn test_semantic_get_empty_blob() {
        // An empty BLOB (0 bytes): length % 4 == 0, so the guard passes and produces an empty
        // f32 slice. cosine_similarity returns 0.0 for mismatched lengths, which is below the
        // 0.9 threshold. Verify Ok(None) is returned without panicking.
        let store = SqliteStore::new(":memory:").await.unwrap();
        let pool = store.pool().clone();
        let cache = ResponseCache::new(pool.clone(), 3600);

        insert_corrupt_blob(&pool, "empty-blob-key", &[]).await;

        let result = cache
            .get_semantic(&[1.0, 0.0], "model-a", 0.9, 10)
            .await
            .unwrap();
        assert!(
            result.is_none(),
            "empty BLOB must yield Ok(None) at threshold 0.9"
        );
    }

    #[tokio::test]
    async fn test_semantic_get_all_blobs_corrupted() {
        // All rows have corrupt BLOBs of various invalid lengths:
        // 1, 3, 5, 7 bytes (odd) and 6 bytes (even but not a multiple of 4).
        // Verify that get_semantic returns Ok(None) — all rows gracefully skipped.
        let store = SqliteStore::new(":memory:").await.unwrap();
        let pool = store.pool().clone();
        let cache = ResponseCache::new(pool.clone(), 3600);

        let corrupt_blobs: &[&[u8]] = &[
            &[0x01],                                     // 1 byte
            &[0x01, 0x02, 0x03],                         // 3 bytes
            &[0x01, 0x02, 0x03, 0x04, 0x05],             // 5 bytes
            &[0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07], // 7 bytes
            &[0x01, 0x02, 0x03, 0x04, 0x05, 0x06], // 6 bytes (even, not multiple of 4 — REC-1)
        ];
        for (i, blob) in corrupt_blobs.iter().enumerate() {
            insert_corrupt_blob(&pool, &format!("corrupt-{i}"), blob).await;
        }

        let result = cache
            .get_semantic(&[1.0, 0.0, 0.0], "model-a", 0.9, 10)
            .await
            .unwrap();
        assert!(result.is_none(), "all corrupt BLOBs must yield Ok(None)");
    }

    // --- Dimension mismatch tests (issue #2034) ---

    #[tokio::test]
    async fn test_semantic_get_dimension_mismatch_returns_none() {
        // Store dim=3, query dim=2 — cosine_similarity returns 0.0 for length mismatch.
        // threshold=0.01 ensures 0.0 is below the bar (CRIT-01 fix verification).
        let cache = test_cache().await;
        cache
            .put_with_embedding("k1", "resp-3d", "m1", &[1.0, 0.0, 0.0], "model-a")
            .await
            .unwrap();
        let result = cache
            .get_semantic(&[1.0, 0.0], "model-a", 0.01, 10)
            .await
            .unwrap();
        assert!(
            result.is_none(),
            "dimension mismatch must not produce a hit"
        );
    }

    #[tokio::test]
    async fn test_semantic_get_dimension_mismatch_query_longer() {
        // Inverse case: store dim=2, query dim=3 — mismatch handling must be symmetric.
        let cache = test_cache().await;
        cache
            .put_with_embedding("k1", "resp-2d", "m1", &[1.0, 0.0], "model-a")
            .await
            .unwrap();
        let result = cache
            .get_semantic(&[1.0, 0.0, 0.0], "model-a", 0.01, 10)
            .await
            .unwrap();
        assert!(
            result.is_none(),
            "query longer than stored embedding must not produce a hit"
        );
    }

    #[tokio::test]
    async fn test_semantic_get_mixed_dimensions_picks_correct_match() {
        // Store entries at dim=2 and dim=3. Query with dim=3 must return only the dim=3 entry.
        // The dim=2 entry scores 0.0 (mismatch) and must not interfere.
        let cache = test_cache().await;
        cache
            .put_with_embedding("k-2d", "resp-2d", "m1", &[1.0, 0.0], "model-a")
            .await
            .unwrap();
        cache
            .put_with_embedding("k-3d", "resp-3d", "m1", &[1.0, 0.0, 0.0], "model-a")
            .await
            .unwrap();
        let result = cache
            .get_semantic(&[1.0, 0.0, 0.0], "model-a", 0.9, 10)
            .await
            .unwrap();
        assert!(result.is_some(), "matching dim=3 entry should be returned");
        let (response, score) = result.unwrap();
        assert_eq!(response, "resp-3d", "wrong entry returned");
        assert!(
            (score - 1.0).abs() < 1e-5,
            "expected score ~1.0 for identical vectors, got {score}"
        );
    }
}
