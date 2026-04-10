// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use zeph_llm::provider::LlmProvider as _;

use crate::error::MemoryError;
use crate::types::ConversationId;
use crate::vector_store::{FieldCondition, FieldValue, VectorFilter};

use super::{SESSION_SUMMARIES_COLLECTION, SemanticMemory};

#[derive(Debug, Clone)]
pub struct SessionSummaryResult {
    pub summary_text: String,
    pub score: f32,
    pub conversation_id: ConversationId,
}

impl SemanticMemory {
    /// Check whether a session summary already exists for the given conversation.
    ///
    /// Returns `true` if at least one session summary is stored in `SQLite` for this conversation.
    /// Used as the primary guard in the shutdown summary path to handle cases where hard
    /// compaction fired but its Qdrant write failed (the `SQLite` record is the authoritative source).
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub async fn has_session_summary(
        &self,
        conversation_id: ConversationId,
    ) -> Result<bool, MemoryError> {
        let summaries = self.sqlite.load_summaries(conversation_id).await?;
        Ok(!summaries.is_empty())
    }

    /// Store a shutdown session summary: persists to `SQLite`, embeds into the
    /// `zeph_session_summaries` Qdrant collection (so cross-session search can find it),
    /// and stores key facts into the key-facts collection.
    ///
    /// Unlike the hard-compaction path, `first_message_id` and `last_message_id` are `None`
    /// because the shutdown hook does not track exact message boundaries.
    ///
    /// # Errors
    ///
    /// Returns an error if the `SQLite` insert fails. Qdrant errors are logged as warnings
    /// and do not propagate — the `SQLite` record is the authoritative summary store.
    pub async fn store_shutdown_summary(
        &self,
        conversation_id: ConversationId,
        summary_text: &str,
        key_facts: &[String],
    ) -> Result<(), MemoryError> {
        let token_estimate =
            i64::try_from(self.token_counter.count_tokens(summary_text)).unwrap_or(0);
        // Persist to SQLite first — this is the authoritative record and the source of truth
        // for has_session_summary(). NULL message range = session-level summary.
        let summary_id = self
            .sqlite
            .save_summary(conversation_id, summary_text, None, None, token_estimate)
            .await?;

        // Embed into SESSION_SUMMARIES_COLLECTION so search_session_summaries() can find it.
        if let Err(e) = self
            .store_session_summary(conversation_id, summary_text)
            .await
        {
            tracing::warn!("shutdown summary: failed to embed into session summaries: {e:#}");
        }

        if !key_facts.is_empty() {
            self.store_key_facts(conversation_id, summary_id, key_facts)
                .await;
        }

        tracing::debug!(
            conversation_id = conversation_id.0,
            summary_id,
            "stored shutdown session summary"
        );
        Ok(())
    }

    /// Store a session summary into the dedicated `zeph_session_summaries` Qdrant collection.
    ///
    /// # Errors
    ///
    /// Returns an error if embedding or Qdrant storage fails.
    pub async fn store_session_summary(
        &self,
        conversation_id: ConversationId,
        summary_text: &str,
    ) -> Result<(), MemoryError> {
        let Some(qdrant) = &self.qdrant else {
            return Ok(());
        };
        if !self.provider.supports_embeddings() {
            return Ok(());
        }

        let vector = self.provider.embed(summary_text).await?;
        let vector_size = u64::try_from(vector.len()).unwrap_or(896);
        qdrant
            .ensure_named_collection(SESSION_SUMMARIES_COLLECTION, vector_size)
            .await?;

        let point_id = {
            const NS: uuid::Uuid = uuid::Uuid::NAMESPACE_OID;
            uuid::Uuid::new_v5(&NS, conversation_id.0.to_string().as_bytes()).to_string()
        };
        let payload = serde_json::json!({
            "conversation_id": conversation_id.0,
            "summary_text": summary_text,
        });

        qdrant
            .upsert_to_collection(SESSION_SUMMARIES_COLLECTION, &point_id, payload, vector)
            .await?;

        tracing::debug!(
            conversation_id = conversation_id.0,
            "stored session summary"
        );
        Ok(())
    }

