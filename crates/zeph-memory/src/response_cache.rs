use sqlx::SqlitePool;
use zeph_llm::provider::{Message, Role};

use crate::error::MemoryError;

fn role_to_str(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
    }
}

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

    /// Compute a deterministic cache key from messages and model name using blake3.
    #[must_use]
    pub fn compute_key(messages: &[Message], model: &str) -> String {
        let mut hasher = blake3::Hasher::new();
        for msg in messages {
            let role = role_to_str(msg.role).as_bytes();
            hasher.update(&(role.len() as u64).to_le_bytes());
            hasher.update(role);
            let content = msg.content.as_bytes();
            hasher.update(&(content.len() as u64).to_le_bytes());
            hasher.update(content);
        }
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
        let msgs = vec![Message {
            role: Role::User,
            content: "hello".into(),
            parts: vec![],
        }];
        let k1 = ResponseCache::compute_key(&msgs, "gpt-4");
        let k2 = ResponseCache::compute_key(&msgs, "gpt-4");
        assert_eq!(k1, k2);
    }

    #[test]
    fn compute_key_different_for_different_content() {
        let msgs1 = vec![Message {
            role: Role::User,
            content: "hello".into(),
            parts: vec![],
        }];
        let msgs2 = vec![Message {
            role: Role::User,
            content: "world".into(),
            parts: vec![],
        }];
        assert_ne!(
            ResponseCache::compute_key(&msgs1, "gpt-4"),
            ResponseCache::compute_key(&msgs2, "gpt-4")
        );
    }

    #[test]
    fn compute_key_different_for_different_model() {
        let msgs = vec![Message {
            role: Role::User,
            content: "hello".into(),
            parts: vec![],
        }];
        assert_ne!(
            ResponseCache::compute_key(&msgs, "gpt-4"),
            ResponseCache::compute_key(&msgs, "gpt-3.5")
        );
    }

    #[test]
    fn compute_key_empty_messages() {
        let k = ResponseCache::compute_key(&[], "model");
        assert!(!k.is_empty());
    }

    #[test]
    fn compute_key_no_length_prefix_ambiguity() {
        // Without length-prefix, "ab"+"c" and "a"+"bc" would hash identically.
        // With proper length-prefixing they must differ.
        let msgs1 = vec![
            Message {
                role: Role::User,
                content: "ab".into(),
                parts: vec![],
            },
            Message {
                role: Role::User,
                content: "c".into(),
                parts: vec![],
            },
        ];
        let msgs2 = vec![
            Message {
                role: Role::User,
                content: "a".into(),
                parts: vec![],
            },
            Message {
                role: Role::User,
                content: "bc".into(),
                parts: vec![],
            },
        ];
        assert_ne!(
            ResponseCache::compute_key(&msgs1, "model"),
            ResponseCache::compute_key(&msgs2, "model")
        );
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
}
