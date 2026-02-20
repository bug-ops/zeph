use zeph_llm::provider::{LlmProvider, Message, MessagePart, Role};

use super::{Agent, CODE_CONTEXT_PREFIX};
use crate::channel::Channel;
use crate::metrics::MetricsSnapshot;

impl<C: Channel> Agent<C> {
    pub(super) fn update_metrics(&self, f: impl FnOnce(&mut MetricsSnapshot)) {
        if let Some(ref tx) = self.metrics_tx {
            let elapsed = self.start_time.elapsed().as_secs();
            tx.send_modify(|m| {
                m.uptime_seconds = elapsed;
                f(m);
            });
        }
    }

    pub(super) fn estimate_tokens(content: &str) -> u64 {
        u64::try_from(content.len()).unwrap_or(0) / 4
    }

    pub(super) fn recompute_prompt_tokens(&mut self) {
        self.cached_prompt_tokens = self
            .messages
            .iter()
            .map(|m| Self::estimate_tokens(&m.content))
            .sum();
    }

    pub(super) fn push_message(&mut self, msg: Message) {
        self.cached_prompt_tokens += Self::estimate_tokens(&msg.content);
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
    use zeph_llm::provider::MessagePart;

    #[test]
    fn estimate_tokens_empty_string() {
        assert_eq!(Agent::<MockChannel>::estimate_tokens(""), 0);
    }

    #[test]
    fn estimate_tokens_short_string() {
        // 8 bytes / 4 = 2 tokens
        assert_eq!(Agent::<MockChannel>::estimate_tokens("12345678"), 2);
    }

    #[test]
    fn estimate_tokens_long_string() {
        let s = "a".repeat(400);
        assert_eq!(Agent::<MockChannel>::estimate_tokens(&s), 100);
    }

    #[test]
    fn push_message_increments_cached_tokens() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        let before = agent.cached_prompt_tokens;
        agent.push_message(Message {
            role: Role::User,
            content: "hello world!!".to_string(),
            parts: vec![],
        });
        // "hello world!!" is 13 bytes → 13/4 = 3 tokens
        assert_eq!(agent.cached_prompt_tokens, before + 3);
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
        });
        agent.messages.push(Message {
            role: Role::Assistant,
            content: "5678".to_string(),
            parts: vec![],
        });

        agent.recompute_prompt_tokens();

        let expected: u64 = agent
            .messages
            .iter()
            .map(|m| Agent::<MockChannel>::estimate_tokens(&m.content))
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
        });

        assert_eq!(agent.context_messages().len(), agent.messages.len());
    }
}
