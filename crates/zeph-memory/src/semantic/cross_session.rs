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

        let payload = serde_json::json!({
            "conversation_id": conversation_id.0,
            "summary_text": summary_text,
        });

        qdrant
            .store_to_collection(SESSION_SUMMARIES_COLLECTION, payload, vector)
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
