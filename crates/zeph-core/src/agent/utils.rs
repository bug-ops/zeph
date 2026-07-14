// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use zeph_llm::provider::{LlmProvider, Message, MessagePart, Role};

use super::{Agent, CODE_CONTEXT_PREFIX};
use crate::channel::Channel;
use crate::metrics::{MetricsSnapshot, SECURITY_EVENT_CAP, SecurityEvent};
use zeph_common::SecurityEventCategory;
use zeph_tools::FilterStats;

/// Fetch entity/edge/community counts from `store`, defaulting each to `0` on a per-metric error.
///
/// Shared by [`Agent::sync_graph_counts`] and the background graph-count-sync tasks in
/// `persistence::extract` (post-extraction refresh and the periodic count-sync task) so the
/// three call sites cannot drift apart.
pub(super) async fn fetch_graph_counts(store: &zeph_memory::graph::GraphStore) -> (u64, u64, u64) {
    let (entities, edges, communities) = tokio::join!(
        store.entity_count(),
        store.active_edge_count(),
        store.community_count()
    );
    (
        entities.unwrap_or(0).cast_unsigned(),
        edges.unwrap_or(0).cast_unsigned(),
        communities.unwrap_or(0).cast_unsigned(),
    )
}

impl<C: Channel> Agent<C> {
    /// Read the community-detection failure counter from `SemanticMemory` and update metrics.
    pub fn sync_community_detection_failures(&self) {
        if let Some(memory) = self.services.memory.persistence.memory.as_ref() {
            let failures = memory.community_detection_failures();
            self.update_metrics(|m| {
                m.graph_community_detection_failures = failures;
            });
        }
    }

