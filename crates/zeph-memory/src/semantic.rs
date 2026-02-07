use zeph_llm::provider::{LlmProvider, Message, Role};

use crate::qdrant::{QdrantStore, SearchFilter};
use crate::sqlite::SqliteStore;

#[derive(Debug)]
pub struct RecalledMessage {
    pub message: Message,
    pub score: f32,
}

#[derive(Debug, Clone)]
pub struct Summary {
    pub id: i64,
    pub conversation_id: i64,
    pub content: String,
    pub first_message_id: i64,
    pub last_message_id: i64,
    pub token_estimate: i64,
}

/// Estimate token count using chars/4 heuristic.
#[must_use]
pub fn estimate_tokens(text: &str) -> usize {
    text.chars().count() / 4
}

fn build_summarization_prompt(messages: &[(i64, String, String)]) -> String {
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
    ) -> anyhow::Result<Self> {
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
        conversation_id: i64,
        role: &str,
        content: &str,
    ) -> anyhow::Result<i64> {
        let message_id = self
            .sqlite
            .save_message(conversation_id, role, content)
            .await?;

        if let Some(qdrant) = &self.qdrant
            && self.provider.supports_embeddings()
        {
            match self.provider.embed(content).await {
                Ok(vector) => {
                    if let Err(e) = qdrant
                        .store(
                            message_id,
                            conversation_id,
                            role,
                            vector,
                            false,
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
    ) -> anyhow::Result<Vec<RecalledMessage>> {
        let Some(qdrant) = &self.qdrant else {
            return Ok(Vec::new());
        };
        if !self.provider.supports_embeddings() {
            return Ok(Vec::new());
        }

        let query_vector = self.provider.embed(query).await?;

        let results = qdrant.search(&query_vector, limit, filter).await?;

        // TODO: Optimize N+1 query pattern by using SELECT WHERE id IN (...) for batch fetch
        let mut recalled = Vec::with_capacity(results.len());
        for result in results {
            if let Ok(Some(msg)) = self.sqlite.message_by_id(result.message_id).await {
                recalled.push(RecalledMessage {
                    message: msg,
                    score: result.score,
                });
            }
        }

        Ok(recalled)
    }

    /// Check whether an embedding exists for a given message ID.
    ///
    /// # Errors
    ///
    /// Returns an error if the `SQLite` query fails.
    pub async fn has_embedding(&self, message_id: i64) -> anyhow::Result<bool> {
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
    pub async fn embed_missing(&self) -> anyhow::Result<usize> {
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
                            false,
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
    pub async fn message_count(&self, conversation_id: i64) -> anyhow::Result<i64> {
        self.sqlite.count_messages(conversation_id).await
    }

    /// Load all summaries for a conversation.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub async fn load_summaries(&self, conversation_id: i64) -> anyhow::Result<Vec<Summary>> {
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
        conversation_id: i64,
        message_count: usize,
    ) -> anyhow::Result<Option<i64>> {
        let total = self.sqlite.count_messages(conversation_id).await?;

        if total <= i64::try_from(message_count)? {
            return Ok(None);
        }

        let after_id = self
            .sqlite
            .latest_summary_last_message_id(conversation_id)
            .await?
            .unwrap_or(0);

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
                    if let Err(e) = qdrant
                        .store(
                            summary_id,
                            conversation_id,
                            "system",
                            vector,
                            true,
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
        async fn chat(&self, _messages: &[Message]) -> anyhow::Result<String> {
            Ok("test response".into())
        }

        async fn chat_stream(&self, messages: &[Message]) -> anyhow::Result<ChatStream> {
            let response = self.chat(messages).await?;
            Ok(Box::pin(tokio_stream::once(Ok(response))))
        }

        fn supports_streaming(&self) -> bool {
            false
        }

        async fn embed(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
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

        assert_eq!(msg_id, 1);

        let history = memory.sqlite.load_history(cid, 50).await.unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].role, Role::User);
        assert_eq!(history[0].content, "hello");
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

        let has_embedding = memory.has_embedding(1).await.unwrap();
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
        assert_eq!(cid, 1);

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
}
