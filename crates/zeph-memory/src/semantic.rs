use qdrant_client::qdrant::Condition;
use zeph_llm::provider::{LlmProvider, Message, Role};

use crate::error::MemoryError;
use crate::qdrant::{Filter, MessageKind, QdrantStore, SearchFilter};
use crate::sqlite::SqliteStore;
use crate::types::{ConversationId, MessageId};

const SESSION_SUMMARIES_COLLECTION: &str = "zeph_session_summaries";

#[derive(Debug)]
pub struct RecalledMessage {
    pub message: Message,
    pub score: f32,
}

#[derive(Debug, Clone)]
pub struct Summary {
    pub id: i64,
    pub conversation_id: ConversationId,
    pub content: String,
    pub first_message_id: MessageId,
    pub last_message_id: MessageId,
    pub token_estimate: i64,
}

#[derive(Debug, Clone)]
pub struct SessionSummaryResult {
    pub summary_text: String,
    pub score: f32,
    pub conversation_id: ConversationId,
}

/// Estimate token count using chars/4 heuristic.
#[must_use]
pub fn estimate_tokens(text: &str) -> usize {
    text.chars().count() / 4
}

fn build_summarization_prompt(messages: &[(MessageId, String, String)]) -> String {
    let mut prompt = String::from(
        "Summarize the following conversation concisely. Preserve key facts, decisions, \
         and context that would be needed to continue the conversation. Be brief.\n\n\
         Conversation:\n",
    );

    for (_, role, content) in messages {
        prompt.push_str(role);
        prompt.push_str(": ");
        prompt.push_str(content);
        prompt.push('\n');
    }

    prompt.push_str("\nSummary:");
    prompt
}

pub struct SemanticMemory<P: LlmProvider> {
    sqlite: SqliteStore,
    qdrant: Option<QdrantStore>,
    provider: P,
    embedding_model: String,
}

impl<P: LlmProvider> SemanticMemory<P> {
    /// Create a new `SemanticMemory` instance.
    ///
    /// Qdrant connection is best-effort: if unavailable, semantic search is disabled.
    ///
    /// # Errors
    ///
    /// Returns an error if `SQLite` cannot be initialized.
    pub async fn new(
        sqlite_path: &str,
        qdrant_url: &str,
        provider: P,
        embedding_model: &str,
    ) -> Result<Self, MemoryError> {
        let sqlite = SqliteStore::new(sqlite_path).await?;
        let pool = sqlite.pool().clone();

        let qdrant = match QdrantStore::new(qdrant_url, pool) {
            Ok(store) => Some(store),
            Err(e) => {
                tracing::warn!("Qdrant unavailable, semantic search disabled: {e:#}");
                None
            }
        };

        Ok(Self {
            sqlite,
            qdrant,
            provider,
            embedding_model: embedding_model.into(),
        })
    }

