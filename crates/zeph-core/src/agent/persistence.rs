use crate::channel::Channel;
use zeph_llm::provider::Role;
use zeph_memory::sqlite::role_str;

use super::Agent;

impl<C: Channel> Agent<C> {
    /// Load conversation history from memory and inject into messages.
    ///
    /// # Errors
    ///
    /// Returns an error if loading history from `SQLite` fails.
    pub async fn load_history(&mut self) -> Result<(), super::error::AgentError> {
        let (Some(memory), Some(cid)) =
            (&self.memory_state.memory, self.memory_state.conversation_id)
        else {
            return Ok(());
        };

        let history = memory
            .sqlite()
            .load_history(cid, self.memory_state.history_limit)
            .await?;
        if !history.is_empty() {
            let mut loaded = 0;
            let mut skipped = 0;

            for msg in history {
                if msg.content.trim().is_empty() {
                    tracing::warn!("skipping empty message from history (role: {:?})", msg.role);
                    skipped += 1;
                    continue;
                }
                self.messages.push(msg);
                loaded += 1;
            }

            tracing::info!("restored {loaded} message(s) from conversation {cid}");
            if skipped > 0 {
                tracing::warn!("skipped {skipped} empty message(s) from history");
            }
        }

        if let Ok(count) = memory.message_count(cid).await {
            let count_u64 = u64::try_from(count).unwrap_or(0);
            self.update_metrics(|m| {
                m.sqlite_message_count = count_u64;
            });
        }

        self.recompute_prompt_tokens();
        Ok(())
    }

    pub(crate) async fn persist_message(&mut self, role: Role, content: &str) {
        let (Some(memory), Some(cid)) =
            (&self.memory_state.memory, self.memory_state.conversation_id)
        else {
            return;
        };

        let parts_json = self
            .messages
            .last()
            .filter(|m| !m.parts.is_empty())
            .and_then(|m| serde_json::to_string(&m.parts).ok())
            .unwrap_or_else(|| "[]".to_string());

        let (_message_id, embedding_stored) = match memory
            .remember_with_parts(cid, role_str(role), content, &parts_json)
            .await
        {
            Ok(result) => result,
            Err(e) => {
                tracing::error!("failed to persist message: {e:#}");
                return;
            }
        };

        self.update_metrics(|m| {
            m.sqlite_message_count += 1;
            if embedding_stored {
                m.embeddings_generated += 1;
            }
        });

        self.check_summarization().await;
    }

    pub(crate) async fn check_summarization(&mut self) {
        let (Some(memory), Some(cid)) =
            (&self.memory_state.memory, self.memory_state.conversation_id)
        else {
            return;
        };

        let count = match memory.unsummarized_message_count(cid).await {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("failed to get unsummarized message count: {e:#}");
                return;
            }
        };

        let count_usize = match usize::try_from(count) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("message count overflow: {e:#}");
                return;
            }
        };

        if count_usize > self.memory_state.summarization_threshold {
            let _ = self.channel.send_status("summarizing...").await;
            let batch_size = self.memory_state.summarization_threshold / 2;
            match memory.summarize(cid, batch_size).await {
                Ok(Some(summary_id)) => {
                    tracing::info!("created summary {summary_id} for conversation {cid}");
                    self.update_metrics(|m| {
                        m.summaries_count += 1;
                    });
                }
                Ok(None) => {
                    tracing::debug!("no summarization needed");
                }
                Err(e) => {
                    tracing::error!("summarization failed: {e:#}");
                }
            }
            let _ = self.channel.send_status("").await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::agent_tests::{
        MockChannel, MockToolExecutor, create_test_registry, mock_provider,
    };
    use super::*;
    use zeph_llm::any::AnyProvider;
    use zeph_memory::semantic::SemanticMemory;

    async fn test_memory(provider: &AnyProvider) -> SemanticMemory {
        SemanticMemory::new(
            ":memory:",
            "http://127.0.0.1:1",
            provider.clone(),
            "test-model",
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn load_history_without_memory_returns_ok() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        let result = agent.load_history().await;
        assert!(result.is_ok());
        // No messages added when no memory is configured
        assert_eq!(agent.messages.len(), 1); // system prompt only
    }

    #[tokio::test]
    async fn load_history_with_messages_injects_into_agent() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let memory = test_memory(&AnyProvider::Mock(zeph_llm::mock::MockProvider::default())).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        memory
            .sqlite()
            .save_message(cid, "user", "hello from history")
            .await
            .unwrap();
        memory
            .sqlite()
            .save_message(cid, "assistant", "hi back")
            .await
            .unwrap();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_memory(memory, cid, 50, 5, 100);

        let messages_before = agent.messages.len();
        agent.load_history().await.unwrap();
        // Two messages were added from history
        assert_eq!(agent.messages.len(), messages_before + 2);
    }

    #[tokio::test]
    async fn load_history_skips_empty_messages() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let memory = test_memory(&AnyProvider::Mock(zeph_llm::mock::MockProvider::default())).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        // Save an empty message (should be skipped) and a valid one
        memory
            .sqlite()
            .save_message(cid, "user", "   ")
            .await
            .unwrap();
        memory
            .sqlite()
            .save_message(cid, "user", "real message")
            .await
            .unwrap();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_memory(memory, cid, 50, 5, 100);

        let messages_before = agent.messages.len();
        agent.load_history().await.unwrap();
        // Only the non-empty message is loaded
        assert_eq!(agent.messages.len(), messages_before + 1);
    }

    #[tokio::test]
    async fn load_history_with_empty_store_returns_ok() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let memory = test_memory(&AnyProvider::Mock(zeph_llm::mock::MockProvider::default())).await;
        let cid = memory.sqlite().create_conversation().await.unwrap();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
            .with_memory(memory, cid, 50, 5, 100);

        let messages_before = agent.messages.len();
        agent.load_history().await.unwrap();
        // No messages added — empty history
        assert_eq!(agent.messages.len(), messages_before);
    }

    #[tokio::test]
    async fn persist_message_without_memory_silently_returns() {
        // No memory configured — persist_message must not panic
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        // Must not panic and must complete
        agent.persist_message(Role::User, "hello").await;
    }
}
