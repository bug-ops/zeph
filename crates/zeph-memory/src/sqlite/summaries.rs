// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use super::SqliteStore;
use crate::error::MemoryError;
use crate::types::{ConversationId, MessageId};

impl SqliteStore {
    /// Save a summary and return its ID.
    ///
    /// `first_message_id` and `last_message_id` are `None` for session-level summaries
    /// (e.g. shutdown summaries) that do not correspond to a specific message range.
    ///
    /// # Errors
    ///
    /// Returns an error if the insert fails.
    pub async fn save_summary(
        &self,
        conversation_id: ConversationId,
        content: &str,
        first_message_id: Option<MessageId>,
        last_message_id: Option<MessageId>,
        token_estimate: i64,
    ) -> Result<i64, MemoryError> {
        let row: (i64,) = sqlx::query_as(
            "INSERT INTO summaries (conversation_id, content, first_message_id, last_message_id, token_estimate) \
             VALUES (?, ?, ?, ?, ?) RETURNING id",
        )
        .bind(conversation_id)
        .bind(content)
        .bind(first_message_id)
        .bind(last_message_id)
        .bind(token_estimate)
        .fetch_one(&self.pool)
        .await
        ?;
        Ok(row.0)
    }

    /// Load all summaries for a conversation.
    ///
    /// `first_message_id` and `last_message_id` are `None` for session-level summaries.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn load_summaries(
        &self,
        conversation_id: ConversationId,
    ) -> Result<
        Vec<(
            i64,
            ConversationId,
            String,
            Option<MessageId>,
            Option<MessageId>,
            i64,
        )>,
        MemoryError,
    > {
        #[allow(clippy::type_complexity)]
        let rows: Vec<(
            i64,
            ConversationId,
            String,
            Option<MessageId>,
            Option<MessageId>,
            i64,
        )> = sqlx::query_as(
            "SELECT id, conversation_id, content, first_message_id, last_message_id, \
                 token_estimate FROM summaries WHERE conversation_id = ? ORDER BY id ASC",
        )
        .bind(conversation_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows)
    }

    /// Get the last message ID covered by the most recent summary.
    ///
    /// Returns `None` if no summaries exist or the most recent is a session-level summary
    /// (shutdown summary) with no tracked message range.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn latest_summary_last_message_id(
        &self,
        conversation_id: ConversationId,
    ) -> Result<Option<MessageId>, MemoryError> {
        let row: Option<(Option<MessageId>,)> = sqlx::query_as(
            "SELECT last_message_id FROM summaries \
             WHERE conversation_id = ? ORDER BY id DESC LIMIT 1",
        )
        .bind(conversation_id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.and_then(|r| r.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn test_store() -> SqliteStore {
        SqliteStore::new(":memory:").await.unwrap()
    }

    #[tokio::test]
    async fn save_and_load_summary() {
        let store = test_store().await;
        let cid = store.create_conversation().await.unwrap();

        let msg_id1 = store.save_message(cid, "user", "hello").await.unwrap();
        let msg_id2 = store.save_message(cid, "assistant", "hi").await.unwrap();

        let summary_id = store
            .save_summary(
                cid,
                "User greeted assistant",
                Some(msg_id1),
                Some(msg_id2),
                5,
            )
            .await
            .unwrap();

        let summaries = store.load_summaries(cid).await.unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].0, summary_id);
        assert_eq!(summaries[0].2, "User greeted assistant");
        assert_eq!(summaries[0].3, Some(msg_id1));
        assert_eq!(summaries[0].4, Some(msg_id2));
        assert_eq!(summaries[0].5, 5);
    }

    #[tokio::test]
    async fn load_summaries_empty() {
        let store = test_store().await;
        let cid = store.create_conversation().await.unwrap();

        let summaries = store.load_summaries(cid).await.unwrap();
        assert!(summaries.is_empty());
    }

    #[tokio::test]
    async fn load_summaries_ordered() {
        let store = test_store().await;
        let cid = store.create_conversation().await.unwrap();

        let msg_id1 = store.save_message(cid, "user", "m1").await.unwrap();
        let msg_id2 = store.save_message(cid, "assistant", "m2").await.unwrap();
        let msg_id3 = store.save_message(cid, "user", "m3").await.unwrap();

        let s1 = store
            .save_summary(cid, "summary1", Some(msg_id1), Some(msg_id2), 3)
            .await
            .unwrap();
        let s2 = store
            .save_summary(cid, "summary2", Some(msg_id2), Some(msg_id3), 3)
            .await
            .unwrap();

        let summaries = store.load_summaries(cid).await.unwrap();
        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].0, s1);
        assert_eq!(summaries[1].0, s2);
    }

    #[tokio::test]
    async fn latest_summary_last_message_id_none() {
        let store = test_store().await;
        let cid = store.create_conversation().await.unwrap();

        let last = store.latest_summary_last_message_id(cid).await.unwrap();
        assert!(last.is_none());
    }

    #[tokio::test]
    async fn latest_summary_last_message_id_some() {
        let store = test_store().await;
        let cid = store.create_conversation().await.unwrap();

        let msg_id1 = store.save_message(cid, "user", "m1").await.unwrap();
        let msg_id2 = store.save_message(cid, "assistant", "m2").await.unwrap();
        let msg_id3 = store.save_message(cid, "user", "m3").await.unwrap();

        store
            .save_summary(cid, "summary1", Some(msg_id1), Some(msg_id2), 3)
            .await
            .unwrap();
        store
            .save_summary(cid, "summary2", Some(msg_id2), Some(msg_id3), 3)
            .await
            .unwrap();

        let last = store.latest_summary_last_message_id(cid).await.unwrap();
        assert_eq!(last, Some(msg_id3));
    }

    #[tokio::test]
    async fn cascade_delete_removes_summaries() {
        let store = test_store().await;
        let pool = store.pool();
        let cid = store.create_conversation().await.unwrap();

        let msg_id1 = store.save_message(cid, "user", "m1").await.unwrap();
        let msg_id2 = store.save_message(cid, "assistant", "m2").await.unwrap();

        store
            .save_summary(cid, "summary", Some(msg_id1), Some(msg_id2), 3)
            .await
            .unwrap();

        let before: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM summaries WHERE conversation_id = ?")
                .bind(cid)
                .fetch_one(pool)
                .await
                .unwrap();
        assert_eq!(before.0, 1);

        sqlx::query("DELETE FROM conversations WHERE id = ?")
            .bind(cid)
            .execute(pool)
            .await
            .unwrap();

        let after: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM summaries WHERE conversation_id = ?")
                .bind(cid)
                .fetch_one(pool)
                .await
                .unwrap();
        assert_eq!(after.0, 0);
    }
}