    /// Save a message to `SQLite` and optionally embed and store in Qdrant.
    ///
    /// Returns the message ID assigned by `SQLite`.
    ///
    /// # Errors
    ///
    /// Returns an error if the `SQLite` save fails. Embedding failures are logged but not
    /// propagated.
    pub async fn remember(
        &self,
        conversation_id: ConversationId,
        role: &str,
        content: &str,
    ) -> Result<MessageId, MemoryError> {
        let message_id = self
            .sqlite
            .save_message(conversation_id, role, content)
            .await?;

        if let Some(qdrant) = &self.qdrant
            && self.provider.supports_embeddings()
        {
            match self.provider.embed(content).await {
                Ok(vector) => {
                    // Ensure collection exists before storing
                    let vector_size = u64::try_from(vector.len()).unwrap_or(896);
                    if let Err(e) = qdrant.ensure_collection(vector_size).await {
                        tracing::warn!("Failed to ensure Qdrant collection: {e:#}");
                    } else if let Err(e) = qdrant
                        .store(
                            message_id,
                            conversation_id,
                            role,
                            vector,
                            MessageKind::Regular,
                            &self.embedding_model,
                        )
                        .await
                    {
                        tracing::warn!("Failed to store embedding: {e:#}");
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to generate embedding: {e:#}");
                }
            }
        }

        Ok(message_id)
    }

    /// Save a message with pre-serialized parts JSON to `SQLite` and optionally embed in Qdrant.
    ///
    /// Returns `(message_id, embedding_stored)` tuple where `embedding_stored` is `true` if
    /// an embedding was successfully generated and stored in Qdrant.
    ///
    /// # Errors
    ///
    /// Returns an error if the `SQLite` save fails.
    pub async fn remember_with_parts(
        &self,
        conversation_id: ConversationId,
        role: &str,
        content: &str,
        parts_json: &str,
    ) -> Result<(MessageId, bool), MemoryError> {
        let message_id = self
            .sqlite
            .save_message_with_parts(conversation_id, role, content, parts_json)
            .await?;

        let mut embedding_stored = false;

        if let Some(qdrant) = &self.qdrant
            && self.provider.supports_embeddings()
        {
            match self.provider.embed(content).await {
                Ok(vector) => {
                    let vector_size = u64::try_from(vector.len()).unwrap_or(896);
                    if let Err(e) = qdrant.ensure_collection(vector_size).await {
                        tracing::warn!("Failed to ensure Qdrant collection: {e:#}");
                    } else if let Err(e) = qdrant
                        .store(
                            message_id,
                            conversation_id,
                            role,
                            vector,
                            MessageKind::Regular,
                            &self.embedding_model,
                        )
                        .await
                    {
                        tracing::warn!("Failed to store embedding: {e:#}");
                    } else {
                        embedding_stored = true;
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to generate embedding: {e:#}");
                }
            }
        }

        Ok((message_id, embedding_stored))
    }

    /// Recall semantically relevant messages based on a query string.
    ///
    /// # Errors
    ///
    /// Returns an error if embedding generation or Qdrant search fails.
    /// Returns empty vector if Qdrant unavailable or embeddings not supported.
    pub async fn recall(
        &self,
        query: &str,
        limit: usize,
        filter: Option<SearchFilter>,
    ) -> Result<Vec<RecalledMessage>, MemoryError> {
        let Some(qdrant) = &self.qdrant else {
            return Ok(Vec::new());
        };
        if !self.provider.supports_embeddings() {
            return Ok(Vec::new());
        }

        let query_vector = self.provider.embed(query).await?;

        // Ensure collection exists before searching
        let vector_size = u64::try_from(query_vector.len()).unwrap_or(896);
        qdrant.ensure_collection(vector_size).await?;

        let results = qdrant.search(&query_vector, limit, filter).await?;

        let ids: Vec<MessageId> = results.iter().map(|r| r.message_id).collect();
        let messages = self.sqlite.messages_by_ids(&ids).await?;
        let msg_map: std::collections::HashMap<MessageId, _> = messages.into_iter().collect();

        let recalled = results
            .iter()
            .filter_map(|r| {
                msg_map.get(&r.message_id).map(|msg| RecalledMessage {
                    message: msg.clone(),
                    score: r.score,
                })
            })
            .collect();

        Ok(recalled)
    }

    /// Check whether an embedding exists for a given message ID.
    ///
    /// # Errors
    ///
    /// Returns an error if the `SQLite` query fails.
    pub async fn has_embedding(&self, message_id: MessageId) -> Result<bool, MemoryError> {
        match &self.qdrant {
            Some(qdrant) => qdrant.has_embedding(message_id).await,
            None => Ok(false),
        }
    }

    /// Embed all messages that do not yet have embeddings.
    ///
    /// Returns the count of successfully embedded messages.
    ///
    /// # Errors
    ///
    /// Returns an error if collection initialization or database query fails.
    /// Individual embedding failures are logged but do not stop processing.
    pub async fn embed_missing(&self) -> Result<usize, MemoryError> {
        let Some(qdrant) = &self.qdrant else {
            return Ok(0);
        };
        if !self.provider.supports_embeddings() {
            return Ok(0);
        }

        let unembedded = self.sqlite.unembedded_message_ids(Some(1000)).await?;

        if unembedded.is_empty() {
            return Ok(0);
        }

        let probe = self.provider.embed("probe").await?;
        let vector_size = u64::try_from(probe.len())?;
        qdrant.ensure_collection(vector_size).await?;

        let mut count = 0;
        for (msg_id, conversation_id, role, content) in &unembedded {
            match self.provider.embed(content).await {
                Ok(vector) => {
                    if let Err(e) = qdrant
                        .store(
                            *msg_id,
                            *conversation_id,
                            role,
                            vector,
                            MessageKind::Regular,
                            &self.embedding_model,
                        )
                        .await
                    {
                        tracing::warn!("Failed to store embedding for msg {msg_id}: {e:#}");
                        continue;
                    }
                    count += 1;
                }
                Err(e) => {
                    tracing::warn!("Failed to embed msg {msg_id}: {e:#}");
                }
            }
        }

        tracing::info!("Embedded {count}/{} missing messages", unembedded.len());
        Ok(count)
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
            return Ok(Vec::new());
        };
        if !self.provider.supports_embeddings() {
            return Ok(Vec::new());
        }

        let vector = self.provider.embed(query).await?;
        let vector_size = u64::try_from(vector.len()).unwrap_or(896);
        qdrant
            .ensure_named_collection(SESSION_SUMMARIES_COLLECTION, vector_size)
            .await?;

        let filter = exclude_conversation_id
            .map(|cid| Filter::must_not(vec![Condition::matches("conversation_id", cid.0)]));

        let points = qdrant
            .search_collection(SESSION_SUMMARIES_COLLECTION, &vector, limit, filter)
            .await?;

        let results = points
            .into_iter()
            .filter_map(|point| {
                let payload = &point.payload;
                let summary_text = payload.get("summary_text")?.as_str()?.to_owned();
                let conversation_id = ConversationId(payload.get("conversation_id")?.as_integer()?);
                Some(SessionSummaryResult {
                    summary_text,
                    score: point.score,
                    conversation_id,
                })
            })
            .collect();

        Ok(results)
    }

    /// Access the underlying `SqliteStore` for operations that don't involve semantics.
    #[must_use]
    pub fn sqlite(&self) -> &SqliteStore {
        &self.sqlite
    }

    /// Check if Qdrant is available for semantic search.
    #[must_use]
    pub fn has_qdrant(&self) -> bool {
        self.qdrant.is_some()
    }

    /// Count messages in a conversation.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn message_count(&self, conversation_id: ConversationId) -> Result<i64, MemoryError> {
        self.sqlite.count_messages(conversation_id).await
    }

    /// Count messages not yet covered by any summary.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn unsummarized_message_count(
        &self,
        conversation_id: ConversationId,
    ) -> Result<i64, MemoryError> {
        let after_id = self
            .sqlite
            .latest_summary_last_message_id(conversation_id)
            .await?
            .unwrap_or(MessageId(0));
        self.sqlite
            .count_messages_after(conversation_id, after_id)
            .await
    }

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
        let summary_text = self
            .provider
            .chat(&[Message {
                role: Role::User,
                content: prompt,
                parts: vec![],
            }])
            .await?;

        let token_estimate = i64::try_from(estimate_tokens(&summary_text))?;
        let first_message_id = messages[0].0;
        let last_message_id = messages[messages.len() - 1].0;

        let summary_id = self
            .sqlite
            .save_summary(
                conversation_id,
                &summary_text,
                first_message_id,
                last_message_id,
                token_estimate,
            )
            .await?;

        if let Some(qdrant) = &self.qdrant
            && self.provider.supports_embeddings()
        {
            match self.provider.embed(&summary_text).await {
                Ok(vector) => {
                    // Ensure collection exists before storing
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

        Ok(Some(summary_id))
    }
}

#[cfg(test)]
mod tests {
    use zeph_llm::provider::{ChatStream, Role};

    use super::*;

    struct TestProvider {
        embedding: Vec<f32>,
        supports_embeddings: bool,
    }

    impl LlmProvider for TestProvider {
        async fn chat(&self, _messages: &[Message]) -> Result<String, zeph_llm::LlmError> {
            Ok("test response".into())
        }

        async fn chat_stream(
            &self,
            messages: &[Message],
        ) -> Result<ChatStream, zeph_llm::LlmError> {
            let response = self.chat(messages).await?;
            Ok(Box::pin(tokio_stream::once(Ok(response))))
        }

        fn supports_streaming(&self) -> bool {
            false
        }

        async fn embed(&self, _text: &str) -> Result<Vec<f32>, zeph_llm::LlmError> {
            Ok(self.embedding.clone())
        }

        fn supports_embeddings(&self) -> bool {
            self.supports_embeddings
        }

        fn name(&self) -> &'static str {
            "test"
        }
    }

    async fn test_semantic_memory(supports_embeddings: bool) -> SemanticMemory<TestProvider> {
        let provider = TestProvider {
            embedding: vec![0.1, 0.2, 0.3],
            supports_embeddings,
        };

        let sqlite = SqliteStore::new(":memory:").await.unwrap();

        SemanticMemory {
            sqlite,
            qdrant: None,
            provider,
            embedding_model: "test-model".into(),
        }
    }

    impl Clone for TestProvider {
        fn clone(&self) -> Self {
            Self {
                embedding: self.embedding.clone(),
                supports_embeddings: self.supports_embeddings,
            }
        }
    }

    #[tokio::test]
    async fn remember_saves_to_sqlite() {
        let memory = test_semantic_memory(false).await;

        let cid = memory.sqlite.create_conversation().await.unwrap();
        let msg_id = memory.remember(cid, "user", "hello").await.unwrap();

        assert_eq!(msg_id, MessageId(1));

        let history = memory.sqlite.load_history(cid, 50).await.unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].role, Role::User);
        assert_eq!(history[0].content, "hello");
    }

