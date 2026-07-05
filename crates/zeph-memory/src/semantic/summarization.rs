// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use zeph_llm::provider::{LlmProvider as _, Message, MessageMetadata, Role};

use super::{KEY_FACTS_COLLECTION, SemanticMemory};
use crate::embedding_store::MessageKind;
use crate::error::MemoryError;
use crate::types::{ConversationId, MessageId};
use crate::vector_store::{FieldCondition, FieldValue, VectorFilter};

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

/// Outcome of a successful [`SemanticMemory::summarize`] call.
#[derive(Debug, Clone, Copy)]
pub struct SummarizeOutcome {
    /// Row id of the newly created summary.
    pub summary_id: i64,
    /// Number of messages actually folded into this summary (the size of the
    /// unsummarized range consumed, not the `message_count` argument requested).
    pub messages_folded: usize,
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
    /// [`SummarizeOutcome::messages_folded`] reflects the actual number of messages folded
    /// into the new summary, which may be less than `message_count` when fewer unsummarized
    /// messages exist.
    ///
    /// # Errors
    ///
    /// Returns an error if LLM call or database operation fails.
    #[tracing::instrument(name = "memory.summarize", skip_all, fields(input_msgs = %message_count, output_len = tracing::field::Empty))]
    pub async fn summarize(
        &self,
        conversation_id: ConversationId,
        message_count: usize,
    ) -> Result<Option<SummarizeOutcome>, MemoryError> {
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

        let messages_folded = messages.len();
        let prompt = build_summarization_prompt(&messages);
        let chat_messages = vec![Message {
            role: Role::User,
            content: prompt,
            parts: vec![],
            metadata: MessageMetadata::default(),
        }];

        let structured = self.call_summarization_llm(&chat_messages).await?;
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
            && self.effective_embed_provider().supports_embeddings()
        {
            match tokio::time::timeout(
                self.embed_timeout,
                self.effective_embed_provider().embed(summary_text),
            )
            .await
            {
                Ok(Ok(vector)) => {
                    if let Err(e) = qdrant.ensure_collection_for_vector(&vector).await {
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
                Ok(Err(e)) => {
                    tracing::warn!("Failed to generate summary embedding: {e:#}");
                }
                Err(_) => {
                    tracing::warn!("summarize: embed timed out for summary text — skipping store");
                }
            }
        }

        if !structured.key_facts.is_empty() {
            self.store_key_facts(conversation_id, summary_id, &structured.key_facts)
                .await;
        }

        Ok(Some(SummarizeOutcome {
            summary_id,
            messages_folded,
        }))
    }

    /// Call the LLM to produce a [`StructuredSummary`], falling back to plain text on parse error.
    ///
    /// Both the structured and fallback calls are bounded by `summarization_llm_timeout_secs`.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError::Timeout`] if the LLM exceeds the deadline, or
    /// [`MemoryError::Llm`] if the provider returns an error.
    async fn call_summarization_llm(
        &self,
        chat_messages: &[Message],
    ) -> Result<StructuredSummary, MemoryError> {
        let timeout_secs = self.summarization_llm_timeout_secs;
        let timeout = std::time::Duration::from_secs(timeout_secs);
        match tokio::time::timeout(
            timeout,
            self.provider
                .chat_typed_erased::<StructuredSummary>(chat_messages),
        )
        .await
        {
            Ok(Ok(s)) => Ok(s),
            Ok(Err(e)) => {
                tracing::warn!(
                    "structured summarization failed, falling back to plain text: {e:#}"
                );
                match tokio::time::timeout(timeout, self.provider.chat(chat_messages)).await {
                    Ok(Ok(plain)) => Ok(StructuredSummary {
                        summary: plain,
                        key_facts: vec![],
                        entities: vec![],
                    }),
                    Ok(Err(e)) => Err(MemoryError::Llm(e)),
                    Err(_elapsed) => {
                        tracing::warn!(
                            "summarization: plain text fallback LLM call timed out after {timeout_secs}s"
                        );
                        Err(MemoryError::Timeout("LLM call timed out".into()))
                    }
                }
            }
            Err(_elapsed) => {
                tracing::warn!(
                    "summarization: structured LLM call timed out after {timeout_secs}s"
                );
                Err(MemoryError::Timeout("LLM call timed out".into()))
            }
        }
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
        if !self.effective_embed_provider().supports_embeddings() {
            return;
        }

        // Filter out transient policy-decision facts that describe a blocked or denied action.
        // These reflect the agent's state at a single point in time and must not be recalled
        // as stable world facts in future turns — doing so causes the agent to skip valid calls.
        let filtered: Vec<&str> = key_facts
            .iter()
            .filter(|f| !is_policy_decision_fact(f.as_str()))
            .map(String::as_str)
            .collect();

        let Some(first_fact) = filtered.first().copied() else {
            return;
        };
        let first_vector = match tokio::time::timeout(
            self.embed_timeout,
            self.effective_embed_provider().embed(first_fact),
        )
        .await
        {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                tracing::warn!("Failed to embed key fact: {e:#}");
                return;
            }
            Err(_) => {
                tracing::warn!("store_key_facts: embed timed out for first fact — skipping");
                return;
            }
        };
        if let Err(e) = qdrant
            .ensure_named_collection_for_vector(KEY_FACTS_COLLECTION, &first_vector)
            .await
        {
            tracing::warn!("Failed to ensure key_facts collection: {e:#}");
            return;
        }

        let threshold = self.key_facts_dedup_threshold;
        self.store_key_fact_if_unique(
            qdrant,
            conversation_id,
            source_summary_id,
            first_fact,
            first_vector,
            threshold,
        )
        .await;

        for fact in filtered[1..].iter().copied() {
            match tokio::time::timeout(
                self.embed_timeout,
                self.effective_embed_provider().embed(fact),
            )
            .await
            {
                Ok(Ok(vector)) => {
                    self.store_key_fact_if_unique(
                        qdrant,
                        conversation_id,
                        source_summary_id,
                        fact,
                        vector,
                        threshold,
                    )
                    .await;
                }
                Ok(Err(e)) => {
                    tracing::warn!("Failed to embed key fact: {e:#}");
                }
                Err(_) => {
                    tracing::warn!("store_key_facts: embed timed out for fact — skipping");
                }
            }
        }
    }

    async fn store_key_fact_if_unique(
        &self,
        qdrant: &crate::embedding_store::EmbeddingStore,
        conversation_id: ConversationId,
        source_summary_id: i64,
        fact: &str,
        vector: Vec<f32>,
        threshold: f32,
    ) {
        // Scope the near-duplicate check to this conversation, matching the read-side filter in
        // `search_key_facts`. An unscoped (global) dedup search would let a fact stored under one
        // conversation silently suppress a near-identical fact for a different conversation, which
        // is then unrecoverable from that other conversation's conversation-scoped search (#5732).
        let dedup_filter = Some(VectorFilter {
            must: vec![
                FieldCondition {
                    field: "conversation_id".into(),
                    value: FieldValue::Integer(conversation_id.0),
                },
                FieldCondition {
                    field: "db_instance_id".into(),
                    value: FieldValue::Text(qdrant.db_instance_id().to_owned()),
                },
            ],
            must_not: vec![],
        });
        match qdrant
            .search_collection(KEY_FACTS_COLLECTION, &vector, 1, dedup_filter)
            .await
        {
            Ok(hits) if hits.first().is_some_and(|h| h.score >= threshold) => {
                tracing::debug!(
                    score = hits[0].score,
                    threshold,
                    "key-facts: skipping near-duplicate fact"
                );
                return;
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!("key-facts: dedup search failed, storing anyway: {e:#}");
            }
        }

        let payload = serde_json::json!({
            "conversation_id": conversation_id.0,
            "db_instance_id": qdrant.db_instance_id(),
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

    /// Search key facts extracted from conversation summaries.
    ///
    /// When `conversation_id` is `Some`, results are restricted to facts scoped to that
    /// conversation; facts written without a `conversation_id` payload field (e.g. cross-session
    /// episodic-consolidation facts, or points written before this scoping was introduced) will
    /// not match. Pass `None` to search across all conversations.
    ///
    /// # Errors
    ///
    /// Returns an error if embedding or Qdrant search fails.
    pub async fn search_key_facts(
        &self,
        query: &str,
        limit: usize,
        conversation_id: Option<ConversationId>,
    ) -> Result<Vec<String>, MemoryError> {
        let Some(qdrant) = &self.qdrant else {
            tracing::debug!("key-facts: skipped, no vector store");
            return Ok(Vec::new());
        };
        if !self.effective_embed_provider().supports_embeddings() {
            tracing::debug!("key-facts: skipped, no embedding support");
            return Ok(Vec::new());
        }

        let vector = match tokio::time::timeout(
            self.embed_timeout,
            self.effective_embed_provider().embed(query),
        )
        .await
        {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => return Err(e.into()),
            Err(_) => {
                tracing::warn!("search_key_facts: embed timed out, returning empty results");
                return Ok(Vec::new());
            }
        };
        qdrant
            .ensure_named_collection_for_vector(KEY_FACTS_COLLECTION, &vector)
            .await?;

        let filter = conversation_id.map(|cid| VectorFilter {
            must: vec![
                FieldCondition {
                    field: "conversation_id".into(),
                    value: FieldValue::Integer(cid.0),
                },
                FieldCondition {
                    field: "db_instance_id".into(),
                    value: FieldValue::Text(qdrant.db_instance_id().to_owned()),
                },
            ],
            must_not: vec![],
        });

        let points = qdrant
            .search_collection(KEY_FACTS_COLLECTION, &vector, limit, filter)
            .await?;

        tracing::debug!(
            results = points.len(),
            limit,
            conversation_id = conversation_id.map(|c| c.0),
            "key-facts: search complete"
        );

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
        if !self.effective_embed_provider().supports_embeddings() {
            return Ok(Vec::new());
        }
        if !qdrant.collection_exists(collection).await? {
            return Ok(Vec::new());
        }
        let vector = match tokio::time::timeout(
            self.embed_timeout,
            self.effective_embed_provider().embed(query),
        )
        .await
        {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => return Err(e.into()),
            Err(_) => {
                tracing::warn!(
                    "search_document_collection: embed timed out, returning empty results"
                );
                return Ok(Vec::new());
            }
        };
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

/// Returns `true` when a fact string describes a transient policy or permission decision.
///
/// Facts like "reading /etc/passwd was blocked by utility policy" are snapshots of a
/// single-turn enforcement state and must not be recalled as durable world knowledge.
/// Storing them causes the agent to believe a tool is permanently unavailable.
pub(crate) fn is_policy_decision_fact(fact: &str) -> bool {
    const MARKERS: &[&str] = &[
        "blocked",
        "skipped",
        "cannot access",
        "security polic",
        "utility polic",
        "not allowed",
        "permission denied",
        "access denied",
        "was denied",
    ];
    let lower = fact.to_lowercase();
    MARKERS.iter().any(|m| lower.contains(m))
}
