// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use zeph_llm::provider::{LlmProvider as _, Message, MessageMetadata, Role};

use super::{KEY_FACTS_COLLECTION, SemanticMemory};
use crate::embedding_store::MessageKind;
use crate::error::MemoryError;
use crate::types::{ConversationId, MessageId};

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
pub struct StructuredSummary {
    pub summary: String,
    pub key_facts: Vec<String>,
    pub entities: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Summary {
    pub id: i64,
    pub conversation_id: ConversationId,
    pub content: String,
    /// `None` for session-level summaries (e.g. shutdown summaries) with no tracked message range.
    pub first_message_id: Option<MessageId>,
    /// `None` for session-level summaries (e.g. shutdown summaries) with no tracked message range.
    pub last_message_id: Option<MessageId>,
    pub token_estimate: i64,
}

#[must_use]
pub fn build_summarization_prompt(messages: &[(MessageId, String, String)]) -> String {
    let mut prompt = String::from(
        "Summarize the following conversation. Extract key facts, decisions, entities, \
         and context needed to continue the conversation.\n\n\
         Respond in JSON with fields: summary (string), key_facts (list of strings), \
         entities (list of strings).\n\nConversation:\n",
    );

    for (_, role, content) in messages {
        prompt.push_str(role);
        prompt.push_str(": ");
        prompt.push_str(content);
        prompt.push('\n');
    }

    prompt
}

impl SemanticMemory {
    /// Load all summaries for a conversation.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn load_summaries(
        &self,
        conversation_id: ConversationId,
    ) -> Result<Vec<Summary>, MemoryError> {
        let rows = self.sqlite.load_summaries(conversation_id).await?;
        let summaries = rows
            .into_iter()
            .map(
                |(
                    id,
                    conversation_id,
                    content,
                    first_message_id,
                    last_message_id,
                    token_estimate,
                )| {
                    Summary {
                        id,
                        conversation_id,
                        content,
                        first_message_id,
                        last_message_id,
                        token_estimate,
                    }
                },
            )
            .collect();
        Ok(summaries)
    }

