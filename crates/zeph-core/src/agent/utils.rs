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
