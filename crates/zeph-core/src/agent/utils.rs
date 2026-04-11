// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use zeph_llm::provider::{LlmProvider, Message, MessagePart, Role};

use super::{Agent, CODE_CONTEXT_PREFIX};
use crate::channel::Channel;
use crate::metrics::{MetricsSnapshot, SECURITY_EVENT_CAP, SecurityEvent, SecurityEventCategory};
use zeph_tools::FilterStats;

impl<C: Channel> Agent<C> {
    /// Read the community-detection failure counter from `SemanticMemory` and update metrics.
    pub fn sync_community_detection_failures(&self) {
        if let Some(memory) = self.memory_state.persistence.memory.as_ref() {
            let failures = memory.community_detection_failures();
            self.update_metrics(|m| {
                m.graph_community_detection_failures = failures;
            });
        }
    }

    /// Sync all graph counters (extraction count/failures) from `SemanticMemory` to metrics.
    pub fn sync_graph_extraction_metrics(&self) {
        if let Some(memory) = self.memory_state.persistence.memory.as_ref() {
            let count = memory.graph_extraction_count();
            let failures = memory.graph_extraction_failures();
            self.update_metrics(|m| {
                m.graph_extraction_count = count;
                m.graph_extraction_failures = failures;
            });
        }
    }