    /// Generate a summary of the oldest unsummarized messages.
    ///
    /// Returns `Ok(None)` if there are not enough messages to summarize.
    ///
    /// # Errors
    ///
    /// Returns an error if LLM call or database operation fails.
    pub async fn summarize(
        &self,
        conversation_id: ConversationId,
        message_count: usize,
    ) -> Result<Option<i64>, MemoryError> {
        let total = self.sqlite.count_messages(conversation_id).await?;

        if total <= i64::try_from(message_count)? {
            return Ok(None);
        }

        let after_id = self
            .sqlite
            .latest_summary_last_message_id(conversation_id)
            .await?
            .unwrap_or(MessageId(0));

        let messages = self
            .sqlite
            .load_messages_range(conversation_id, after_id, message_count)
            .await?;

        if messages.is_empty() {
            return Ok(None);
        }

        let prompt = build_summarization_prompt(&messages);
        let chat_messages = vec![Message {
            role: Role::User,
            content: prompt,
            parts: vec![],
            metadata: MessageMetadata::default(),
        }];

        let structured = match self
            .provider
            .chat_typed_erased::<StructuredSummary>(&chat_messages)
            .await
        {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    "structured summarization failed, falling back to plain text: {e:#}"
                );
                let plain = self.provider.chat(&chat_messages).await?;
                StructuredSummary {
                    summary: plain,
                    key_facts: vec![],
                    entities: vec![],
                }
            }
        };
        let summary_text = &structured.summary;

        let token_estimate = i64::try_from(self.token_counter.count_tokens(summary_text))?;
        let first_message_id = messages[0].0;
        let last_message_id = messages[messages.len() - 1].0;

        let summary_id = self
            .sqlite
            .save_summary(
                conversation_id,
                summary_text,
                Some(first_message_id),
                Some(last_message_id),
                token_estimate,
            )
            .await?;

        if let Some(qdrant) = &self.qdrant
            && self.provider.supports_embeddings()
        {
            match self.provider.embed(summary_text).await {
                Ok(vector) => {
                    let vector_size = u64::try_from(vector.len()).unwrap_or(896);
                    if let Err(e) = qdrant.ensure_collection(vector_size).await {
                        tracing::warn!("Failed to ensure Qdrant collection: {e:#}");
                    } else if let Err(e) = qdrant
                        .store(
                            MessageId(summary_id),
                            conversation_id,
                            "system",
                            vector,
                            MessageKind::Summary,
                            &self.embedding_model,
                            0,
                        )
                        .await
                    {
                        tracing::warn!("Failed to embed summary: {e:#}");
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to generate summary embedding: {e:#}");
                }
            }
        }

        if !structured.key_facts.is_empty() {
            self.store_key_facts(conversation_id, summary_id, &structured.key_facts)
                .await;
        }

        Ok(Some(summary_id))
    }

    pub(super) async fn store_key_facts(
        &self,
        conversation_id: ConversationId,
        source_summary_id: i64,
        key_facts: &[String],
    ) {
        let Some(qdrant) = &self.qdrant else {
            return;
        };
        if !self.provider.supports_embeddings() {
            return;
        }

        let Some(first_fact) = key_facts.first() else {
            return;
        };
        let first_vector = match self.provider.embed(first_fact).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("Failed to embed key fact: {e:#}");
                return;
            }
        };
        let vector_size = u64::try_from(first_vector.len()).unwrap_or(896);
        if let Err(e) = qdrant
            .ensure_named_collection(KEY_FACTS_COLLECTION, vector_size)
            .await
        {
            tracing::warn!("Failed to ensure key_facts collection: {e:#}");
            return;
        }

        let first_payload = serde_json::json!({
            "conversation_id": conversation_id.0,
            "fact_text": first_fact,
            "source_summary_id": source_summary_id,
        });
        if let Err(e) = qdrant
            .store_to_collection(KEY_FACTS_COLLECTION, first_payload, first_vector)
            .await
        {
            tracing::warn!("Failed to store key fact: {e:#}");
        }

        for fact in &key_facts[1..] {
            match self.provider.embed(fact).await {
                Ok(vector) => {
                    let payload = serde_json::json!({
                        "conversation_id": conversation_id.0,
                        "fact_text": fact,
                        "source_summary_id": source_summary_id,
                    });
                    if let Err(e) = qdrant
                        .store_to_collection(KEY_FACTS_COLLECTION, payload, vector)
                        .await
                    {
                        tracing::warn!("Failed to store key fact: {e:#}");
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to embed key fact: {e:#}");
                }
            }
        }
    }

    /// Search key facts extracted from conversation summaries.
    ///
    /// # Errors
    ///
    /// Returns an error if embedding or Qdrant search fails.
    pub async fn search_key_facts(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Vec<String>, MemoryError> {
        let Some(qdrant) = &self.qdrant else {
            tracing::debug!("key-facts: skipped, no vector store");
            return Ok(Vec::new());
        };
        if !self.provider.supports_embeddings() {
            tracing::debug!("key-facts: skipped, no embedding support");
            return Ok(Vec::new());
        }

        let vector = self.provider.embed(query).await?;
        let vector_size = u64::try_from(vector.len()).unwrap_or(896);
        qdrant
            .ensure_named_collection(KEY_FACTS_COLLECTION, vector_size)
            .await?;

        let points = qdrant
            .search_collection(KEY_FACTS_COLLECTION, &vector, limit, None)
            .await?;

        tracing::debug!(results = points.len(), limit, "key-facts: search complete");

        let facts = points
            .into_iter()
            .filter_map(|p| p.payload.get("fact_text")?.as_str().map(String::from))
            .collect();

        Ok(facts)
    }

    /// Search a named document collection by semantic similarity.
    ///
    /// Returns up to `limit` scored vector points whose payloads contain ingested document chunks.
    /// Returns an empty vec when Qdrant is unavailable, the collection does not exist,
    /// or the provider does not support embeddings.
    ///
    /// # Errors
    ///
    /// Returns an error if embedding generation or Qdrant search fails.
    pub async fn search_document_collection(
        &self,
        collection: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<crate::ScoredVectorPoint>, MemoryError> {
        let Some(qdrant) = &self.qdrant else {
            return Ok(Vec::new());
        };
        if !self.provider.supports_embeddings() {
            return Ok(Vec::new());
        }
        if !qdrant.collection_exists(collection).await? {
            return Ok(Vec::new());
        }
        let vector = self.provider.embed(query).await?;
        let results = qdrant
            .search_collection(collection, &vector, limit, None)
            .await?;

        tracing::debug!(
            results = results.len(),
            limit,
            collection,
            "document-collection: search complete"
        );

        Ok(results)
    }
}