    #[tokio::test]
    async fn remember_with_parts_saves_parts_json() {
        let memory = test_semantic_memory(false).await;
        let cid = memory.sqlite.create_conversation().await.unwrap();

        let parts_json =
            r#"[{"kind":"ToolOutput","tool_name":"shell","body":"hello","compacted_at":null}]"#;
        let (msg_id, _embedding_stored) = memory
            .remember_with_parts(cid, "assistant", "tool output", parts_json)
            .await
            .unwrap();
        assert!(msg_id > MessageId(0));

        let history = memory.sqlite.load_history(cid, 50).await.unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].content, "tool output");
    }

    #[tokio::test]
    async fn recall_returns_empty_without_qdrant() {
        let memory = test_semantic_memory(true).await;

        let recalled = memory.recall("test", 5, None).await.unwrap();
        assert!(recalled.is_empty());
    }

    #[tokio::test]
    async fn has_embedding_without_qdrant() {
        let memory = test_semantic_memory(true).await;

        let has_embedding = memory.has_embedding(MessageId(1)).await.unwrap();
        assert!(!has_embedding);
    }

    #[tokio::test]
    async fn embed_missing_without_qdrant() {
        let memory = test_semantic_memory(true).await;

        let count = memory.embed_missing().await.unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn sqlite_accessor() {
        let memory = test_semantic_memory(false).await;

        let cid = memory.sqlite().create_conversation().await.unwrap();
        assert_eq!(cid, ConversationId(1));

        memory
            .sqlite()
            .save_message(cid, "user", "test")
            .await
            .unwrap();

        let history = memory.sqlite().load_history(cid, 50).await.unwrap();
        assert_eq!(history.len(), 1);
    }

    #[tokio::test]
    async fn has_qdrant_returns_false_when_unavailable() {
        let memory = test_semantic_memory(false).await;
        assert!(!memory.has_qdrant());
    }

    #[tokio::test]
    async fn recall_returns_empty_when_embeddings_not_supported() {
        let memory = test_semantic_memory(false).await;

        let recalled = memory.recall("test", 5, None).await.unwrap();
        assert!(recalled.is_empty());
    }

    #[tokio::test]
    async fn embed_missing_returns_zero_when_embeddings_not_supported() {
        let memory = test_semantic_memory(false).await;

        let cid = memory.sqlite().create_conversation().await.unwrap();
        memory
            .sqlite()
            .save_message(cid, "user", "test")
            .await
            .unwrap();

        let count = memory.embed_missing().await.unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn estimate_tokens_ascii() {
        let text = "Hello, world!";
        assert_eq!(estimate_tokens(text), 3);
    }

    #[test]
    fn estimate_tokens_unicode() {
        let text = "Привет мир";
        assert_eq!(estimate_tokens(text), 2);
    }

    #[test]
    fn estimate_tokens_empty() {
        assert_eq!(estimate_tokens(""), 0);
    }

    #[tokio::test]
    async fn message_count_empty_conversation() {
        let memory = test_semantic_memory(false).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        let count = memory.message_count(cid).await.unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn message_count_after_saves() {
        let memory = test_semantic_memory(false).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        memory.remember(cid, "user", "msg1").await.unwrap();
        memory.remember(cid, "assistant", "msg2").await.unwrap();

        let count = memory.message_count(cid).await.unwrap();
        assert_eq!(count, 2);
    }

    #[tokio::test]
    async fn unsummarized_count_decreases_after_summary() {
        let memory = test_semantic_memory(false).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        for i in 0..10 {
            memory
                .remember(cid, "user", &format!("msg{i}"))
                .await
                .unwrap();
        }
        assert_eq!(memory.unsummarized_message_count(cid).await.unwrap(), 10);

        memory.summarize(cid, 5).await.unwrap();

        assert!(memory.unsummarized_message_count(cid).await.unwrap() < 10);
        assert_eq!(memory.message_count(cid).await.unwrap(), 10);
    }

    #[tokio::test]
    async fn load_summaries_empty() {
        let memory = test_semantic_memory(false).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        let summaries = memory.load_summaries(cid).await.unwrap();
        assert!(summaries.is_empty());
    }

    #[tokio::test]
    async fn load_summaries_ordered() {
        let memory = test_semantic_memory(false).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        let msg_id1 = memory.remember(cid, "user", "m1").await.unwrap();
        let msg_id2 = memory.remember(cid, "assistant", "m2").await.unwrap();
        let msg_id3 = memory.remember(cid, "user", "m3").await.unwrap();

        let s1 = memory
            .sqlite()
            .save_summary(cid, "summary1", msg_id1, msg_id2, 3)
            .await
            .unwrap();
        let s2 = memory
            .sqlite()
            .save_summary(cid, "summary2", msg_id2, msg_id3, 3)
            .await
            .unwrap();

        let summaries = memory.load_summaries(cid).await.unwrap();
        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].id, s1);
        assert_eq!(summaries[0].content, "summary1");
        assert_eq!(summaries[1].id, s2);
        assert_eq!(summaries[1].content, "summary2");
    }

    #[tokio::test]
    async fn summarize_below_threshold() {
        let memory = test_semantic_memory(false).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        memory.remember(cid, "user", "hello").await.unwrap();

        let result = memory.summarize(cid, 10).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn summarize_stores_summary() {
        let memory = test_semantic_memory(false).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        for i in 0..5 {
            memory
                .remember(cid, "user", &format!("message {i}"))
                .await
                .unwrap();
        }

        let summary_id = memory.summarize(cid, 3).await.unwrap();
        assert!(summary_id.is_some());

        let summaries = memory.load_summaries(cid).await.unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].id, summary_id.unwrap());
        assert!(!summaries[0].content.is_empty());
    }

    #[tokio::test]
    async fn summarize_respects_previous_summaries() {
        let memory = test_semantic_memory(false).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        for i in 0..10 {
            memory
                .remember(cid, "user", &format!("message {i}"))
                .await
                .unwrap();
        }

        let s1 = memory.summarize(cid, 3).await.unwrap();
        assert!(s1.is_some());

        let s2 = memory.summarize(cid, 3).await.unwrap();
        assert!(s2.is_some());

        let summaries = memory.load_summaries(cid).await.unwrap();
        assert_eq!(summaries.len(), 2);
        assert!(summaries[0].last_message_id < summaries[1].first_message_id);
    }

    #[tokio::test]
    async fn remember_multiple_messages_increments_ids() {
        let memory = test_semantic_memory(false).await;
        let cid = memory.sqlite.create_conversation().await.unwrap();

        let id1 = memory.remember(cid, "user", "first").await.unwrap();
        let id2 = memory.remember(cid, "assistant", "second").await.unwrap();
        let id3 = memory.remember(cid, "user", "third").await.unwrap();

        assert!(id1 < id2);
        assert!(id2 < id3);
    }

    #[tokio::test]
    async fn message_count_across_conversations() {
        let memory = test_semantic_memory(false).await;
        let cid1 = memory.sqlite().create_conversation().await.unwrap();
        let cid2 = memory.sqlite().create_conversation().await.unwrap();

        memory.remember(cid1, "user", "msg1").await.unwrap();
        memory.remember(cid1, "user", "msg2").await.unwrap();
        memory.remember(cid2, "user", "msg3").await.unwrap();

        assert_eq!(memory.message_count(cid1).await.unwrap(), 2);
        assert_eq!(memory.message_count(cid2).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn summarize_exact_threshold_returns_none() {
        let memory = test_semantic_memory(false).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        for i in 0..3 {
            memory
                .remember(cid, "user", &format!("msg {i}"))
                .await
                .unwrap();
        }

        let result = memory.summarize(cid, 3).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn summarize_one_above_threshold_produces_summary() {
        let memory = test_semantic_memory(false).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        for i in 0..4 {
            memory
                .remember(cid, "user", &format!("msg {i}"))
                .await
                .unwrap();
        }

        let result = memory.summarize(cid, 3).await.unwrap();
        assert!(result.is_some());
    }

    #[tokio::test]
    async fn summary_fields_populated() {
        let memory = test_semantic_memory(false).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        for i in 0..5 {
            memory
                .remember(cid, "user", &format!("msg {i}"))
                .await
                .unwrap();
        }

        memory.summarize(cid, 3).await.unwrap();
        let summaries = memory.load_summaries(cid).await.unwrap();
        let s = &summaries[0];

        assert_eq!(s.conversation_id, cid);
        assert!(s.first_message_id > MessageId(0));
        assert!(s.last_message_id >= s.first_message_id);
        assert!(s.token_estimate >= 0);
        assert!(!s.content.is_empty());
    }

    #[test]
    fn build_summarization_prompt_format() {
        let messages = vec![
            (MessageId(1), "user".into(), "Hello".into()),
            (MessageId(2), "assistant".into(), "Hi there".into()),
        ];
        let prompt = build_summarization_prompt(&messages);
        assert!(prompt.contains("user: Hello"));
        assert!(prompt.contains("assistant: Hi there"));
        assert!(prompt.contains("Summary:"));
    }

    #[test]
    fn build_summarization_prompt_empty() {
        let messages: Vec<(MessageId, String, String)> = vec![];
        let prompt = build_summarization_prompt(&messages);
        assert!(prompt.contains("Summary:"));
    }

    #[test]
    fn recalled_message_debug() {
        let recalled = RecalledMessage {
            message: Message {
                role: Role::User,
                content: "test".into(),
                parts: vec![],
            },
            score: 0.95,
        };
        let dbg = format!("{recalled:?}");
        assert!(dbg.contains("RecalledMessage"));
        assert!(dbg.contains("0.95"));
    }

    #[test]
    fn summary_clone() {
        let summary = Summary {
            id: 1,
            conversation_id: ConversationId(2),
            content: "test summary".into(),
            first_message_id: MessageId(1),
            last_message_id: MessageId(5),
            token_estimate: 10,
        };
        let cloned = summary.clone();
        assert_eq!(summary.id, cloned.id);
        assert_eq!(summary.content, cloned.content);
    }

    #[test]
    fn estimate_tokens_short_text() {
        assert_eq!(estimate_tokens("ab"), 0);
    }

    #[test]
    fn estimate_tokens_longer_text() {
        let text = "a".repeat(100);
        assert_eq!(estimate_tokens(&text), 25);
    }

    #[tokio::test]
    async fn remember_preserves_role_mapping() {
        let memory = test_semantic_memory(false).await;
        let cid = memory.sqlite.create_conversation().await.unwrap();

        memory.remember(cid, "user", "u").await.unwrap();
        memory.remember(cid, "assistant", "a").await.unwrap();
        memory.remember(cid, "system", "s").await.unwrap();

        let history = memory.sqlite.load_history(cid, 50).await.unwrap();
        assert_eq!(history.len(), 3);
        assert_eq!(history[0].role, Role::User);
        assert_eq!(history[1].role, Role::Assistant);
        assert_eq!(history[2].role, Role::System);
    }

    #[tokio::test]
    async fn new_with_invalid_qdrant_url_graceful() {
        let provider = TestProvider {
            embedding: vec![0.1, 0.2, 0.3],
            supports_embeddings: true,
        };
        let result =
            SemanticMemory::new(":memory:", "http://127.0.0.1:1", provider, "test-model").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn remember_with_embeddings_supported_but_no_qdrant() {
        let memory = test_semantic_memory(true).await;
        let cid = memory.sqlite.create_conversation().await.unwrap();

        let msg_id = memory.remember(cid, "user", "hello embed").await.unwrap();
        assert!(msg_id > MessageId(0));

        let history = memory.sqlite.load_history(cid, 50).await.unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].content, "hello embed");
    }

    #[tokio::test]
    async fn remember_verifies_content_via_load_history() {
        let memory = test_semantic_memory(false).await;
        let cid = memory.sqlite.create_conversation().await.unwrap();

        memory.remember(cid, "user", "alpha").await.unwrap();
        memory.remember(cid, "assistant", "beta").await.unwrap();
        memory.remember(cid, "user", "gamma").await.unwrap();

        let history = memory.sqlite().load_history(cid, 50).await.unwrap();
        assert_eq!(history.len(), 3);
        assert_eq!(history[0].content, "alpha");
        assert_eq!(history[1].content, "beta");
        assert_eq!(history[2].content, "gamma");
    }

    #[tokio::test]
    async fn message_count_multiple_conversations_isolated() {
        let memory = test_semantic_memory(false).await;
        let cid1 = memory.sqlite().create_conversation().await.unwrap();
        let cid2 = memory.sqlite().create_conversation().await.unwrap();
        let cid3 = memory.sqlite().create_conversation().await.unwrap();

        for _ in 0..5 {
            memory.remember(cid1, "user", "msg").await.unwrap();
        }
        for _ in 0..3 {
            memory.remember(cid2, "user", "msg").await.unwrap();
        }

        assert_eq!(memory.message_count(cid1).await.unwrap(), 5);
        assert_eq!(memory.message_count(cid2).await.unwrap(), 3);
        assert_eq!(memory.message_count(cid3).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn summarize_empty_messages_range_returns_none() {
        let memory = test_semantic_memory(false).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        for i in 0..6 {
            memory
                .remember(cid, "user", &format!("msg {i}"))
                .await
                .unwrap();
        }

        memory.summarize(cid, 3).await.unwrap();
        memory.summarize(cid, 3).await.unwrap();

        let summaries = memory.load_summaries(cid).await.unwrap();
        assert_eq!(summaries.len(), 2);
    }

    #[tokio::test]
    async fn summarize_token_estimate_populated() {
        let memory = test_semantic_memory(false).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        for i in 0..5 {
            memory
                .remember(cid, "user", &format!("message {i}"))
                .await
                .unwrap();
        }

        memory.summarize(cid, 3).await.unwrap();
        let summaries = memory.load_summaries(cid).await.unwrap();
        let token_est = summaries[0].token_estimate;
        let expected = i64::try_from(estimate_tokens(&summaries[0].content)).unwrap();
        assert_eq!(token_est, expected);
    }

    struct FailChatProvider;

    impl Clone for FailChatProvider {
        fn clone(&self) -> Self {
            Self
        }
    }

    impl LlmProvider for FailChatProvider {
        async fn chat(&self, _messages: &[Message]) -> Result<String, zeph_llm::LlmError> {
            Err(zeph_llm::LlmError::Other("chat failed".into()))
        }

        async fn chat_stream(
            &self,
            _messages: &[Message],
        ) -> Result<ChatStream, zeph_llm::LlmError> {
            Err(zeph_llm::LlmError::Other("stream failed".into()))
        }

        fn supports_streaming(&self) -> bool {
            false
        }

        async fn embed(&self, _text: &str) -> Result<Vec<f32>, zeph_llm::LlmError> {
            Err(zeph_llm::LlmError::Other("embed not supported".into()))
        }

        fn supports_embeddings(&self) -> bool {
            false
        }

        fn name(&self) -> &'static str {
            "fail"
        }
    }

    #[tokio::test]
    async fn summarize_fails_when_provider_chat_fails() {
        let sqlite = SqliteStore::new(":memory:").await.unwrap();
        let memory = SemanticMemory {
            sqlite,
            qdrant: None,
            provider: FailChatProvider,
            embedding_model: "test".into(),
        };
        let cid = memory.sqlite().create_conversation().await.unwrap();

        for i in 0..5 {
            memory
                .remember(cid, "user", &format!("msg {i}"))
                .await
                .unwrap();
        }

        let result = memory.summarize(cid, 3).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn embed_missing_without_embedding_support_returns_zero() {
        let memory = test_semantic_memory(false).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();
        memory
            .sqlite()
            .save_message(cid, "user", "test message")
            .await
            .unwrap();

        let count = memory.embed_missing().await.unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn has_embedding_returns_false_when_no_qdrant() {
        let memory = test_semantic_memory(false).await;
        let cid = memory.sqlite.create_conversation().await.unwrap();
        let msg_id = memory.remember(cid, "user", "test").await.unwrap();
        assert!(!memory.has_embedding(msg_id).await.unwrap());
    }

    #[tokio::test]
    async fn recall_empty_without_qdrant_regardless_of_filter() {
        let memory = test_semantic_memory(true).await;
        let filter = SearchFilter {
            conversation_id: Some(ConversationId(1)),
            role: None,
        };
        let recalled = memory.recall("query", 10, Some(filter)).await.unwrap();
        assert!(recalled.is_empty());
    }

    #[tokio::test]
    async fn summarize_message_range_bounds() {
        let memory = test_semantic_memory(false).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        for i in 0..8 {
            memory
                .remember(cid, "user", &format!("msg {i}"))
                .await
                .unwrap();
        }

        let summary_id = memory.summarize(cid, 4).await.unwrap().unwrap();
        let summaries = memory.load_summaries(cid).await.unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].id, summary_id);
        assert!(summaries[0].first_message_id >= MessageId(1));
        assert!(summaries[0].last_message_id >= summaries[0].first_message_id);
    }

    #[test]
    fn build_summarization_prompt_preserves_order() {
        let messages = vec![
            (MessageId(1), "user".into(), "first".into()),
            (MessageId(2), "assistant".into(), "second".into()),
            (MessageId(3), "user".into(), "third".into()),
        ];
        let prompt = build_summarization_prompt(&messages);
        let first_pos = prompt.find("user: first").unwrap();
        let second_pos = prompt.find("assistant: second").unwrap();
        let third_pos = prompt.find("user: third").unwrap();
        assert!(first_pos < second_pos);
        assert!(second_pos < third_pos);
    }

    #[test]
    fn summary_debug() {
        let summary = Summary {
            id: 1,
            conversation_id: ConversationId(2),
            content: "test".into(),
            first_message_id: MessageId(1),
            last_message_id: MessageId(5),
            token_estimate: 10,
        };
        let dbg = format!("{summary:?}");
        assert!(dbg.contains("Summary"));
    }

    #[tokio::test]
    async fn message_count_nonexistent_conversation() {
        let memory = test_semantic_memory(false).await;
        let count = memory.message_count(ConversationId(999)).await.unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn load_summaries_nonexistent_conversation() {
        let memory = test_semantic_memory(false).await;
        let summaries = memory.load_summaries(ConversationId(999)).await.unwrap();
        assert!(summaries.is_empty());
    }

    #[tokio::test]
    async fn store_session_summary_no_qdrant_noop() {
        let memory = test_semantic_memory(true).await;
        let result = memory
            .store_session_summary(ConversationId(1), "test summary")
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn store_session_summary_no_embeddings_noop() {
        let memory = test_semantic_memory(false).await;
        let result = memory
            .store_session_summary(ConversationId(1), "test summary")
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn search_session_summaries_no_qdrant_empty() {
        let memory = test_semantic_memory(true).await;
        let results = memory
            .search_session_summaries("query", 5, None)
            .await
            .unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn search_session_summaries_no_embeddings_empty() {
        let memory = test_semantic_memory(false).await;
        let results = memory
            .search_session_summaries("query", 5, Some(ConversationId(1)))
            .await
            .unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn session_summary_result_debug() {
        let result = SessionSummaryResult {
            summary_text: "test".into(),
            score: 0.9,
            conversation_id: ConversationId(1),
        };
        let dbg = format!("{result:?}");
        assert!(dbg.contains("SessionSummaryResult"));
    }

    #[test]
    fn session_summary_result_clone() {
        let result = SessionSummaryResult {
            summary_text: "test".into(),
            score: 0.9,
            conversation_id: ConversationId(1),
        };
        let cloned = result.clone();
        assert_eq!(result.summary_text, cloned.summary_text);
        assert_eq!(result.conversation_id, cloned.conversation_id);
    }
}