    /// Fetch entity/edge/community counts from the graph store and write to metrics.
    pub async fn sync_graph_counts(&self) {
        let Some(memory) = self.memory_state.persistence.memory.as_ref() else {
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
        let connected = match self.memory_state.persistence.memory.as_ref() {
            Some(m) => m.is_vector_store_connected().await,
            None => false,
        };
        let name = backend_name.to_owned();
        self.update_metrics(|m| {
            m.qdrant_available = connected;
            m.vector_backend = name;
        });
    }

    /// Fetch compression-guidelines metadata from `SQLite` and write to metrics.
    ///
    /// Only fetches version and `created_at`; does not load the full guidelines text.
    /// Feature-gated: compiled only when `compression-guidelines` is enabled.
    pub async fn sync_guidelines_status(&self) {
        let Some(memory) = self.memory_state.persistence.memory.as_ref() else {
            return;
        };
        let cid = self.memory_state.persistence.conversation_id;
        match memory.sqlite().load_compression_guidelines_meta(cid).await {
            Ok((version, created_at)) => {
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let version_u32 = u32::try_from(version).unwrap_or(0);
                self.update_metrics(|m| {
                    m.guidelines_version = version_u32;
                    m.guidelines_updated_at = created_at;
                });
            }
            Err(e) => {
                tracing::warn!("failed to sync guidelines status: {e:#}");
            }
        }
    }

    pub(super) fn record_filter_metrics(&mut self, fs: &FilterStats) {
        let saved = fs.estimated_tokens_saved() as u64;
        let raw = (fs.raw_chars / 4) as u64;
        let confidence = fs.confidence;
        let was_filtered = fs.filtered_chars < fs.raw_chars;
        self.update_metrics(|m| {
            m.filter_raw_tokens += raw;
            m.filter_saved_tokens += saved;
            m.filter_applications += 1;
            m.filter_total_commands += 1;
            if was_filtered {
                m.filter_filtered_commands += 1;
            }
            if let Some(c) = confidence {
                match c {
                    zeph_tools::FilterConfidence::Full => {
                        m.filter_confidence_full += 1;
                    }
                    zeph_tools::FilterConfidence::Partial => {
                        m.filter_confidence_partial += 1;
                    }
                    zeph_tools::FilterConfidence::Fallback => {
                        m.filter_confidence_fallback += 1;
                    }
                }
            }
        });
    }

    pub(super) fn update_metrics(&self, f: impl FnOnce(&mut MetricsSnapshot)) {
        if let Some(ref tx) = self.metrics.metrics_tx {
            let elapsed = self.lifecycle.start_time.elapsed().as_secs();
            tx.send_modify(|m| {
                m.uptime_seconds = elapsed;
                f(m);
            });
        }
    }

    /// Flush `metrics.pending_timings` into the rolling window and publish to the metrics snapshot.
    ///
    /// Call once per turn after all four phases have written to `pending_timings`.
    /// Resets `pending_timings` to default after flushing.
    pub(super) fn flush_turn_timings(&mut self) {
        let timings = std::mem::take(&mut self.metrics.pending_timings);
        tracing::debug!(
            prepare_context_ms = timings.prepare_context_ms,
            llm_chat_ms = timings.llm_chat_ms,
            tool_exec_ms = timings.tool_exec_ms,
            persist_message_ms = timings.persist_message_ms,
            "turn timings"
        );

        if self.metrics.timing_window.len() >= 10 {
            self.metrics.timing_window.pop_front();
        }
        self.metrics.timing_window.push_back(timings.clone());

        let count = self.metrics.timing_window.len();
        let mut avg = crate::metrics::TurnTimings::default();
        let mut max = crate::metrics::TurnTimings::default();
        for t in &self.metrics.timing_window {
            avg.prepare_context_ms = avg.prepare_context_ms.saturating_add(t.prepare_context_ms);
            avg.llm_chat_ms = avg.llm_chat_ms.saturating_add(t.llm_chat_ms);
            avg.tool_exec_ms = avg.tool_exec_ms.saturating_add(t.tool_exec_ms);
            avg.persist_message_ms = avg.persist_message_ms.saturating_add(t.persist_message_ms);

            max.prepare_context_ms = max.prepare_context_ms.max(t.prepare_context_ms);
            max.llm_chat_ms = max.llm_chat_ms.max(t.llm_chat_ms);
            max.tool_exec_ms = max.tool_exec_ms.max(t.tool_exec_ms);
            max.persist_message_ms = max.persist_message_ms.max(t.persist_message_ms);
        }
        let n = count as u64;
        avg.prepare_context_ms /= n;
        avg.llm_chat_ms /= n;
        avg.tool_exec_ms /= n;
        avg.persist_message_ms /= n;

        let total_ms = timings
            .prepare_context_ms
            .saturating_add(timings.llm_chat_ms)
            .saturating_add(timings.tool_exec_ms)
            .saturating_add(timings.persist_message_ms);

        self.update_metrics(|m| {
            m.last_turn_timings = timings;
            m.avg_turn_timings = avg;
            m.max_turn_timings = max;
            m.timing_sample_count = n;
        });

        if let Some(ref recorder) = self.metrics.histogram_recorder {
            recorder.observe_turn_duration(std::time::Duration::from_millis(total_ms));
        }
    }

    /// Push the current classifier metrics snapshot into `MetricsSnapshot`.
    ///
    /// Call this after any classifier invocation (injection, PII, feedback) so the TUI panel
    /// reflects the latest p50/p95 values. No-op when classifier metrics are not configured.
    pub(super) fn push_classifier_metrics(&self) {
        if let Some(ref m) = self.metrics.classifier_metrics {
            let snapshot = m.snapshot();
            self.update_metrics(|ms| ms.classifier = snapshot);
        }
    }

    pub(super) fn push_security_event(
        &self,
        category: SecurityEventCategory,
        source: &str,
        detail: impl Into<String>,
    ) {
        if let Some(ref tx) = self.metrics.metrics_tx {
            let event = SecurityEvent::new(category, source, detail);
            let elapsed = self.lifecycle.start_time.elapsed().as_secs();
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
        self.providers.cached_prompt_tokens = self
            .msg
            .messages
            .iter()
            .map(|m| self.metrics.token_counter.count_message_tokens(m) as u64)
            .sum();
    }

    pub(super) fn push_message(&mut self, msg: Message) {
        self.providers.cached_prompt_tokens +=
            self.metrics.token_counter.count_message_tokens(&msg) as u64;
        if msg.role == zeph_llm::provider::Role::Assistant {
            self.session.last_assistant_at = Some(std::time::Instant::now());
        }
        self.msg.messages.push(msg);
        // Detect MagicDoc headers in tool output after pushing the message.
        self.detect_magic_docs_in_messages();
    }

    pub(crate) fn record_cost_and_cache(&self, input_tokens: u64, output_tokens: u64) {
        let (cache_write, cache_read) = self.provider.last_cache_usage().unwrap_or((0, 0));

        if let Some(ref tracker) = self.metrics.cost_tracker {
            let provider_name = if self.runtime.active_provider_name.is_empty() {
                self.provider.name()
            } else {
                self.runtime.active_provider_name.as_str()
            };
            tracker.record_usage(
                provider_name,
                &self.runtime.model_name,
                input_tokens,
                cache_read,
                cache_write,
                output_tokens,
            );
            let breakdown = tracker.provider_breakdown();
            self.update_metrics(|m| {
                m.cost_spent_cents = tracker.current_spend();
                m.cache_creation_tokens += cache_write;
                m.cache_read_tokens += cache_read;
                m.provider_cost_breakdown = breakdown;
            });
        } else if cache_write > 0 || cache_read > 0 {
            self.update_metrics(|m| {
                m.cache_creation_tokens += cache_write;
                m.cache_read_tokens += cache_read;
            });
        }
    }

    /// Inject pre-formatted code context into the message list.
    /// The caller is responsible for retrieving and formatting the text.
    pub fn inject_code_context(&mut self, text: &str) {
        self.remove_code_context_messages();
        if text.is_empty() || self.msg.messages.len() <= 1 {
            return;
        }
        let content = format!("{CODE_CONTEXT_PREFIX}{text}");
        self.msg.messages.insert(
            1,
            Message::from_parts(
                Role::System,
                vec![MessagePart::CodeContext { text: content }],
            ),
        );
    }

    #[must_use]
    pub fn context_messages(&self) -> &[Message] {
        &self.msg.messages
    }

    /// Truncate stale tool result content in old messages to bound in-memory growth.
    ///
    /// After the LLM has seen and responded to tool output, the full content is no longer
    /// needed in the hot message list (it is already persisted to `SQLite`). Truncating keeps
    /// the in-process message vec small across long sessions.
    ///
    /// Skips the last 2 messages so the LLM retains full context for the next turn.
    ///
    /// Truncated variants: `MessagePart::ToolResult` (content) and `MessagePart::ToolOutput` (body).
    pub(super) fn truncate_old_tool_results(&mut self) {
        const LIMIT: usize = 2048;
        const SUFFIX: &str = "…[truncated]";

        let len = self.msg.messages.len();
        if len <= 2 {
            return;
        }
        for msg in &mut self.msg.messages[..len - 2] {
            for part in &mut msg.parts {
                match part {
                    MessagePart::ToolResult { content, .. } if content.len() > LIMIT => {
                        content.truncate(content.floor_char_boundary(LIMIT));
                        content.push_str(SUFFIX);
                    }
                    MessagePart::ToolOutput { body, .. } if body.len() > LIMIT => {
                        body.truncate(body.floor_char_boundary(LIMIT));
                        body.push_str(SUFFIX);
                    }
                    _ => {}
                }
            }
        }
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

        let before = agent.providers.cached_prompt_tokens;
        let msg = Message {
            role: Role::User,
            content: "hello world!!".to_string(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        };
        let expected_delta = agent.metrics.token_counter.count_message_tokens(&msg) as u64;
        agent.push_message(msg);
        assert_eq!(
            agent.providers.cached_prompt_tokens,
            before + expected_delta
        );
    }

    #[test]
    fn recompute_prompt_tokens_matches_sum() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        agent.msg.messages.push(Message {
            role: Role::User,
            content: "1234".to_string(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });
        agent.msg.messages.push(Message {
            role: Role::Assistant,
            content: "5678".to_string(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });

        agent.recompute_prompt_tokens();

        let expected: u64 = agent
            .msg
            .messages
            .iter()
            .map(|m| agent.metrics.token_counter.count_message_tokens(m) as u64)
            .sum();
        assert_eq!(agent.providers.cached_prompt_tokens, expected);
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

        let found = agent.msg.messages.iter().any(|m| {
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
        let count_before = agent.msg.messages.len();

        agent.inject_code_context("");

        // No code context message inserted for empty text
        assert_eq!(agent.msg.messages.len(), count_before);
    }

    #[test]
    fn inject_code_context_with_single_message_is_noop() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
        // Only system prompt → len == 1 → inject should be noop
        let count_before = agent.msg.messages.len();

        agent.inject_code_context("some code");

        assert_eq!(agent.msg.messages.len(), count_before);
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

        assert_eq!(agent.context_messages().len(), agent.msg.messages.len());
    }

    #[test]
    fn truncate_old_tool_results_truncates_stale_content() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        let big_content = "x".repeat(4096);

        // Message 0 (old) — should be truncated.
        agent.msg.messages.push(Message {
            role: Role::User,
            content: String::new(),
            parts: vec![MessagePart::ToolResult {
                tool_use_id: "id1".to_string(),
                content: big_content.clone(),
                is_error: false,
            }],
            metadata: MessageMetadata::default(),
        });
        // Message 1 (old) — ToolOutput should also be truncated.
        agent.msg.messages.push(Message {
            role: Role::User,
            content: String::new(),
            parts: vec![MessagePart::ToolOutput {
                tool_name: "shell".to_string(),
                body: big_content.clone(),
                compacted_at: None,
            }],
            metadata: MessageMetadata::default(),
        });
        // Message 2 (recent) — must NOT be truncated.
        agent.msg.messages.push(Message {
            role: Role::Assistant,
            content: "reply".to_string(),
            parts: vec![MessagePart::ToolResult {
                tool_use_id: "id3".to_string(),
                content: big_content.clone(),
                is_error: false,
            }],
            metadata: MessageMetadata::default(),
        });
        // Message 3 (most recent) — must NOT be truncated.
        agent.msg.messages.push(Message {
            role: Role::User,
            content: "last".to_string(),
            parts: vec![MessagePart::ToolResult {
                tool_use_id: "id4".to_string(),
                content: big_content.clone(),
                is_error: false,
            }],
            metadata: MessageMetadata::default(),
        });

        // Agent::new inserts a system prompt at index 0, so our messages are at 1..=4.
        let base = agent.msg.messages.len() - 4;

        agent.truncate_old_tool_results();

        // Old ToolResult truncated.
        if let MessagePart::ToolResult { content, .. } = &agent.msg.messages[base].parts[0] {
            assert!(
                content.ends_with("…[truncated]"),
                "msg[base] should be truncated"
            );
            assert!(content.len() <= 2048 + 16);
        } else {
            panic!("expected ToolResult at msg[base]");
        }

        // Old ToolOutput truncated.
        if let MessagePart::ToolOutput { body, .. } = &agent.msg.messages[base + 1].parts[0] {
            assert!(
                body.ends_with("…[truncated]"),
                "msg[base+1] should be truncated"
            );
        } else {
            panic!("expected ToolOutput at msg[base+1]");
        }

        // Recent messages untouched.
        if let MessagePart::ToolResult { content, .. } = &agent.msg.messages[base + 2].parts[0] {
            assert_eq!(content.len(), 4096, "msg[base+2] should NOT be truncated");
        } else {
            panic!("expected ToolResult at msg[base+2]");
        }
        if let MessagePart::ToolResult { content, .. } = &agent.msg.messages[base + 3].parts[0] {
            assert_eq!(content.len(), 4096, "msg[base+3] should NOT be truncated");
        } else {
            panic!("expected ToolResult at msg[base+3]");
        }
    }

    #[test]
    fn truncate_old_tool_results_noop_when_few_messages() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        let big = "y".repeat(4096);
        agent.msg.messages.push(Message {
            role: Role::User,
            content: String::new(),
            parts: vec![MessagePart::ToolResult {
                tool_use_id: "id".to_string(),
                content: big.clone(),
                is_error: false,
            }],
            metadata: MessageMetadata::default(),
        });
        agent.msg.messages.push(Message {
            role: Role::Assistant,
            content: "ok".to_string(),
            parts: vec![MessagePart::ToolResult {
                tool_use_id: "id2".to_string(),
                content: big.clone(),
                is_error: false,
            }],
            metadata: MessageMetadata::default(),
        });

        // Agent::new inserts a system prompt at index 0; our messages are at 1 and 2.
        let len_before = agent.msg.messages.len();
        agent.truncate_old_tool_results();

        // Neither message truncated — both fall in the last-2 window (len=3, skip last 2).
        assert_eq!(agent.msg.messages.len(), len_before);
        if let MessagePart::ToolResult { content, .. } =
            &agent.msg.messages[len_before - 2].parts[0]
        {
            assert_eq!(
                content.len(),
                4096,
                "second-to-last should not be truncated"
            );
        } else {
            panic!("expected ToolResult");
        }
        if let MessagePart::ToolResult { content, .. } =
            &agent.msg.messages[len_before - 1].parts[0]
        {
            assert_eq!(content.len(), 4096, "last should not be truncated");
        } else {
            panic!("expected ToolResult");
        }
    }

    fn make_timings(ctx: u64, llm: u64, tool: u64, persist: u64) -> crate::metrics::TurnTimings {
        crate::metrics::TurnTimings {
            prepare_context_ms: ctx,
            llm_chat_ms: llm,
            tool_exec_ms: tool,
            persist_message_ms: persist,
        }
    }

    fn agent_with_metrics_watch() -> (
        Agent<MockChannel>,
        tokio::sync::watch::Receiver<crate::metrics::MetricsSnapshot>,
    ) {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        let (tx, rx) = tokio::sync::watch::channel(crate::metrics::MetricsSnapshot::default());
        agent.metrics.metrics_tx = Some(tx);
        (agent, rx)
    }

    // T1-a: single flush — last_turn_timings equals the flushed value, count == 1.
    #[test]
    fn flush_turn_timings_single_flush() {
        let (mut agent, rx) = agent_with_metrics_watch();

        agent.metrics.pending_timings = make_timings(10, 200, 50, 5);
        agent.flush_turn_timings();

        let snap = rx.borrow();
        assert_eq!(snap.last_turn_timings.prepare_context_ms, 10);
        assert_eq!(snap.last_turn_timings.llm_chat_ms, 200);
        assert_eq!(snap.last_turn_timings.tool_exec_ms, 50);
        assert_eq!(snap.last_turn_timings.persist_message_ms, 5);
        assert_eq!(snap.timing_sample_count, 1);
        // avg == last when sample_count == 1
        assert_eq!(snap.avg_turn_timings.llm_chat_ms, 200);
    }

    // T1-b: pending_timings reset to default after flush.
    #[test]
    fn flush_turn_timings_resets_pending() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        agent.metrics.pending_timings = make_timings(10, 200, 50, 5);
        agent.flush_turn_timings();

        let p = &agent.metrics.pending_timings;
        assert_eq!(p.prepare_context_ms, 0);
        assert_eq!(p.llm_chat_ms, 0);
        assert_eq!(p.tool_exec_ms, 0);
        assert_eq!(p.persist_message_ms, 0);
    }

    // T1-c: window capped at 10; avg and max computed correctly.
    #[test]
    fn flush_turn_timings_window_capped_at_10() {
        let (mut agent, rx) = agent_with_metrics_watch();

        // Push 12 turns: llm_chat_ms = i * 10 for i in 1..=12.
        for i in 1_u64..=12 {
            agent.metrics.pending_timings = make_timings(0, i * 10, 0, 0);
            agent.flush_turn_timings();
        }

        let snap = rx.borrow();
        // Window holds last 10: turns 3..=12, llm values 30..=120.
        assert_eq!(snap.timing_sample_count, 10);
        // max = 120
        assert_eq!(snap.max_turn_timings.llm_chat_ms, 120);
        // avg of 30,40,...,120 = (30+120)*10/2/10 = 75
        assert_eq!(snap.avg_turn_timings.llm_chat_ms, 75);
    }
}