    /// Search session summaries from other conversations.
    ///
    /// # Errors
    ///
    /// Returns an error if embedding or Qdrant search fails.
    #[cfg_attr(
        feature = "profiling",
        tracing::instrument(name = "memory.cross_session", skip_all, fields(result_count = tracing::field::Empty))
    )]
    pub async fn search_session_summaries(
        &self,
        query: &str,
        limit: usize,
        exclude_conversation_id: Option<ConversationId>,
    ) -> Result<Vec<SessionSummaryResult>, MemoryError> {
        let Some(qdrant) = &self.qdrant else {
            tracing::debug!("session-summaries: skipped, no vector store");
            return Ok(Vec::new());
        };
        if !self.provider.supports_embeddings() {
            tracing::debug!("session-summaries: skipped, no embedding support");
            return Ok(Vec::new());
        }

        let vector = self.provider.embed(query).await?;
        let vector_size = u64::try_from(vector.len()).unwrap_or(896);
        qdrant
            .ensure_named_collection(SESSION_SUMMARIES_COLLECTION, vector_size)
            .await?;

        let filter = exclude_conversation_id.map(|cid| VectorFilter {
            must: vec![],
            must_not: vec![FieldCondition {
                field: "conversation_id".into(),
                value: FieldValue::Integer(cid.0),
            }],
        });

        let points = qdrant
            .search_collection(SESSION_SUMMARIES_COLLECTION, &vector, limit, filter)
            .await?;

        tracing::debug!(
            results = points.len(),
            limit,
            exclude_conversation_id = exclude_conversation_id.map(|c| c.0),
            "session-summaries: search complete"
        );

        let results = points
            .into_iter()
            .filter_map(|point| {
                let summary_text = point.payload.get("summary_text")?.as_str()?.to_owned();
                let conversation_id =
                    ConversationId(point.payload.get("conversation_id")?.as_i64()?);
                Some(SessionSummaryResult {
                    summary_text,
                    score: point.score,
                    conversation_id,
                })
            })
            .collect();

        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use zeph_llm::any::AnyProvider;
    use zeph_llm::mock::MockProvider;

    use crate::types::MessageId;

    use super::*;

    async fn make_memory() -> SemanticMemory {
        SemanticMemory::new(
            ":memory:",
            "http://127.0.0.1:1",
            AnyProvider::Mock(MockProvider::default()),
            "test-model",
        )
        .await
        .unwrap()
    }

    /// Insert a real message into the conversation and return its `MessageId`.
    /// Required because the `summaries` table has FK constraints on `messages.id`.
    async fn insert_message(memory: &SemanticMemory, cid: ConversationId) -> MessageId {
        memory
            .sqlite()
            .save_message(cid, "user", "test message")
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn has_session_summary_returns_false_when_no_summaries() {
        let memory = make_memory().await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        let result = memory.has_session_summary(cid).await.unwrap();
        assert!(!result, "new conversation must have no summaries");
    }

    #[tokio::test]
    async fn has_session_summary_returns_true_after_summary_stored_via_sqlite() {
        let memory = make_memory().await;
        let cid = memory.sqlite().create_conversation().await.unwrap();
        let msg_id = insert_message(&memory, cid).await;

        // Use sqlite directly to insert a valid summary with real FK references.
        memory
            .sqlite()
            .save_summary(
                cid,
                "session about Rust and async",
                Some(msg_id),
                Some(msg_id),
                10,
            )
            .await
            .unwrap();

        let result = memory.has_session_summary(cid).await.unwrap();
        assert!(result, "must return true after a summary is persisted");
    }

    #[tokio::test]
    async fn has_session_summary_is_isolated_per_conversation() {
        let memory = make_memory().await;
        let cid_a = memory.sqlite().create_conversation().await.unwrap();
        let cid_b = memory.sqlite().create_conversation().await.unwrap();
        let msg_id = insert_message(&memory, cid_a).await;

        memory
            .sqlite()
            .save_summary(
                cid_a,
                "summary for conversation A",
                Some(msg_id),
                Some(msg_id),
                5,
            )
            .await
            .unwrap();

        assert!(
            memory.has_session_summary(cid_a).await.unwrap(),
            "cid_a must have a summary"
        );
        assert!(
            !memory.has_session_summary(cid_b).await.unwrap(),
            "cid_b must not be affected by cid_a summary"
        );
    }

    #[test]
    fn store_session_summary_point_id_is_deterministic() {
        // Same conversation_id must always produce the same UUID v5 point ID,
        // ensuring that repeated compaction calls upsert rather than insert a new point.
        const NS: uuid::Uuid = uuid::Uuid::NAMESPACE_OID;
        let cid = ConversationId(42);
        let id1 = uuid::Uuid::new_v5(&NS, cid.0.to_string().as_bytes()).to_string();
        let id2 = uuid::Uuid::new_v5(&NS, cid.0.to_string().as_bytes()).to_string();
        assert_eq!(
            id1, id2,
            "point_id must be deterministic for the same conversation_id"
        );

        let cid2 = ConversationId(43);
        let id3 = uuid::Uuid::new_v5(&NS, cid2.0.to_string().as_bytes()).to_string();
        assert_ne!(
            id1, id3,
            "different conversation_ids must produce different point_ids"
        );
    }

    #[test]
    fn store_session_summary_point_id_boundary_ids() {
        // conversation_id = 0 and negative values are valid i64 variants — confirm they produce
        // valid, distinct, and stable UUIDs.
        const NS: uuid::Uuid = uuid::Uuid::NAMESPACE_OID;

        let id_zero_a = uuid::Uuid::new_v5(&NS, ConversationId(0).0.to_string().as_bytes());
        let id_zero_b = uuid::Uuid::new_v5(&NS, ConversationId(0).0.to_string().as_bytes());
        assert_eq!(id_zero_a, id_zero_b, "zero conversation_id must be stable");

        let id_neg = uuid::Uuid::new_v5(&NS, ConversationId(-1).0.to_string().as_bytes());
        assert_ne!(
            id_zero_a, id_neg,
            "zero and -1 conversation_ids must produce different point_ids"
        );

        // Confirm the UUID version is 5 (deterministic SHA-1 name-based).
        assert_eq!(
            id_zero_a.get_version_num(),
            5,
            "generated UUID must be version 5"
        );
    }

    #[tokio::test]
    async fn store_shutdown_summary_succeeds_with_null_message_ids() {
        let memory = make_memory().await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        let result = memory
            .store_shutdown_summary(cid, "summary text", &[])
            .await;

        assert!(
            result.is_ok(),
            "shutdown summary must succeed without messages"
        );
        assert!(
            memory.has_session_summary(cid).await.unwrap(),
            "SQLite must record the shutdown summary"
        );
    }
}
