// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `SQLite` CRUD for the `session_digest` table — upsert, load, delete.

use crate::error::MemoryError;
use crate::store::SqliteStore;
use crate::store::compression_guidelines::redact_sensitive;
use crate::types::ConversationId;
#[allow(unused_imports)]
use zeph_db::sql;

/// A distilled session digest: key facts and outcomes for a single conversation.
#[derive(Debug, Clone)]
pub struct SessionDigest {
    pub id: i64,
    pub conversation_id: ConversationId,
    pub digest: String,
    pub token_count: i64,
    pub updated_at: String,
}

impl SqliteStore {
    /// Upsert a session digest for `conversation_id`.
    ///
    /// Uses `INSERT ... ON CONFLICT ... DO UPDATE` to preserve the original `created_at`
    /// and avoid resetting the auto-incremented `id` on updates.
    ///
    /// # Errors
    ///
    /// Returns an error if the database write fails.
    pub async fn save_session_digest(
        &self,
        conversation_id: ConversationId,
        digest: &str,
        token_count: i64,
    ) -> Result<(), MemoryError> {
        let safe_digest = redact_sensitive(digest);
        sqlx::query(sql!(
            "INSERT INTO session_digest (conversation_id, digest, token_count, updated_at) \
             VALUES (?, ?, ?, CURRENT_TIMESTAMP) \
             ON CONFLICT(conversation_id) DO UPDATE SET \
               digest = excluded.digest, \
               token_count = excluded.token_count, \
               updated_at = excluded.updated_at"
        ))
        .bind(conversation_id.0)
        .bind(safe_digest.as_ref())
        .bind(token_count)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Load the session digest for `conversation_id`, if it exists.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn load_session_digest(
        &self,
        conversation_id: ConversationId,
    ) -> Result<Option<SessionDigest>, MemoryError> {
        let row = sqlx::query_as::<_, (i64, i64, String, i64, String)>(sql!(
            "SELECT id, conversation_id, digest, token_count, updated_at \
             FROM session_digest WHERE conversation_id = ?"
        ))
        .bind(conversation_id.0)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(
            |(id, conv_id, digest, token_count, updated_at)| SessionDigest {
                id,
                conversation_id: ConversationId(conv_id),
                digest,
                token_count,
                updated_at,
            },
        ))
    }

    /// Delete the session digest for `conversation_id`.
    ///
    /// # Errors
    ///
    /// Returns an error if the database write fails.
    pub async fn delete_session_digest(
        &self,
        conversation_id: ConversationId,
    ) -> Result<(), MemoryError> {
        sqlx::query(sql!("DELETE FROM session_digest WHERE conversation_id = ?"))
            .bind(conversation_id.0)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::SqliteStore;

    async fn make_store() -> SqliteStore {
        SqliteStore::with_pool_size(":memory:", 1)
            .await
            .expect("in-memory store")
    }

    async fn insert_conversation(store: &SqliteStore) -> ConversationId {
        sqlx::query_scalar::<_, i64>(sql!(
            "INSERT INTO conversations (created_at) VALUES (CURRENT_TIMESTAMP) RETURNING id"
        ))
        .fetch_one(&store.pool)
        .await
        .map(ConversationId)
        .expect("insert conversation")
    }

    #[tokio::test]
    async fn save_and_load_digest() {
        let store = make_store().await;
        let conv_id = insert_conversation(&store).await;

        store
            .save_session_digest(conv_id, "Key facts from session.", 5)
            .await
            .unwrap();

        let digest = store
            .load_session_digest(conv_id)
            .await
            .unwrap()
            .expect("digest should exist");

        assert_eq!(digest.conversation_id, conv_id);
        assert_eq!(digest.digest, "Key facts from session.");
        assert_eq!(digest.token_count, 5);
    }

    #[tokio::test]
    async fn upsert_preserves_id_and_created_at() {
        let store = make_store().await;
        let conv_id = insert_conversation(&store).await;

        store
            .save_session_digest(conv_id, "first", 3)
            .await
            .unwrap();
        let first = store.load_session_digest(conv_id).await.unwrap().unwrap();

        store
            .save_session_digest(conv_id, "updated", 7)
            .await
            .unwrap();
        let second = store.load_session_digest(conv_id).await.unwrap().unwrap();

        // id must NOT change on update (ON CONFLICT DO UPDATE, not INSERT OR REPLACE)
        assert_eq!(first.id, second.id);
        assert_eq!(second.digest, "updated");
        assert_eq!(second.token_count, 7);
    }

    #[tokio::test]
    async fn load_nonexistent_returns_none() {
        let store = make_store().await;
        let result = store
            .load_session_digest(ConversationId(9999))
            .await
            .unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn delete_digest() {
        let store = make_store().await;
        let conv_id = insert_conversation(&store).await;

        store
            .save_session_digest(conv_id, "to delete", 2)
            .await
            .unwrap();
        store.delete_session_digest(conv_id).await.unwrap();

        let result = store.load_session_digest(conv_id).await.unwrap();
        assert!(result.is_none());
    }
}
