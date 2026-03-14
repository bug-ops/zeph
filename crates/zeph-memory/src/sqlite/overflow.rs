// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use uuid::Uuid;

use crate::error::MemoryError;
use crate::sqlite::SqliteStore;

impl SqliteStore {
    /// Save overflow content associated with a conversation, returning the generated UUID.
    ///
    /// # Errors
    ///
    /// Returns an error if the database insert fails.
    pub async fn save_overflow(
        &self,
        conversation_id: i64,
        content: &[u8],
    ) -> Result<String, MemoryError> {
        let id = Uuid::new_v4().to_string();
        let byte_size = i64::try_from(content.len()).unwrap_or(i64::MAX);
        sqlx::query(
            "INSERT INTO tool_overflow (id, conversation_id, content, byte_size) \
             VALUES (?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(conversation_id)
        .bind(content)
        .bind(byte_size)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// Load overflow content by UUID, scoped to the given conversation.
    /// Returns `None` if the entry does not exist or belongs to a different conversation.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn load_overflow(
        &self,
        id: &str,
        conversation_id: i64,
    ) -> Result<Option<Vec<u8>>, MemoryError> {
        let row: Option<(Vec<u8>,)> = sqlx::query_as(
            "SELECT content FROM tool_overflow WHERE id = ? AND conversation_id = ?",
        )
        .bind(id)
        .bind(conversation_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(content,)| content))
    }

    /// Delete overflow entries older than `max_age_secs` seconds.
    /// Returns the number of deleted rows.
    ///
    /// # Errors
    ///
    /// Returns an error if the database delete fails.
    pub async fn cleanup_overflow(&self, max_age_secs: u64) -> Result<u64, MemoryError> {
        let result = sqlx::query(
            "DELETE FROM tool_overflow \
             WHERE created_at < datetime('now', printf('-%d seconds', ?))",
        )
        .bind(max_age_secs.cast_signed())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// Return total overflow bytes stored for a conversation.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn overflow_size(&self, conversation_id: i64) -> Result<u64, MemoryError> {
        let total: Option<i64> = sqlx::query_scalar(
            "SELECT COALESCE(SUM(byte_size), 0) FROM tool_overflow WHERE conversation_id = ?",
        )
        .bind(conversation_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(total.unwrap_or(0).cast_unsigned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn make_store() -> (SqliteStore, i64) {
        let store = SqliteStore::new(":memory:")
            .await
            .expect("SqliteStore::new");
        let cid = store
            .create_conversation()
            .await
            .expect("create_conversation");
        (store, cid.0)
    }

    #[tokio::test]
    async fn save_and_load_roundtrip() {
        let (store, cid) = make_store().await;
        let content = b"hello overflow world";
        let id = store.save_overflow(cid, content).await.expect("save");
        let loaded = store.load_overflow(&id, cid).await.expect("load");
        assert_eq!(loaded, Some(content.to_vec()));
    }

    #[tokio::test]
    async fn load_missing_returns_none() {
        let (store, cid) = make_store().await;
        let loaded = store
            .load_overflow("00000000-0000-0000-0000-000000000000", cid)
            .await
            .expect("load");
        assert!(loaded.is_none());
    }

    #[tokio::test]
    async fn load_wrong_conversation_returns_none() {
        let (store, cid1) = make_store().await;
        let cid2 = store
            .create_conversation()
            .await
            .expect("create_conversation")
            .0;
        let id = store.save_overflow(cid1, b"secret").await.expect("save");
        // Loading with a different conversation_id must return None.
        let loaded = store.load_overflow(&id, cid2).await.expect("load");
        assert!(
            loaded.is_none(),
            "overflow entry must not be accessible from a different conversation"
        );
    }

    #[tokio::test]
    async fn overflow_size_empty_returns_zero() {
        let (store, cid) = make_store().await;
        let size = store.overflow_size(cid).await.expect("size");
        assert_eq!(size, 0);
    }

    #[tokio::test]
    async fn overflow_size_sums_byte_sizes() {
        let (store, cid) = make_store().await;
        store.save_overflow(cid, b"aaa").await.expect("save1");
        store.save_overflow(cid, b"bb").await.expect("save2");
        let size = store.overflow_size(cid).await.expect("size");
        assert_eq!(size, 5);
    }

    #[tokio::test]
    async fn cascade_delete_removes_overflow() {
        let (store, cid) = make_store().await;
        let id = store.save_overflow(cid, b"data").await.expect("save");
        // Delete the conversation — overflow should cascade.
        sqlx::query("DELETE FROM conversations WHERE id = ?")
            .bind(cid)
            .execute(store.pool())
            .await
            .expect("delete conversation");
        // Use a fresh store to load by id only — conversation is gone, use id=0 (will miss).
        // Verify via direct SQL that the row is gone.
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tool_overflow WHERE id = ?")
            .bind(&id)
            .fetch_one(store.pool())
            .await
            .expect("count");
        assert_eq!(count, 0, "overflow row should be removed by CASCADE");
    }

    #[tokio::test]
    async fn cleanup_removes_old_entries() {
        let (store, cid) = make_store().await;
        // Insert a row with an old timestamp.
        let id = Uuid::new_v4().to_string();
        sqlx::query(
            "INSERT INTO tool_overflow (id, conversation_id, content, byte_size, created_at) \
             VALUES (?, ?, ?, ?, datetime('now', '-2 days'))",
        )
        .bind(&id)
        .bind(cid)
        .bind(b"old data".as_slice())
        .bind(8i64)
        .execute(store.pool())
        .await
        .expect("insert old row");

        // Insert a fresh row.
        let fresh_id = store.save_overflow(cid, b"fresh").await.expect("fresh");

        let deleted = store.cleanup_overflow(86400).await.expect("cleanup");
        assert_eq!(deleted, 1, "one old row should be deleted");

        assert!(
            store
                .load_overflow(&id, cid)
                .await
                .expect("load old")
                .is_none()
        );
        assert!(
            store
                .load_overflow(&fresh_id, cid)
                .await
                .expect("load fresh")
                .is_some()
        );
    }

    #[tokio::test]
    async fn cleanup_fresh_entries_not_removed() {
        let (store, cid) = make_store().await;
        store.save_overflow(cid, b"a").await.expect("save");
        store.save_overflow(cid, b"b").await.expect("save");
        // Cleanup with 1 day retention — fresh entries should not be removed.
        let deleted = store.cleanup_overflow(86400).await.expect("cleanup");
        assert_eq!(deleted, 0);
    }
}
