// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use zeph_llm::provider::{LlmProvider, Message, MessagePart, Role};

use super::{Agent, CODE_CONTEXT_PREFIX};
use crate::channel::Channel;
use crate::metrics::{MetricsSnapshot, SECURITY_EVENT_CAP, SecurityEvent, SecurityEventCategory};

impl<C: Channel> Agent<C> {
    /// Read the community-detection failure counter from `SemanticMemory` and update metrics.
    #[cfg(feature = "graph-memory")]
    pub fn sync_community_detection_failures(&self) {
        if let Some(memory) = self.memory_state.memory.as_ref() {
            let failures = memory.community_detection_failures();
            self.update_metrics(|m| {
                m.graph_community_detection_failures = failures;
            });
        }
    }

    /// Sync all graph counters (extraction count/failures) from `SemanticMemory` to metrics.
    #[cfg(feature = "graph-memory")]
    pub fn sync_graph_extraction_metrics(&self) {
        if let Some(memory) = self.memory_state.memory.as_ref() {
            let count = memory.graph_extraction_count();
            let failures = memory.graph_extraction_failures();
            self.update_metrics(|m| {
                m.graph_extraction_count = count;
                m.graph_extraction_failures = failures;
            });
        }
    }

    /// Fetch entity/edge/community counts from the graph store and write to metrics.
    #[cfg(feature = "graph-memory")]
    pub async fn sync_graph_counts(&self) {
        let Some(memory) = self.memory_state.memory.as_ref() else {
            return;
        };
        let Some(store) = memory.graph_store.as_ref() else {
            return;
        };
        let (entities, edges, communities) = tokio::join!(
            store.entity_count(),
            store.active_edge_count(),
            store.community_count()
        );
        self.update_metrics(|m| {
            m.graph_entities_total = entities.unwrap_or(0).cast_unsigned();
            m.graph_edges_total = edges.unwrap_or(0).cast_unsigned();
            m.graph_communities_total = communities.unwrap_or(0).cast_unsigned();
        });
    }

    /// Perform a real health check on the vector store and update metrics.
    pub async fn check_vector_store_health(&self, backend_name: &str) {
        let connected = match self.memory_state.memory.as_ref() {
            Some(m) => m.is_vector_store_connected().await,
            None => false,
        };
        let name = backend_name.to_owned();
        self.update_metrics(|m| {
            m.qdrant_available = connected;
            m.vector_backend = name;
        });
    }

    pub(super) fn update_metrics(&self, f: impl FnOnce(&mut MetricsSnapshot)) {
        if let Some(ref tx) = self.metrics_tx {
            let elapsed = self.start_time.elapsed().as_secs();
            tx.send_modify(|m| {
                m.uptime_seconds = elapsed;
                f(m);
            });
        }
    }

    pub(super) fn push_security_event(
        &self,
        category: SecurityEventCategory,
        source: &str,
        detail: impl Into<String>,
    ) {
        if let Some(ref tx) = self.metrics_tx {
            let event = SecurityEvent::new(category, source, detail);
            let elapsed = self.start_time.elapsed().as_secs();
            tx.send_modify(|m| {
                m.uptime_seconds = elapsed;
                if m.security_events.len() >= SECURITY_EVENT_CAP {
                    m.security_events.pop_front();
                }
                m.security_events.push_back(event);
            });
        }
    }

    pub(super) fn recompute_prompt_tokens(&mut self) {
        self.cached_prompt_tokens = self
            .messages
            .iter()
            .map(|m| self.token_counter.count_message_tokens(m) as u64)
            .sum();
    }

    pub(super) fn push_message(&mut self, msg: Message) {
        self.cached_prompt_tokens += self.token_counter.count_message_tokens(&msg) as u64;
        self.messages.push(msg);
    }

    pub(crate) fn record_cost(&self, prompt_tokens: u64, completion_tokens: u64) {
        if let Some(ref tracker) = self.cost_tracker {
            tracker.record_usage(&self.runtime.model_name, prompt_tokens, completion_tokens);
            self.update_metrics(|m| {
                m.cost_spent_cents = tracker.current_spend();
            });
        }
    }

    pub(crate) fn record_cache_usage(&self) {
        if let Some((creation, read)) = self.provider.last_cache_usage() {
            self.update_metrics(|m| {
                m.cache_creation_tokens += creation;
                m.cache_read_tokens += read;
            });
        }
    }

    /// Inject pre-formatted code context into the message list.
    /// The caller is responsible for retrieving and formatting the text.
    pub fn inject_code_context(&mut self, text: &str) {
        self.remove_code_context_messages();
        if text.is_empty() || self.messages.len() <= 1 {
            return;
        }
        let content = format!("{CODE_CONTEXT_PREFIX}{text}");
        self.messages.insert(
            1,
            Message::from_parts(
                Role::System,
                vec![MessagePart::CodeContext { text: content }],
            ),
        );
    }

    #[must_use]
    pub fn context_messages(&self) -> &[Message] {
        &self.messages
    }
}

#[cfg(test)]
mod tests {
    use super::super::agent_tests::{
        MockChannel, MockToolExecutor, create_test_registry, mock_provider,
    };
    use super::*;
    use zeph_llm::provider::{MessageMetadata, MessagePart};

    #[test]
    fn push_message_increments_cached_tokens() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        let before = agent.cached_prompt_tokens;
        let msg = Message {
            role: Role::User,
            content: "hello world!!".to_string(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        };
        let expected_delta = agent.token_counter.count_message_tokens(&msg) as u64;
        agent.push_message(msg);
        assert_eq!(agent.cached_prompt_tokens, before + expected_delta);
    }

    #[test]
    fn recompute_prompt_tokens_matches_sum() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        agent.messages.push(Message {
            role: Role::User,
            content: "1234".to_string(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });
        agent.messages.push(Message {
            role: Role::Assistant,
            content: "5678".to_string(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });

        agent.recompute_prompt_tokens();

        let expected: u64 = agent
            .messages
            .iter()
            .map(|m| agent.token_counter.count_message_tokens(m) as u64)
            .sum();
        assert_eq!(agent.cached_prompt_tokens, expected);
    }

    #[test]
    fn inject_code_context_into_messages_with_existing_content() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        // Add a user message so we have more than 1 message
        agent.push_message(Message {
            role: Role::User,
            content: "question".to_string(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });

        agent.inject_code_context("some code here");

        let found = agent.messages.iter().any(|m| {
            m.parts.iter().any(|p| {
                matches!(p, MessagePart::CodeContext { text } if text.contains("some code here"))
            })
        });
        assert!(found, "code context should be injected into messages");
    }

    #[test]
    fn inject_code_context_empty_text_is_noop() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        agent.push_message(Message {
            role: Role::User,
            content: "question".to_string(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });
        let count_before = agent.messages.len();

        agent.inject_code_context("");

        // No code context message inserted for empty text
        assert_eq!(agent.messages.len(), count_before);
    }

    #[test]
    fn inject_code_context_with_single_message_is_noop() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        // Only system prompt → len == 1 → inject should be noop
        let count_before = agent.messages.len();

        agent.inject_code_context("some code");

        assert_eq!(agent.messages.len(), count_before);
    }

    #[test]
    fn context_messages_returns_all_messages() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        agent.push_message(Message {
            role: Role::User,
            content: "test".to_string(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });

        assert_eq!(agent.context_messages().len(), agent.messages.len());
    }
}