    /// Sync all graph counters (extraction count/failures) from `SemanticMemory` to metrics.
    pub fn sync_graph_extraction_metrics(&self) {
        if let Some(memory) = self.services.memory.persistence.memory.as_ref() {
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
        let Some(memory) = self.services.memory.persistence.memory.as_ref() else {
            return;
        };
        let Some(store) = memory.graph_store.as_ref() else {
            return;
        };
        let (entities, edges, communities) = fetch_graph_counts(store).await;
        self.update_metrics(|m| {
            m.graph_entities_total = entities;
            m.graph_edges_total = edges;
            m.graph_communities_total = communities;
        });
    }

    /// Perform a real health check on the vector store and update metrics.
    pub async fn check_vector_store_health(&self, backend_name: &str) {
        let connected = match self.services.memory.persistence.memory.as_ref() {
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
        let Some(memory) = self.services.memory.persistence.memory.as_ref() else {
            return;
        };
        let cid = self.services.memory.persistence.conversation_id;
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
                    _ => {}
                }
            }
        });
    }

    pub(super) fn update_metrics(&self, f: impl FnOnce(&mut MetricsSnapshot)) {
        if let Some(ref tx) = self.runtime.metrics.metrics_tx {
            let elapsed = self.runtime.lifecycle.start_time.elapsed().as_secs();
            tx.send_modify(|m| {
                m.uptime_seconds = elapsed;
                f(m);
            });
        }
    }

    /// Publish the effective context window limit from the active provider's budget into
    /// [`MetricsSnapshot::context_max_tokens`].
    ///
    /// Call after the provider pool is constructed (builder) and on every successful `/provider`
    /// switch so the TUI context gauge always reflects the active provider's window.
    /// When no budget is configured the field is set to `0`, which the gauge renders as `"—"`.
    pub(crate) fn publish_context_budget(&self) {
        let max_tokens = self
            .context_manager
            .budget
            .as_ref()
            .map_or(0, |b| b.max_tokens() as u64);
        self.update_metrics(|m| m.context_max_tokens = max_tokens);
    }

    /// Flush `metrics.pending_timings` into the rolling window and publish to the metrics snapshot.
    ///
    /// Call once per turn after all four phases have written to `pending_timings`.
    /// Resets `pending_timings` to default after flushing.
    ///
    /// When the `profiling` feature is compiled in, per-field values that `MetricsBridge`
    /// marked as freshly written this turn (`MetricsSnapshot::bridge_timings_written`) take
    /// precedence over the manual value computed here; unmarked fields keep the manual value.
    /// This avoids unconditionally clobbering the bridge's span-derived timings every turn
    /// (#5946) while still working correctly for fields the bridge does not (yet) populate.
    pub(super) fn flush_turn_timings(&mut self) {
        #[cfg_attr(not(feature = "profiling"), allow(unused_mut))]
        let mut timings = std::mem::take(&mut self.runtime.metrics.pending_timings);
        tracing::debug!(
            prepare_context_ms = timings.prepare_context_ms,
            llm_chat_ms = timings.llm_chat_ms,
            tool_exec_ms = timings.tool_exec_ms,
            persist_message_ms = timings.persist_message_ms,
            "turn timings"
        );

        // #5946 (critic finding S2): read the bridge's per-field "written this turn" mask,
        // reconcile it into `timings`, AND clear the mask — all inside this one `send_modify`
        // closure (via `update_metrics`), so the whole read-then-clear is atomic. A previous
        // version read the mask via a separate `borrow()` and cleared it in a later, independent
        // `update_metrics` call; a `MetricsBridge::on_close` write landing in the gap between
        // those two steps would have had its bit and value silently discarded.
        #[cfg(feature = "profiling")]
        self.update_metrics(|m| {
            let mask = m.bridge_timings_written;
            if mask & crate::metrics_bridge::TimingField::PrepareContext.bridge_bit() != 0 {
                timings.prepare_context_ms = m.last_turn_timings.prepare_context_ms;
            }
            if mask & crate::metrics_bridge::TimingField::LlmChat.bridge_bit() != 0 {
                timings.llm_chat_ms = m.last_turn_timings.llm_chat_ms;
            }
            if mask & crate::metrics_bridge::TimingField::ToolExec.bridge_bit() != 0 {
                timings.tool_exec_ms = m.last_turn_timings.tool_exec_ms;
            }
            // persist_message_ms is intentionally never bridged (#6111) — its real span fires
            // 7+ times per turn, not once, so `timings.persist_message_ms` always keeps the
            // manual `Instant::now()` value computed in `agent/mod.rs`.
            m.bridge_timings_written = 0;
            // `MetricsBridge::on_close` accumulates `llm_chat_ms` across every `chat_with_tools`
            // span closed this turn (#6275). Reset it to 0 here, now that it has been read into
            // `timings` above, so the next turn's accumulation starts fresh instead of adding
            // onto this turn's total.
            m.last_turn_timings.llm_chat_ms = 0;
        });

        if self.runtime.metrics.timing_window.len() >= 10 {
            self.runtime.metrics.timing_window.pop_front();
        }
        self.runtime
            .metrics
            .timing_window
            .push_back(timings.clone());

        let count = self.runtime.metrics.timing_window.len();
        let mut avg = crate::metrics::TurnTimings::default();
        let mut max = crate::metrics::TurnTimings::default();
        for t in &self.runtime.metrics.timing_window {
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

        if let Some(ref recorder) = self.runtime.metrics.histogram_recorder {
            recorder.observe_turn_duration(std::time::Duration::from_millis(total_ms));
        }
    }

    /// Push the current classifier metrics snapshot into `MetricsSnapshot`.
    ///
    /// Call this after any classifier invocation (injection, PII, feedback) so the TUI panel
    /// reflects the latest p50/p95 values. No-op when classifier metrics are not configured.
    pub(super) fn push_classifier_metrics(&self) {
        if let Some(ref m) = self.runtime.metrics.classifier_metrics {
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
        if let Some(ref tx) = self.runtime.metrics.metrics_tx {
            let event = SecurityEvent::new(category, source, detail);
            let elapsed = self.runtime.lifecycle.start_time.elapsed().as_secs();
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
        self.runtime.providers.cached_prompt_tokens = self
            .msg
            .messages
            .iter()
            .map(|m| self.runtime.metrics.token_counter.count_message_tokens(m) as u64)
            .sum();
    }

    pub(super) fn push_message(&mut self, msg: Message) {
        self.runtime.providers.cached_prompt_tokens +=
            self.runtime
                .metrics
                .token_counter
                .count_message_tokens(&msg) as u64;
        if msg.role == zeph_llm::provider::Role::Assistant {
            self.services.session.last_assistant_at = Some(std::time::Instant::now());
        }
        self.msg.messages.push(msg);
        // Detect MagicDoc headers in tool output after pushing the message.
        self.detect_magic_docs_in_messages();
    }

    /// Like [`Self::push_message`], but splices `msg` at `index` instead of appending it at the
    /// true end — for repairing an out-of-order shutdown-flush tombstone (see
    /// `shutdown::flush_orphaned_tool_use_on_shutdown`) where a later turn's message may already
    /// have been appended after the orphaned assistant message this tombstone must immediately
    /// follow. Token accounting and `MagicDoc` detection are position-independent, so both are
    /// shared with `push_message`.
    pub(super) fn insert_message(&mut self, index: usize, msg: Message) {
        self.runtime.providers.cached_prompt_tokens +=
            self.runtime
                .metrics
                .token_counter
                .count_message_tokens(&msg) as u64;
        if msg.role == zeph_llm::provider::Role::Assistant {
            self.services.session.last_assistant_at = Some(std::time::Instant::now());
        }
        self.msg.messages.insert(index, msg);
        self.detect_magic_docs_in_messages();
    }

    pub(crate) fn record_cost_and_cache(&self, input_tokens: u64, output_tokens: u64) {
        let (cache_write, cache_read) = self.provider.last_cache_usage().unwrap_or((0, 0));

        if let Some(ref tracker) = self.runtime.metrics.cost_tracker {
            let provider_name = if self.runtime.config.active_provider_name.is_empty() {
                self.provider.name()
            } else {
                self.runtime.config.active_provider_name.as_str()
            };
            tracker.record_usage(
                provider_name,
                self.provider.provider_kind_str(),
                &self.runtime.config.model_name,
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

    pub(crate) fn record_successful_task(&self) {
        if let Some(ref tracker) = self.runtime.metrics.cost_tracker {
            tracker.record_successful_task();
            self.update_metrics(|m| {
                m.cost_cps_cents = tracker.cps();
                m.cost_successful_tasks = tracker.successful_tasks();
            });
        }
    }

    /// Extract a redacted preview of the last assistant message.
    ///
    /// Walks `self.msg.messages` in reverse to find the most recent `Role::Assistant`
    /// message, takes up to `max_chars` Unicode scalar values from `message.content`,
    /// and applies [`crate::redact::scrub_content`] to redact any secrets.
    ///
    /// Returns an empty string when no assistant message exists in the current turn.
    pub(super) fn last_assistant_preview(&self, max_chars: usize) -> String {
        let raw = self
            .msg
            .messages
            .iter()
            .rev()
            .find(|m| m.role == Role::Assistant)
            .map_or("", |m| m.content.as_str());

        if raw.is_empty() {
            return String::new();
        }

        // Truncate to max_chars before redaction to bound redaction work.
        let truncated: &str = if raw.chars().count() > max_chars {
            let end = raw
                .char_indices()
                .nth(max_chars)
                .map_or(raw.len(), |(i, _)| i);
            &raw[..end]
        } else {
            raw
        };

        crate::redact::scrub_content(truncated).into_owned()
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
    use zeph_memory::graph::GraphStore;
    use zeph_memory::graph::types::EntityType;
    use zeph_memory::store::SqliteStore;

    async fn setup_graph_store() -> GraphStore {
        let sqlite = SqliteStore::new(":memory:").await.unwrap();
        GraphStore::new(sqlite.pool().clone())
    }

    #[tokio::test]
    async fn fetch_graph_counts_empty_store_returns_zeros() {
        let store = setup_graph_store().await;
        assert_eq!(fetch_graph_counts(&store).await, (0, 0, 0));
    }

    #[tokio::test]
    async fn fetch_graph_counts_reflects_actual_counts() {
        let store = setup_graph_store().await;
        let a = store
            .upsert_entity("Alice", "Alice", EntityType::Person, None, None)
            .await
            .unwrap()
            .0;
        let b = store
            .upsert_entity("Bob", "Bob", EntityType::Person, None, None)
            .await
            .unwrap()
            .0;
        store
            .insert_edge(a, b, "knows", "Alice knows Bob", 1.0, None, None)
            .await
            .unwrap();
        store
            .upsert_community("cluster", "summary", &[a, b], None)
            .await
            .unwrap();

        assert_eq!(fetch_graph_counts(&store).await, (2, 1, 1));
    }

    /// #5677 follow-up: each of the 3 metrics must fall back to `0` independently on its own
    /// query error, not abort the other two — verified by breaking only `graph_communities`
    /// while `graph_entities`/`graph_edges` stay intact and populated.
    #[tokio::test]
    async fn fetch_graph_counts_falls_back_to_zero_per_field_on_error() {
        let sqlite = SqliteStore::new(":memory:").await.unwrap();
        let pool = sqlite.pool().clone();
        let store = GraphStore::new(pool.clone());
        let a = store
            .upsert_entity("Alice", "Alice", EntityType::Person, None, None)
            .await
            .unwrap()
            .0;
        let b = store
            .upsert_entity("Bob", "Bob", EntityType::Person, None, None)
            .await
            .unwrap()
            .0;
        store
            .insert_edge(a, b, "knows", "Alice knows Bob", 1.0, None, None)
            .await
            .unwrap();

        sqlx::query("DROP TABLE graph_communities")
            .execute(&pool)
            .await
            .unwrap();

        assert_eq!(fetch_graph_counts(&store).await, (2, 1, 0));
    }

    #[test]
    fn push_message_increments_cached_tokens() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        let before = agent.runtime.providers.cached_prompt_tokens;
        let msg = Message {
            role: Role::User,
            content: "hello world!!".to_string(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        };
        let expected_delta = agent
            .runtime
            .metrics
            .token_counter
            .count_message_tokens(&msg) as u64;
        agent.push_message(msg);
        assert_eq!(
            agent.runtime.providers.cached_prompt_tokens,
            before + expected_delta
        );
    }

    /// #5646: `insert_message` must splice at the given index (not append at the end) while
    /// still tracking token accounting identically to `push_message` — direct coverage of the
    /// method itself, complementing its indirect exercise via
    /// `flush_orphaned_tests::flush_orphaned_inserts_tombstone_immediately_after_orphan_not_at_end`
    /// and `focus_tests::persist_cancelled_tool_results_some_index_inserts_at_that_position`.
    #[test]
    fn insert_message_splices_at_index_and_tracks_tokens() {
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor);

        agent.msg.messages.push(Message {
            role: Role::User,
            content: "first".to_string(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });
        agent.msg.messages.push(Message {
            role: Role::User,
            content: "third".to_string(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        });
        let insert_idx = agent.msg.messages.len() - 1;
        let before_tokens = agent.runtime.providers.cached_prompt_tokens;

        let msg = Message {
            role: Role::User,
            content: "second".to_string(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        };
        let expected_delta = agent
            .runtime
            .metrics
            .token_counter
            .count_message_tokens(&msg) as u64;
        agent.insert_message(insert_idx, msg);

        assert_eq!(
            agent.msg.messages[insert_idx].content, "second",
            "message must be spliced at the given index"
        );
        assert_eq!(
            agent.msg.messages[insert_idx + 1].content,
            "third",
            "the message previously at insert_idx must be pushed one slot forward"
        );
        assert_eq!(
            agent.runtime.providers.cached_prompt_tokens,
            before_tokens + expected_delta,
            "insert_message must track token accounting identically to push_message"
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
            .map(|m| agent.runtime.metrics.token_counter.count_message_tokens(m) as u64)
            .sum();
        assert_eq!(agent.runtime.providers.cached_prompt_tokens, expected);
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
                tool_name: "shell".into(),
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
        agent.runtime.metrics.metrics_tx = Some(tx);
        (agent, rx)
    }

    // T1-a: single flush — last_turn_timings equals the flushed value, count == 1.
    #[test]
    fn flush_turn_timings_single_flush() {
        let (mut agent, rx) = agent_with_metrics_watch();

        agent.runtime.metrics.pending_timings = make_timings(10, 200, 50, 5);
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

        agent.runtime.metrics.pending_timings = make_timings(10, 200, 50, 5);
        agent.flush_turn_timings();

        let p = &agent.runtime.metrics.pending_timings;
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
            agent.runtime.metrics.pending_timings = make_timings(0, i * 10, 0, 0);
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

    // #5946: fields MetricsBridge marked as written this turn keep the bridge's span-derived
    // value instead of being clobbered by the manual `Instant::now()` value; unmarked fields
    // still fall back to manual. The bitmask is cleared (taken) after the flush.
    #[cfg(feature = "profiling")]
    #[test]
    fn flush_turn_timings_prefers_bridge_value_for_marked_fields() {
        let (mut agent, rx) = agent_with_metrics_watch();

        if let Some(tx) = agent.runtime.metrics.metrics_tx.as_ref() {
            tx.send_modify(|m| {
                m.last_turn_timings.llm_chat_ms = 999;
                m.bridge_timings_written = crate::metrics_bridge::TimingField::LlmChat.bridge_bit();
            });
        }

        agent.runtime.metrics.pending_timings = make_timings(10, 200, 50, 5);
        agent.flush_turn_timings();

        let snap = rx.borrow();
        assert_eq!(
            snap.last_turn_timings.llm_chat_ms, 999,
            "bridge-marked field must keep the bridge value, not the manual one"
        );
        assert_eq!(snap.last_turn_timings.prepare_context_ms, 10);
        assert_eq!(snap.last_turn_timings.tool_exec_ms, 50);
        assert_eq!(snap.last_turn_timings.persist_message_ms, 5);
        assert_eq!(
            snap.bridge_timings_written, 0,
            "bitmask must be cleared after flush"
        );
    }

    // #6275: `MetricsBridge::on_close` accumulates `last_turn_timings.llm_chat_ms` via
    // `saturating_add` across every `chat_with_tools` span closed in a turn. Without resetting
    // that field back to 0 once `flush_turn_timings` has read it, the next turn's accumulation
    // would start on top of the previous turn's total instead of from zero — a slow, silent
    // leak across turns rather than a one-turn glitch.
    #[cfg(feature = "profiling")]
    #[test]
    fn flush_turn_timings_resets_bridge_llm_chat_ms_across_turns() {
        let (mut agent, rx) = agent_with_metrics_watch();

        // Turn 1: bridge reports a real chat_with_tools-derived duration.
        if let Some(tx) = agent.runtime.metrics.metrics_tx.as_ref() {
            tx.send_modify(|m| {
                m.last_turn_timings.llm_chat_ms = 500;
                m.bridge_timings_written = crate::metrics_bridge::TimingField::LlmChat.bridge_bit();
            });
        }
        agent.runtime.metrics.pending_timings = make_timings(0, 0, 0, 0);
        agent.flush_turn_timings();
        assert_eq!(rx.borrow().last_turn_timings.llm_chat_ms, 500);

        // Turn 2: no chat_with_tools span closes this turn (bridge does not mark the bit).
        // `last_turn_timings.llm_chat_ms` must have been reset to 0 by turn 1's flush, so
        // `MetricsBridge::on_close`'s `saturating_add` (if it fired again) would start fresh —
        // and here, since it never fires, the field must simply read 0, not the stale 500.
        agent.runtime.metrics.pending_timings = make_timings(0, 0, 0, 0);
        agent.flush_turn_timings();

        assert_eq!(
            rx.borrow().last_turn_timings.llm_chat_ms,
            0,
            "llm_chat_ms must not leak turn 1's bridge value into turn 2 (#6275)"
        );
    }
}
