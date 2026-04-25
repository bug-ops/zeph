// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::sync::Arc;

use zeph_llm::provider::{Message, MessagePart, Role};
use zeph_memory::AnchoredSummary;

use super::super::Agent;
use crate::channel::Channel;
use zeph_context::summarization::SummarizationDeps;

impl<C: Channel> Agent<C> {
    pub(super) fn build_chunk_prompt(messages: &[Message], guidelines: &str) -> String {
        zeph_context::summarization::build_chunk_prompt(messages, guidelines)
    }

    /// Build the explicit LLM deps struct used by stateless summarization helpers.
    fn build_summarization_deps(&self) -> SummarizationDeps {
        let debug_dumper = self.debug_state.debug_dumper.clone();
        let token_counter = Arc::clone(&self.metrics.token_counter);
        #[allow(clippy::type_complexity)]
        let on_anchored_summary: Option<Arc<dyn Fn(&AnchoredSummary, bool) + Send + Sync>> =
            debug_dumper.map(|d| {
                let tc = Arc::clone(&self.metrics.token_counter);
                #[allow(clippy::type_complexity)]
                let cb: Arc<dyn Fn(&AnchoredSummary, bool) + Send + Sync> =
                    Arc::new(move |summary: &AnchoredSummary, fallback: bool| {
                        d.dump_anchored_summary(summary, fallback, &tc);
                    });
                cb
            });
        SummarizationDeps {
            provider: self.summary_or_primary_provider().clone(),
            llm_timeout: std::time::Duration::from_secs(self.runtime.timeouts.llm_seconds),
            token_counter,
            structured_summaries: self.memory_state.compaction.structured_summaries,
            on_anchored_summary,
        }
    }

    /// Attempt structured summarization via `chat_typed_erased::<AnchoredSummary>()`.
    ///
    /// Returns `Ok(AnchoredSummary)` on success, `Err` when mandatory fields are missing
    /// or the LLM fails. The caller is responsible for falling back to prose on `Err`.
    async fn try_summarize_structured(
        &self,
        messages: &[Message],
        guidelines: &str,
    ) -> Result<AnchoredSummary, zeph_llm::LlmError> {
        let deps = self.build_summarization_deps();
        zeph_context::summarization::summarize_structured(&deps, messages, guidelines).await
    }

    async fn try_summarize_structured_with_deps(
        deps: SummarizationDeps,
        messages: &[Message],
        guidelines: &str,
    ) -> Result<AnchoredSummary, zeph_llm::LlmError> {
        zeph_context::summarization::summarize_structured(&deps, messages, guidelines).await
    }

    /// Build a metadata-only summary without calling the LLM.
    /// Used as last-resort fallback when LLM summarization repeatedly fails.
    pub(super) fn build_metadata_summary(messages: &[Message]) -> String {
        zeph_context::summarization::build_metadata_summary(messages, |s, n| {
            super::truncate_chars(s, n)
        })
    }

    async fn try_summarize_with_llm(
        &self,
        messages: &[Message],
        guidelines: &str,
    ) -> Result<String, zeph_llm::LlmError> {
        let deps = self.build_summarization_deps();
        zeph_context::summarization::summarize_with_llm(&deps, messages, guidelines).await
    }

    async fn try_summarize_with_llm_with_deps(
        deps: SummarizationDeps,
        messages: &[Message],
        guidelines: &str,
    ) -> Result<String, zeph_llm::LlmError> {
        zeph_context::summarization::summarize_with_llm(&deps, messages, guidelines).await
    }

    /// Remove tool response parts from messages using middle-out order.
    /// `fraction` is in range (0.0, 1.0] — fraction of tool responses to remove.
    /// Returns the modified message list.
    pub(super) fn remove_tool_responses_middle_out(
        messages: Vec<Message>,
        fraction: f32,
    ) -> Vec<Message> {
        zeph_context::summarization::remove_tool_responses_middle_out(messages, fraction)
    }

    async fn summarize_messages(
        &self,
        messages: &[Message],
        guidelines: &str,
    ) -> Result<String, super::super::error::AgentError> {
        // Density-aware budget partitioning (#2481).
        //
        // When density budgets are configured (non-default or explicitly set), log the split
        // so operators can observe which fraction of content is high vs. low density.
        // The budgets inform future per-density summarization passes (Phase 2).
        {
            use crate::agent::compaction_strategy::partition_by_density;
            let compression = &self.context_manager.compression;
            let high_budget = compression.high_density_budget;
            let low_budget = compression.low_density_budget;
            let (high, low) = partition_by_density(messages);
            tracing::debug!(
                high_density_count = high.len(),
                low_density_count = low.len(),
                high_budget,
                low_budget,
                "compaction: density-aware partition"
            );
        }

        // Structured path: attempt AnchoredSummary when enabled, fall back to prose on failure.
        if self.memory_state.compaction.structured_summaries {
            match self.try_summarize_structured(messages, guidelines).await {
                Ok(anchored) => {
                    if let Some(ref d) = self.debug_state.debug_dumper {
                        d.dump_anchored_summary(&anchored, false, &self.metrics.token_counter);
                    }
                    return Ok(super::cap_summary(anchored.to_markdown(), 16_000));
                }
                Err(e) => {
                    tracing::warn!(error = %e, "structured summarization failed, falling back to prose");
                    if let Some(ref d) = self.debug_state.debug_dumper {
                        let empty = AnchoredSummary {
                            session_intent: String::new(),
                            files_modified: vec![],
                            decisions_made: vec![],
                            open_questions: vec![],
                            next_steps: vec![],
                        };
                        d.dump_anchored_summary(&empty, true, &self.metrics.token_counter);
                    }
                }
            }
        }

        // Try direct summarization first
        match self.try_summarize_with_llm(messages, guidelines).await {
            Ok(summary) => return Ok(summary),
            Err(e) if !e.is_context_length_error() => return Err(e.into()),
            Err(e) => {
                tracing::warn!(
                    "summarization hit context length error ({e}), trying progressive tool response removal"
                );
            }
        }

        // Progressive tool response removal tiers: 10%, 20%, 50%, 100%
        for fraction in [0.10f32, 0.20, 0.50, 1.0] {
            let reduced = Self::remove_tool_responses_middle_out(messages.to_vec(), fraction);
            tracing::debug!(
                fraction,
                "retrying summarization with reduced tool responses"
            );
            match self.try_summarize_with_llm(&reduced, guidelines).await {
                Ok(summary) => {
                    tracing::info!(
                        fraction,
                        "summarization succeeded after tool response removal"
                    );
                    return Ok(summary);
                }
                Err(e) if e.is_context_length_error() => {
                    tracing::warn!(fraction, "still context length error, trying next tier");
                }
                Err(e) => return Err(e.into()),
            }
        }

        // Final fallback: metadata-only summary without LLM
        tracing::warn!("all LLM summarization attempts failed, using metadata fallback");
        Ok(Self::build_metadata_summary(messages))
    }

    /// Summarize `messages` using `deps` extracted from `&self` before any `.await`.
    ///
    /// Equivalent to `summarize_messages` but takes all inputs by value so the
    /// caller can extract them synchronously, then call this without holding `&self`
    /// or any borrowed slice across any `.await`, making the enclosing future `Send`.
    async fn summarize_messages_with_deps(
        deps: SummarizationDeps,
        structured_summaries: bool,
        messages: Vec<Message>,
        guidelines: String,
    ) -> Result<String, super::super::error::AgentError> {
        if structured_summaries {
            match Self::try_summarize_structured_with_deps(deps.clone(), &messages, &guidelines)
                .await
            {
                Ok(anchored) => {
                    if let Some(ref cb) = deps.on_anchored_summary {
                        cb(&anchored, false);
                    }
                    return Ok(super::cap_summary(anchored.to_markdown(), 16_000));
                }
                Err(e) => {
                    tracing::warn!(error = %e, "structured summarization failed, falling back to prose");
                    if let Some(ref cb) = deps.on_anchored_summary {
                        let empty = AnchoredSummary {
                            session_intent: String::new(),
                            files_modified: vec![],
                            decisions_made: vec![],
                            open_questions: vec![],
                            next_steps: vec![],
                        };
                        cb(&empty, true);
                    }
                }
            }
        }

        match Self::try_summarize_with_llm_with_deps(deps.clone(), &messages, &guidelines).await {
            Ok(summary) => return Ok(summary),
            Err(e) if !e.is_context_length_error() => return Err(e.into()),
            Err(e) => {
                tracing::warn!(
                    "summarization hit context length error ({e}), trying progressive tool response removal"
                );
            }
        }

        for fraction in [0.10f32, 0.20, 0.50, 1.0] {
            let reduced = Self::remove_tool_responses_middle_out(messages.clone(), fraction);
            tracing::debug!(
                fraction,
                "retrying summarization with reduced tool responses"
            );
            match Self::try_summarize_with_llm_with_deps(deps.clone(), &reduced, &guidelines).await
            {
                Ok(summary) => {
                    tracing::info!(
                        fraction,
                        "summarization succeeded after tool response removal"
                    );
                    return Ok(summary);
                }
                Err(e) if e.is_context_length_error() => {
                    tracing::warn!(fraction, "still context length error, trying next tier");
                }
                Err(e) => return Err(e.into()),
            }
        }

        tracing::warn!("all LLM summarization attempts failed, using metadata fallback");
        Ok(Self::build_metadata_summary(&messages))
    }

    /// Load the current compression guidelines from `SQLite` if the feature is enabled.
    ///
    /// Returns an empty string when the feature is disabled, memory is not initialized,
    /// or the database query fails (non-fatal).
    ///
    /// Callers must extract `memory` and `conv_id` from `&self` before the first `.await`
    /// so that `&self` is not held across the await boundary (required for Send futures).
    async fn load_compression_guidelines(
        enabled: bool,
        memory: Option<std::sync::Arc<zeph_memory::semantic::SemanticMemory>>,
        conv_id: Option<zeph_memory::ConversationId>,
    ) -> String {
        if !enabled {
            return String::new();
        }
        let Some(memory) = memory else {
            return String::new();
        };
        // Clone DbStore before .await to avoid holding &SemanticMemory across the await
        // boundary (SemanticMemory contains AnyProvider which is !Sync → &SM is !Send).
        let sqlite = memory.sqlite().clone();
        match sqlite.load_compression_guidelines(conv_id).await {
            Ok((_, text)) => text,
            Err(e) => {
                tracing::warn!("failed to load compression guidelines: {e:#}");
                String::new()
            }
        }
    }

    /// Load the current compression guidelines from `SQLite` if the feature is enabled.
    ///
    /// Returns an empty string when the feature is disabled, memory is not initialized,
    /// or the database query fails (non-fatal).
    async fn load_compression_guidelines_if_enabled(&self) -> String {
        let enabled = self
            .memory_state
            .compaction
            .compression_guidelines_config
            .enabled;
        let memory = self.memory_state.persistence.memory.clone();
        let conv_id = self.memory_state.persistence.conversation_id;
        Self::load_compression_guidelines(enabled, memory, conv_id).await
    }

    /// Archive tool output bodies from `to_compact` messages before compaction (Memex #2432).
    ///
    /// Saves each non-empty, non-already-archived `ToolOutput` body to `tool_overflow`
    /// with `archive_type = 'archive'`. Returns a list of reference strings in the format
    /// `[archived:{uuid} — tool: {tool_name} — {bytes} bytes]` for use as a postfix.
    ///
    /// References are injected AFTER summarization (fix C1: LLM would destroy them).
    /// Returns an empty vec when `archive_tool_outputs` is disabled or memory is unavailable.
    ///
    /// Callers must extract `memory` and `cid` from `&self` before the first `.await`
    /// so that `&self` is not held across the await boundary (required for Send futures).
    async fn archive_tool_outputs(
        archive_enabled: bool,
        memory: Option<std::sync::Arc<zeph_memory::semantic::SemanticMemory>>,
        cid: Option<zeph_memory::ConversationId>,
        to_compact: Vec<Message>,
    ) -> Vec<String> {
        if !archive_enabled {
            return Vec::new();
        }
        let (Some(memory), Some(cid)) = (memory, cid) else {
            return Vec::new();
        };

        let mut refs = Vec::new();
        // Clone DbStore before the loop to avoid holding &SemanticMemory across .await points.
        let sqlite = memory.sqlite().clone();

        for msg in to_compact {
            for part in &msg.parts {
                if let MessagePart::ToolOutput {
                    body, tool_name, ..
                } = part
                {
                    // Skip empty, already-archived, or already-overflowed bodies.
                    if body.is_empty()
                        || body.starts_with("[archived:")
                        || body.starts_with("[full output stored")
                        || body.starts_with("[tool output pruned")
                    {
                        continue;
                    }
                    match sqlite.save_archive(cid.0, body.as_bytes()).await {
                        Ok(uuid) => {
                            let bytes = body.len();
                            refs.push(format!(
                                "[archived:{uuid} — tool: {tool_name} — {bytes} bytes]"
                            ));
                        }
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                "Memex: failed to archive tool output (non-fatal)"
                            );
                        }
                    }
                }
            }
        }

        if !refs.is_empty() {
            tracing::debug!(
                archived = refs.len(),
                "Memex: archived tool outputs before compaction"
            );
        }
        refs
    }

    async fn archive_tool_outputs_for_compaction(&self, to_compact: &[Message]) -> Vec<String> {
        let archive_enabled = self.context_manager.compression.archive_tool_outputs;
        let memory = self.memory_state.persistence.memory.clone();
        let cid = self.memory_state.persistence.conversation_id;
        Self::archive_tool_outputs(archive_enabled, memory, cid, to_compact.to_vec()).await
    }

    /// Adjust `compact_end` backward so the preserved tail begins on a clean message boundary.
    ///
    /// If `messages[compact_end]` is a `Role::User` message that contains only `ToolResult`
    /// parts, its paired `Role::Assistant` `ToolUse` message is at `compact_end - 1` and would
    /// be drained, leaving an orphaned `tool_result`.  This function walks the boundary backward
    /// until the first message of the tail is not a `ToolResult`-only user message, absorbing all
    /// consecutive `ToolResult` messages and the single preceding `ToolUse` assistant message.
    ///
    /// Returns the adjusted `compact_end`.  The minimum returned value is `1` (drain nothing
    /// beyond the system message).
    fn adjust_compact_end_for_tool_pairs(messages: &[Message], compact_end: usize) -> usize {
        let mut end = compact_end;
        loop {
            // Nothing left to drain — stop.
            if end <= 1 {
                return 1;
            }
            // compact_end may equal messages.len() when preserve_tail = 0.
            if end >= messages.len() {
                break;
            }
            let first_tail = &messages[end];
            let is_tool_result_msg = first_tail.role == Role::User
                && !first_tail.parts.is_empty()
                && first_tail
                    .parts
                    .iter()
                    .all(|p| matches!(p, MessagePart::ToolResult { .. }));
            if !is_tool_result_msg {
                break;
            }
            // Absorb this ToolResult message into the tail.
            end -= 1;
        }
        // If we moved the boundary, also absorb the preceding ToolUse assistant message.
        if end < compact_end && end > 1 {
            let preceding = &messages[end - 1];
            let is_tool_use_msg = preceding.role == Role::Assistant
                && preceding
                    .parts
                    .iter()
                    .any(|p| matches!(p, MessagePart::ToolUse { .. }));
            if is_tool_use_msg {
                end -= 1;
            }
        }
        end.max(1)
    }
}

mod compaction;
mod deferred;
mod pruning;
mod scheduling;

#[cfg(test)]
mod tests {
    use super::*;
    use zeph_context::summarization::extract_overflow_ref;

    #[test]
    fn extract_overflow_ref_returns_uuid_when_present() {
        let uuid = "550e8400-e29b-41d4-a716-446655440000";
        let body = format!(
            "some output\n[full output stored \u{2014} ID: {uuid} \u{2014} 12345 bytes, use read_overflow tool to retrieve]"
        );
        assert_eq!(extract_overflow_ref(&body), Some(uuid));
    }

    #[test]
    fn extract_overflow_ref_returns_none_when_absent() {
        let body = "normal small output without overflow notice";
        assert_eq!(extract_overflow_ref(body), None);
    }

    #[test]
    fn extract_overflow_ref_returns_none_for_empty_body() {
        assert_eq!(extract_overflow_ref(""), None);
    }

    #[test]
    fn extract_overflow_ref_handles_notice_at_start() {
        let uuid = "a1b2c3d4-e5f6-7890-abcd-ef1234567890";
        let body = format!(
            "[full output stored \u{2014} ID: {uuid} \u{2014} 9999 bytes, use read_overflow tool to retrieve]"
        );
        assert_eq!(extract_overflow_ref(&body), Some(uuid));
    }

    // T-CRIT-01: prune_tool_outputs must skip focus_pinned messages.
    #[test]
    fn prune_tool_outputs_skips_focus_pinned_messages() {
        use crate::agent::tests::agent_tests::{
            MockChannel, MockToolExecutor, create_test_registry, mock_provider,
        };
        use zeph_llm::provider::{Message, MessageMetadata, MessagePart, Role};

        let mut agent = Agent::new(
            mock_provider(vec![]),
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        );
        // Disable tail protection so the pruner can evict all messages in the test.
        agent.context_manager.prune_protect_tokens = 0;
        // Agent::new prepopulates messages[0] with a system prompt.

        // Pinned knowledge block with a large tool output part
        let mut pinned_meta = MessageMetadata::focus_pinned();
        pinned_meta.focus_pinned = true;
        let big_body = "x".repeat(5000);
        let mut pinned_msg = Message {
            role: Role::System,
            content: big_body.clone(),
            parts: vec![MessagePart::ToolOutput {
                tool_name: "read".into(),
                body: big_body.clone(),
                compacted_at: None,
            }],
            metadata: pinned_meta,
        };
        pinned_msg.rebuild_content();
        agent.msg.messages.push(pinned_msg);

        // Non-pinned message with a large tool output
        let big_body2 = "y".repeat(5000);
        let mut normal_msg = Message {
            role: Role::User,
            content: big_body2.clone(),
            parts: vec![MessagePart::ToolOutput {
                tool_name: "shell".into(),
                body: big_body2.clone(),
                compacted_at: None,
            }],
            metadata: MessageMetadata::default(),
        };
        normal_msg.rebuild_content();
        agent.msg.messages.push(normal_msg);

        let freed = agent.prune_tool_outputs(1);

        // messages[0] = agent system prompt, messages[1] = pinned, messages[2] = normal.
        let pinned = &agent.msg.messages[1];
        if let MessagePart::ToolOutput {
            body, compacted_at, ..
        } = &pinned.parts[0]
        {
            assert_eq!(*body, "x".repeat(5000), "pinned body must not be evicted");
            assert!(
                compacted_at.is_none(),
                "pinned compacted_at must remain None"
            );
        }

        // Non-pinned body must be evicted
        let normal = &agent.msg.messages[2];
        if let MessagePart::ToolOutput { compacted_at, .. } = &normal.parts[0] {
            assert!(compacted_at.is_some(), "non-pinned body must be evicted");
        }

        assert!(freed > 0, "must free tokens from non-pinned message");
    }

    // T-CRIT-03: prune_tool_outputs_oldest_first basic ordering.
    #[test]
    fn prune_tool_outputs_oldest_first_evicts_from_front() {
        use crate::agent::tests::agent_tests::{
            MockChannel, MockToolExecutor, create_test_registry, mock_provider,
        };
        use zeph_llm::provider::{Message, MessageMetadata, MessagePart, Role};

        let mut agent = Agent::new(
            mock_provider(vec![]),
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        );
        // Disable tail protection so the pruner can evict all messages in the test.
        agent.context_manager.prune_protect_tokens = 0;
        // Agent::new puts system prompt at messages[0]; tool outputs go to indices 1..=3.

        for i in 0..3 {
            let body = format!("tool output {i} {}", "z".repeat(500));
            let mut msg = Message {
                role: Role::User,
                content: body.clone(),
                parts: vec![MessagePart::ToolOutput {
                    tool_name: "shell".into(),
                    body: body.clone(),
                    compacted_at: None,
                }],
                metadata: MessageMetadata::default(),
            };
            msg.rebuild_content();
            agent.msg.messages.push(msg);
        }

        // Evict just enough for the first message; the last two should be intact.
        agent.prune_tool_outputs_oldest_first(1);

        // messages[0] = agent system prompt, messages[1..=3] = ToolOutput messages.
        if let MessagePart::ToolOutput { compacted_at, .. } = &agent.msg.messages[1].parts[0] {
            assert!(
                compacted_at.is_some(),
                "oldest tool output must be evicted first"
            );
        }
        // Second should be intact (we only freed enough for 1)
        if let MessagePart::ToolOutput { compacted_at, .. } = &agent.msg.messages[2].parts[0] {
            assert!(
                compacted_at.is_none(),
                "second tool output must still be intact"
            );
        }
    }

    // --- Structured summarization tests ---

    // T-STR-01: build_anchored_summary_prompt embeds conversation and all 5 JSON field names.
    #[test]
    fn build_anchored_summary_prompt_contains_required_fields_and_history() {
        use zeph_llm::provider::{Message, MessageMetadata, Role};

        let messages = vec![
            Message {
                role: Role::User,
                content: "refactor the auth middleware".into(),
                parts: vec![],
                metadata: MessageMetadata::default(),
            },
            Message {
                role: Role::Assistant,
                content: "I will split it into two modules".into(),
                parts: vec![],
                metadata: MessageMetadata::default(),
            },
        ];

        let prompt = zeph_context::summarization::build_anchored_summary_prompt(&messages, "");

        // All 5 JSON field names must appear in the prompt.
        assert!(prompt.contains("session_intent"), "missing session_intent");
        assert!(prompt.contains("files_modified"), "missing files_modified");
        assert!(prompt.contains("decisions_made"), "missing decisions_made");
        assert!(prompt.contains("open_questions"), "missing open_questions");
        assert!(prompt.contains("next_steps"), "missing next_steps");

        // Conversation content must be embedded.
        assert!(
            prompt.contains("refactor the auth middleware"),
            "user message not in prompt"
        );
        assert!(
            prompt.contains("I will split it into two modules"),
            "assistant message not in prompt"
        );
    }

    // T-STR-02: build_anchored_summary_prompt injects guidelines when non-empty.
    #[test]
    fn build_anchored_summary_prompt_includes_guidelines() {
        use zeph_llm::provider::{Message, MessageMetadata, Role};

        let messages = vec![Message {
            role: Role::User,
            content: "hello".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        }];
        let prompt = zeph_context::summarization::build_anchored_summary_prompt(
            &messages,
            "focus on file paths",
        );

        assert!(
            prompt.contains("compression-guidelines"),
            "guidelines section missing"
        );
        assert!(
            prompt.contains("focus on file paths"),
            "guidelines content missing"
        );
    }

    // T-STR-03: try_summarize_structured returns Ok(AnchoredSummary) when mock returns valid JSON.
    #[tokio::test]
    async fn try_summarize_structured_returns_anchored_summary_on_valid_json() {
        use crate::agent::tests::agent_tests::{
            MockChannel, MockToolExecutor, create_test_registry, mock_provider,
        };
        use zeph_llm::provider::{Message, MessageMetadata, Role};
        use zeph_memory::AnchoredSummary;

        let valid_json = serde_json::to_string(&AnchoredSummary {
            session_intent: "Implement auth middleware".into(),
            files_modified: vec!["src/auth.rs".into()],
            decisions_made: vec!["Decision: use JWT — Reason: stateless".into()],
            open_questions: vec![],
            next_steps: vec!["Write tests".into()],
        })
        .unwrap();

        let mut agent = Agent::new(
            mock_provider(vec![valid_json]),
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        );
        agent.memory_state.compaction.structured_summaries = true;

        let messages = vec![Message {
            role: Role::User,
            content: "implement auth".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        }];

        let result = agent.try_summarize_structured(&messages, "").await;
        assert!(result.is_ok(), "expected Ok, got: {result:?}");
        let summary = result.unwrap();
        assert_eq!(summary.session_intent, "Implement auth middleware");
        assert_eq!(summary.files_modified, vec!["src/auth.rs"]);
        assert!(summary.is_complete());
    }

    // T-STR-04: try_summarize_structured returns Err when mandatory fields are missing.
    #[tokio::test]
    async fn try_summarize_structured_returns_err_when_incomplete() {
        use crate::agent::tests::agent_tests::{
            MockChannel, MockToolExecutor, create_test_registry, mock_provider,
        };
        use zeph_llm::provider::{Message, MessageMetadata, Role};
        use zeph_memory::AnchoredSummary;

        // next_steps is empty → is_complete() returns false → method must return Err.
        let incomplete_json = serde_json::to_string(&AnchoredSummary {
            session_intent: "Some intent".into(),
            files_modified: vec![],
            decisions_made: vec![],
            open_questions: vec![],
            next_steps: vec![], // missing → incomplete
        })
        .unwrap();

        let mut agent = Agent::new(
            mock_provider(vec![incomplete_json]),
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        );
        agent.memory_state.compaction.structured_summaries = true;

        let messages = vec![Message {
            role: Role::User,
            content: "do something".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        }];

        let result = agent.try_summarize_structured(&messages, "").await;
        assert!(
            result.is_err(),
            "expected Err for incomplete summary, got Ok"
        );
    }

    // T-STR-05: try_summarize_structured returns Err when LLM returns invalid JSON.
    #[tokio::test]
    async fn try_summarize_structured_returns_err_on_malformed_json() {
        use crate::agent::tests::agent_tests::{
            MockChannel, MockToolExecutor, create_test_registry, mock_provider,
        };
        use zeph_llm::provider::{Message, MessageMetadata, Role};

        // chat_typed retries once then returns StructuredParse error on bad JSON.
        let bad_json = "this is not json at all".to_string();
        let mut agent = Agent::new(
            mock_provider(vec![bad_json.clone(), bad_json]),
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        );
        agent.memory_state.compaction.structured_summaries = true;

        let messages = vec![Message {
            role: Role::User,
            content: "summarize".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        }];

        let result = agent.try_summarize_structured(&messages, "").await;
        assert!(result.is_err(), "expected Err for malformed JSON, got Ok");
    }

    // T-STR-06: summarize_messages uses prose path when structured_summaries = false.
    #[tokio::test]
    async fn summarize_messages_uses_prose_when_flag_disabled() {
        use crate::agent::tests::agent_tests::{
            MockChannel, MockToolExecutor, create_test_registry, mock_provider,
        };
        use zeph_llm::provider::{Message, MessageMetadata, Role};

        let prose_response = "1. User Intent: test\n2. Files: none".to_string();
        let agent = Agent::new(
            mock_provider(vec![prose_response.clone()]),
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        );
        // structured_summaries = false by default in Agent::new()

        let messages = vec![Message {
            role: Role::User,
            content: "do a thing".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        }];

        let result = agent.summarize_messages(&messages, "").await;
        assert!(result.is_ok(), "prose path must succeed");
        // Prose path returns the raw LLM output (no markdown section headers from AnchoredSummary).
        assert!(
            !result.unwrap().contains("[anchored summary]"),
            "prose path must not produce anchored summary header"
        );
    }

    // T-STR-07: summarize_messages returns markdown with anchored headers when flag enabled.
    #[tokio::test]
    async fn summarize_messages_returns_anchored_markdown_when_flag_enabled() {
        use crate::agent::tests::agent_tests::{
            MockChannel, MockToolExecutor, create_test_registry, mock_provider,
        };
        use zeph_llm::provider::{Message, MessageMetadata, Role};
        use zeph_memory::AnchoredSummary;

        let valid_json = serde_json::to_string(&AnchoredSummary {
            session_intent: "Build a CLI tool".into(),
            files_modified: vec!["src/cli.rs".into()],
            decisions_made: vec!["Decision: use clap — Reason: ergonomic API".into()],
            open_questions: vec![],
            next_steps: vec!["Add help text".into()],
        })
        .unwrap();

        let mut agent = Agent::new(
            mock_provider(vec![valid_json]),
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        );
        agent.memory_state.compaction.structured_summaries = true;

        let messages = vec![Message {
            role: Role::User,
            content: "build CLI".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        }];

        let result = agent.summarize_messages(&messages, "").await;
        assert!(result.is_ok(), "structured path must succeed");
        let md = result.unwrap();
        assert!(
            md.contains("[anchored summary]"),
            "output must start with anchored summary header"
        );
        assert!(md.contains("## Session Intent"), "missing Session Intent");
        assert!(md.contains("## Next Steps"), "missing Next Steps");
        assert!(
            md.contains("Build a CLI tool"),
            "session_intent content missing"
        );
    }

    // T-STR-08: dump_anchored_summary creates a file with required JSON fields.
    #[test]
    fn dump_anchored_summary_creates_file_with_required_fields() {
        use crate::debug_dump::{DebugDumper, DumpFormat};
        use zeph_memory::{AnchoredSummary, TokenCounter};

        let dir = tempfile::tempdir().expect("tempdir");
        let dumper = DebugDumper::new(dir.path(), DumpFormat::Raw).expect("dumper creation");
        let summary = AnchoredSummary {
            session_intent: "Test dump".into(),
            files_modified: vec!["a.rs".into(), "b.rs".into()],
            decisions_made: vec!["Decision: async — Reason: performance".into()],
            open_questions: vec![],
            next_steps: vec!["Run tests".into()],
        };
        let counter = TokenCounter::new();
        dumper.dump_anchored_summary(&summary, false, &counter);

        // Find the anchored-summary file.
        let entries: Vec<_> = std::fs::read_dir(dumper.dir())
            .expect("read_dir")
            .filter_map(std::result::Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.ends_with("-anchored-summary.json"))
            })
            .collect();
        assert_eq!(
            entries.len(),
            1,
            "exactly one anchored-summary.json expected"
        );

        let content = std::fs::read_to_string(&entries[0]).expect("read file");
        let v: serde_json::Value = serde_json::from_str(&content).expect("valid JSON");
        assert!(
            v.get("section_completeness").is_some(),
            "missing section_completeness"
        );
        assert!(v.get("total_items").is_some(), "missing total_items");
        assert!(v.get("token_estimate").is_some(), "missing token_estimate");
        assert!(v.get("fallback").is_some(), "missing fallback field");
        assert_eq!(v["fallback"], false, "fallback must be false");

        let sc = &v["section_completeness"];
        assert_eq!(sc["session_intent"], true);
        assert_eq!(sc["files_modified"], true);
        assert_eq!(sc["decisions_made"], true);
        assert_eq!(sc["open_questions"], false);
        assert_eq!(sc["next_steps"], true);
    }

    // T-STR-09: dump_anchored_summary with fallback=true sets fallback field correctly.
    #[test]
    fn dump_anchored_summary_fallback_flag_propagated() {
        use crate::debug_dump::{DebugDumper, DumpFormat};
        use zeph_memory::{AnchoredSummary, TokenCounter};

        let dir = tempfile::tempdir().expect("tempdir");
        let dumper = DebugDumper::new(dir.path(), DumpFormat::Raw).expect("dumper creation");
        let empty = AnchoredSummary {
            session_intent: String::new(),
            files_modified: vec![],
            decisions_made: vec![],
            open_questions: vec![],
            next_steps: vec![],
        };
        let counter = TokenCounter::new();
        dumper.dump_anchored_summary(&empty, true, &counter);

        let entries: Vec<_> = std::fs::read_dir(dumper.dir())
            .expect("read_dir")
            .filter_map(std::result::Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.ends_with("-anchored-summary.json"))
            })
            .collect();
        assert_eq!(
            entries.len(),
            1,
            "exactly one anchored-summary.json expected"
        );

        let content = std::fs::read_to_string(&entries[0]).expect("read file");
        let v: serde_json::Value = serde_json::from_str(&content).expect("valid JSON");
        assert_eq!(v["fallback"], true, "fallback flag must be true");
        assert_eq!(
            v["total_items"], 0,
            "total_items must be 0 for empty summary"
        );
    }

    // T-CRIT-03: prune_tool_outputs_scored basic — lowest-relevance block evicted first.
    #[test]
    fn prune_tool_outputs_scored_evicts_lowest_relevance_first() {
        use crate::agent::tests::agent_tests::{
            MockChannel, MockToolExecutor, create_test_registry, mock_provider,
        };
        use crate::config::PruningStrategy;
        use zeph_llm::provider::{Message, MessageMetadata, MessagePart, Role};

        let mut agent = Agent::new(
            mock_provider(vec![]),
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        );
        agent.context_manager.compression.pruning_strategy = PruningStrategy::TaskAware;
        agent.compression.current_task_goal =
            Some("authentication middleware session token".to_string());
        // Disable tail protection so the pruner can evict all messages in the test.
        agent.context_manager.prune_protect_tokens = 0;
        // Agent::new puts system prompt at messages[0]; rel_msg goes to index 1, irrel_msg to 2.

        // High-relevance: contains goal keywords
        let rel_body = "authentication middleware session token implementation ".repeat(50);
        let mut rel_msg = Message {
            role: Role::User,
            content: rel_body.clone(),
            parts: vec![MessagePart::ToolOutput {
                tool_name: "read".into(),
                body: rel_body.clone(),
                compacted_at: None,
            }],
            metadata: MessageMetadata::default(),
        };
        rel_msg.rebuild_content();
        agent.msg.messages.push(rel_msg);

        // Low-relevance: unrelated content
        let irrel_body = "database migration schema table column index ".repeat(50);
        let mut irrel_msg = Message {
            role: Role::User,
            content: irrel_body.clone(),
            parts: vec![MessagePart::ToolOutput {
                tool_name: "read".into(),
                body: irrel_body.clone(),
                compacted_at: None,
            }],
            metadata: MessageMetadata::default(),
        };
        irrel_msg.rebuild_content();
        agent.msg.messages.push(irrel_msg);

        agent.prune_tool_outputs_scored(1);

        // messages[0] = agent system prompt, messages[1] = rel_msg, messages[2] = irrel_msg.
        if let MessagePart::ToolOutput { compacted_at, .. } = &agent.msg.messages[2].parts[0] {
            assert!(
                compacted_at.is_some(),
                "low-relevance block must be evicted"
            );
        }
        if let MessagePart::ToolOutput { compacted_at, .. } = &agent.msg.messages[1].parts[0] {
            assert!(compacted_at.is_none(), "high-relevance block must survive");
        }
    }

    // T-CRIT-04: prune_tool_outputs_mig evicts blocks with lowest MIG score first.
    #[test]
    fn prune_tool_outputs_mig_evicts_lowest_mig_first() {
        use crate::agent::tests::agent_tests::{
            MockChannel, MockToolExecutor, create_test_registry, mock_provider,
        };
        use crate::config::PruningStrategy;
        use zeph_llm::provider::{Message, MessageMetadata, MessagePart, Role};

        let mut agent = Agent::new(
            mock_provider(vec![]),
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        );
        agent.context_manager.compression.pruning_strategy = PruningStrategy::Mig;
        // Set a goal so MIG scorer has context for relevance scoring.
        agent.compression.current_task_goal = Some("authentication token".to_string());
        // Disable tail protection so the pruner can evict all messages in the test.
        agent.context_manager.prune_protect_tokens = 0;

        // High-relevance: repeated goal keywords → high relevance, low redundancy relative to goal
        let rel_body = "authentication token session middleware ".repeat(50);
        let mut rel_msg = Message {
            role: Role::User,
            content: rel_body.clone(),
            parts: vec![MessagePart::ToolOutput {
                tool_name: "read".into(),
                body: rel_body.clone(),
                compacted_at: None,
            }],
            metadata: MessageMetadata::default(),
        };
        rel_msg.rebuild_content();
        agent.msg.messages.push(rel_msg);

        // Low-relevance: unrelated content → low relevance → low MIG → evicted first
        let irrel_body = "database schema table column index ".repeat(50);
        let mut irrel_msg = Message {
            role: Role::User,
            content: irrel_body.clone(),
            parts: vec![MessagePart::ToolOutput {
                tool_name: "read".into(),
                body: irrel_body.clone(),
                compacted_at: None,
            }],
            metadata: MessageMetadata::default(),
        };
        irrel_msg.rebuild_content();
        agent.msg.messages.push(irrel_msg);

        // Ask to free only 1 token — should evict the lowest-MIG block.
        agent.prune_tool_outputs_mig(1);

        // messages[0] = system prompt, messages[1] = rel_msg, messages[2] = irrel_msg.
        if let MessagePart::ToolOutput { compacted_at, .. } = &agent.msg.messages[2].parts[0] {
            assert!(
                compacted_at.is_some(),
                "low-MIG (irrelevant) block must be evicted"
            );
        } else {
            panic!("expected ToolOutput at messages[2]");
        }
        if let MessagePart::ToolOutput { compacted_at, .. } = &agent.msg.messages[1].parts[0] {
            assert!(
                compacted_at.is_none(),
                "high-MIG (relevant) block must survive"
            );
        } else {
            panic!("expected ToolOutput at messages[1]");
        }
    }

    // T-CRIT-05: scored pruning respects prune_protect_tokens.
    #[test]
    fn prune_tool_outputs_scored_respects_protect_tokens() {
        use crate::agent::tests::agent_tests::{
            MockChannel, MockToolExecutor, create_test_registry, mock_provider,
        };
        use crate::config::PruningStrategy;
        use zeph_llm::provider::{Message, MessageMetadata, MessagePart, Role};

        let mut agent = Agent::new(
            mock_provider(vec![]),
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        );
        agent.context_manager.compression.pruning_strategy = PruningStrategy::TaskAware;
        agent.compression.current_task_goal = Some("irrelevant goal".to_string());
        // Protect the entire tail (999_999 tokens) — nothing should be evicted.
        agent.context_manager.prune_protect_tokens = 999_999;

        let body = "unrelated content database schema ".repeat(50);
        let mut msg = Message {
            role: Role::User,
            content: body.clone(),
            parts: vec![MessagePart::ToolOutput {
                tool_name: "read".into(),
                body: body.clone(),
                compacted_at: None,
            }],
            metadata: MessageMetadata::default(),
        };
        msg.rebuild_content();
        agent.msg.messages.push(msg);

        let freed = agent.prune_tool_outputs_scored(1);
        assert_eq!(
            freed, 0,
            "no tokens should be freed when everything is protected"
        );

        if let MessagePart::ToolOutput { compacted_at, .. } = &agent.msg.messages[1].parts[0] {
            assert!(
                compacted_at.is_none(),
                "protected block must not be evicted"
            );
        } else {
            panic!("expected ToolOutput at messages[1]");
        }
    }

    // T-CRIT-06: MIG pruning respects prune_protect_tokens.
    #[test]
    fn prune_tool_outputs_mig_respects_protect_tokens() {
        use crate::agent::tests::agent_tests::{
            MockChannel, MockToolExecutor, create_test_registry, mock_provider,
        };
        use crate::config::PruningStrategy;
        use zeph_llm::provider::{Message, MessageMetadata, MessagePart, Role};

        let mut agent = Agent::new(
            mock_provider(vec![]),
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        );
        agent.context_manager.compression.pruning_strategy = PruningStrategy::Mig;
        agent.compression.current_task_goal = Some("irrelevant goal".to_string());
        // Protect the entire tail (999_999 tokens) — nothing should be evicted.
        agent.context_manager.prune_protect_tokens = 999_999;

        let body = "unrelated content database schema ".repeat(50);
        let mut msg = Message {
            role: Role::User,
            content: body.clone(),
            parts: vec![MessagePart::ToolOutput {
                tool_name: "read".into(),
                body: body.clone(),
                compacted_at: None,
            }],
            metadata: MessageMetadata::default(),
        };
        msg.rebuild_content();
        agent.msg.messages.push(msg);

        let freed = agent.prune_tool_outputs_mig(1);
        assert_eq!(
            freed, 0,
            "no tokens should be freed when everything is protected"
        );

        if let MessagePart::ToolOutput { compacted_at, .. } = &agent.msg.messages[1].parts[0] {
            assert!(
                compacted_at.is_none(),
                "protected block must not be evicted"
            );
        } else {
            panic!("expected ToolOutput at messages[1]");
        }
    }
}

#[cfg(test)]
mod subgoal_extraction_tests {
    use crate::agent::context::summarization::scheduling::parse_subgoal_extraction_response;

    #[test]
    fn parse_well_formed_with_both() {
        let response = "CURRENT: Implement login\nCOMPLETED: Setup database";
        let result = parse_subgoal_extraction_response(response);
        assert_eq!(result.current, "Implement login");
        assert_eq!(result.completed, Some("Setup database".to_string()));
    }

    #[test]
    fn parse_well_formed_no_completed() {
        let response = "CURRENT: Fetch user data\nCOMPLETED: NONE";
        let result = parse_subgoal_extraction_response(response);
        assert_eq!(result.current, "Fetch user data");
        assert_eq!(result.completed, None);
    }

    #[test]
    fn parse_malformed_no_current_prefix() {
        let response = "Just some random text about subgoals";
        let result = parse_subgoal_extraction_response(response);
        assert_eq!(result.current, "Just some random text about subgoals");
        assert_eq!(result.completed, None);
    }

    #[test]
    fn parse_malformed_empty_current() {
        let response = "CURRENT: \nCOMPLETED: Setup";
        let result = parse_subgoal_extraction_response(response);
        // Empty CURRENT falls back to treating entire response as current
        assert_eq!(result.current.trim(), "CURRENT: \nCOMPLETED: Setup");
        assert_eq!(result.completed, None);
    }
}

#[cfg(test)]
mod orphan_tool_result_tests {
    use super::super::super::Agent;

    // T-ORPHAN-01: compact_context_with_budget must not produce an orphan ToolResult when
    // the boundary lands exactly on a ToolUse/ToolResult pair.
    // Regression guard for #3257: adjust_compact_end_for_tool_pairs must absorb the pair.
    #[tokio::test]
    async fn compact_context_with_budget_no_orphan_tool_result() {
        use crate::agent::tests::agent_tests::{
            MockChannel, MockToolExecutor, create_test_registry, mock_provider,
        };
        use zeph_llm::provider::{Message, MessageMetadata, MessagePart, Role};

        // 6 messages (including agent system prompt at idx 0):
        // [0] system (agent prompt, added by Agent::new)
        // [1] user "hello"
        // [2] assistant "hi"
        // [3] user "ask"
        // [4] assistant with ToolUse "t1"
        // [5] user with ToolResult "t1"   ← tail preserved
        //
        // preserve_tail=1, so raw=5; messages[5] is ToolResult-only (Role::User) →
        // adjust absorbs it → end=4; messages[4] is ToolUse-only assistant → absorb → end=3.
        // to_compact = messages[1..3] = 2 messages drained; summary inserted at idx 1.
        // Without the helper, messages[5] (ToolResult) would become idx 2 after drain —
        // orphaned after the summary — and assert_no_orphan_tool_results would fail.

        let mut agent = Agent::new(
            mock_provider(vec!["SUMMARY".to_string()]),
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        );
        agent.context_manager.compaction_preserve_tail = 1;
        agent.memory_state.compaction.structured_summaries = false;

        // Append messages after the agent system prompt (idx 0).
        agent.msg.messages.push(Message {
            role: Role::User,
            content: "hello".into(),
            parts: vec![MessagePart::Text {
                text: "hello".into(),
            }],
            metadata: MessageMetadata::default(),
        });
        agent.msg.messages.push(Message {
            role: Role::Assistant,
            content: "hi".into(),
            parts: vec![MessagePart::Text { text: "hi".into() }],
            metadata: MessageMetadata::default(),
        });
        agent.msg.messages.push(Message {
            role: Role::User,
            content: "ask".into(),
            parts: vec![MessagePart::Text { text: "ask".into() }],
            metadata: MessageMetadata::default(),
        });
        agent.msg.messages.push(Message {
            role: Role::Assistant,
            content: String::new(),
            parts: vec![MessagePart::ToolUse {
                id: "t1".into(),
                name: "shell".into(),
                input: serde_json::json!({}),
            }],
            metadata: MessageMetadata::default(),
        });
        agent.msg.messages.push(Message {
            role: Role::User,
            content: String::new(),
            parts: vec![MessagePart::ToolResult {
                tool_use_id: "t1".into(),
                content: "result".into(),
                is_error: false,
            }],
            metadata: MessageMetadata::default(),
        });

        assert_eq!(agent.msg.messages.len(), 6, "precondition: 6 messages");

        let result = agent.compact_context_with_budget(None).await;
        assert!(
            result.is_ok(),
            "compact_context_with_budget must succeed: {result:?}"
        );

        // After compaction: messages[5]=ToolResult is absorbed into tail (end moves 5→4).
        // messages[3]=user-text so preceding is NOT ToolUse → no extra absorption.
        // compact_end=4; drained messages[1..4] = 3 messages; summary inserted at idx 1.
        // Final: [0]=agent-sys, [1]=summary, [2]=ToolUse, [3]=ToolResult. Total = 4.
        assert_eq!(
            agent.msg.messages.len(),
            4,
            "should compact 3 msgs + insert summary"
        );
        assert_eq!(agent.msg.messages[1].role, Role::System, "summary at idx 1");
        assert!(
            agent.msg.messages[1]
                .content
                .starts_with("[conversation summary —"),
            "summary marker: {:?}",
            &agent.msg.messages[1].content[..agent.msg.messages[1].content.len().min(60)]
        );

        assert_no_orphan_tool_results(&agent.msg.messages);
    }

    // T-ORPHAN-02: hard compaction (LLM path) with a multi-turn transcript that ends in a
    // ToolUse/ToolResult pair must not orphan the ToolResult — e2e regression for #3258/#3255.
    #[tokio::test]
    async fn compact_context_hard_compaction_no_orphan_tool_result_e2e() {
        use crate::agent::tests::agent_tests::{
            MockChannel, MockToolExecutor, create_test_registry, mock_provider,
        };
        use zeph_llm::provider::{Message, MessageMetadata, MessagePart, Role};

        // 11 messages total (idx 0 = agent system prompt from Agent::new):
        // [0]  system  (agent prompt)
        // [1]  user    "turn 1"
        // [2]  assistant "reply 1"
        // [3]  user    "turn 2"
        // [4]  assistant ToolUse "t1"
        // [5]  user    ToolResult "t1"
        // [6]  user    "turn 3"
        // [7]  assistant "reply 3"
        // [8]  user    "turn 4"
        // [9]  assistant ToolUse "t_last"
        // [10] user    ToolResult "t_last"   ← the orphaned position from bug #3255
        //
        // preserve_tail=1 → raw=10; messages[10]=ToolResult-only user → absorb → end=9;
        // messages[9]=ToolUse-only assistant → absorb → end=8.
        // Without the helper, ToolResult at [10] would end up orphaned after drain.

        let mut agent = Agent::new(
            mock_provider(vec!["SUMMARY".to_string()]),
            MockChannel::new(vec![]),
            create_test_registry(),
            None,
            5,
            MockToolExecutor::no_tools(),
        );
        agent.context_manager.compaction_preserve_tail = 1;
        agent.memory_state.compaction.structured_summaries = false;

        let text = |role: Role, s: &str| Message {
            role,
            content: s.into(),
            parts: vec![MessagePart::Text { text: s.into() }],
            metadata: MessageMetadata::default(),
        };
        let tool_use = |id: &str| Message {
            role: Role::Assistant,
            content: String::new(),
            parts: vec![MessagePart::ToolUse {
                id: id.into(),
                name: "shell".into(),
                input: serde_json::json!({}),
            }],
            metadata: MessageMetadata::default(),
        };
        let tool_result = |id: &str| Message {
            role: Role::User,
            content: String::new(),
            parts: vec![MessagePart::ToolResult {
                tool_use_id: id.into(),
                content: "ok".into(),
                is_error: false,
            }],
            metadata: MessageMetadata::default(),
        };

        agent.msg.messages.push(text(Role::User, "turn 1"));
        agent.msg.messages.push(text(Role::Assistant, "reply 1"));
        agent.msg.messages.push(text(Role::User, "turn 2"));
        agent.msg.messages.push(tool_use("t1"));
        agent.msg.messages.push(tool_result("t1"));
        agent.msg.messages.push(text(Role::User, "turn 3"));
        agent.msg.messages.push(text(Role::Assistant, "reply 3"));
        agent.msg.messages.push(text(Role::User, "turn 4"));
        agent.msg.messages.push(tool_use("t_last"));
        agent.msg.messages.push(tool_result("t_last"));

        assert_eq!(agent.msg.messages.len(), 11, "precondition: 11 messages");

        let result = agent.compact_context_with_budget(None).await;
        assert!(
            result.is_ok(),
            "compact_context_with_budget must succeed: {result:?}"
        );

        assert_eq!(agent.msg.messages[1].role, Role::System, "summary at idx 1");
        assert!(
            agent.msg.messages[1]
                .content
                .starts_with("[conversation summary —"),
            "summary marker: {:?}",
            &agent.msg.messages[1].content[..agent.msg.messages[1].content.len().min(60)]
        );

        assert!(
            agent.msg.messages.len() < 11,
            "compaction must have reduced message count (got {})",
            agent.msg.messages.len()
        );
        assert_no_orphan_tool_results(&agent.msg.messages);
    }

    fn assert_no_orphan_tool_results(messages: &[zeph_llm::provider::Message]) {
        use zeph_llm::provider::MessagePart;
        for (i, msg) in messages.iter().enumerate() {
            for part in &msg.parts {
                if let MessagePart::ToolResult { tool_use_id, .. } = part {
                    assert!(i > 0, "orphan ToolResult at idx 0: no preceding message");
                    let matched = messages[i - 1]
                        .parts
                        .iter()
                        .any(|p| matches!(p, MessagePart::ToolUse { id, .. } if id == tool_use_id));
                    assert!(
                        matched,
                        "orphan ToolResult at idx {i} (tool_use_id={tool_use_id:?}): \
                         preceding message has no matching ToolUse"
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod compact_end_tool_pair_tests {
    use zeph_llm::provider::{Message, MessageMetadata, MessagePart, Role};

    use super::super::super::Agent;
    use crate::agent::tests::agent_tests::MockChannel;

    fn tool_use_msg() -> Message {
        Message {
            role: Role::Assistant,
            content: String::new(),
            parts: vec![MessagePart::ToolUse {
                id: "tu1".into(),
                name: "shell".into(),
                input: serde_json::json!({}),
            }],
            metadata: MessageMetadata::default(),
        }
    }

    fn tool_result_msg() -> Message {
        Message {
            role: Role::User,
            content: String::new(),
            parts: vec![MessagePart::ToolResult {
                tool_use_id: "tu1".into(),
                content: "ok".into(),
                is_error: false,
            }],
            metadata: MessageMetadata::default(),
        }
    }

    fn text_msg(role: Role, text: &str) -> Message {
        Message {
            role,
            content: text.into(),
            parts: vec![MessagePart::Text { text: text.into() }],
            metadata: MessageMetadata::default(),
        }
    }

    fn system_msg() -> Message {
        Message {
            role: Role::System,
            content: "system".into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        }
    }

    // Alias for the static helper under test.
    fn adjust(messages: &[Message], compact_end: usize) -> usize {
        Agent::<MockChannel>::adjust_compact_end_for_tool_pairs(messages, compact_end)
    }

    #[test]
    fn no_tool_pair_at_boundary_unchanged() {
        // [sys, user, assistant_text, user2]  preserve_tail=1 → compact_end=3
        // messages[3] = user2 (plain text) → no adjustment
        let msgs = vec![
            system_msg(),
            text_msg(Role::User, "hi"),
            text_msg(Role::Assistant, "ok"),
            text_msg(Role::User, "bye"),
        ];
        assert_eq!(adjust(&msgs, 3), 3);
    }

    #[test]
    fn tool_result_at_boundary_absorbs_pair() {
        // [sys, user, tool_use, tool_result, user2]  preserve_tail=2 → compact_end=3
        // messages[3] = tool_result → adjust to 2, then absorb tool_use → compact_end=2
        let msgs = vec![
            system_msg(),
            text_msg(Role::User, "hi"),
            tool_use_msg(),
            tool_result_msg(),
            text_msg(Role::User, "bye"),
        ];
        assert_eq!(adjust(&msgs, 3), 2);
    }

    #[test]
    fn multiple_tool_results_absorbed() {
        // Parallel tool calls: one tool_use, two tool_results (two result messages)
        let tool_result2 = Message {
            role: Role::User,
            content: String::new(),
            parts: vec![MessagePart::ToolResult {
                tool_use_id: "tu2".into(),
                content: "ok2".into(),
                is_error: false,
            }],
            metadata: MessageMetadata::default(),
        };
        // [sys, user, tool_use, tool_result, tool_result2, assistant_reply]
        // preserve_tail=2 → compact_end=4
        // messages[4] = tool_result2 → absorb; messages[3] = tool_result → absorb;
        // messages[2] = tool_use → absorb; compact_end=2
        let msgs = vec![
            system_msg(),
            text_msg(Role::User, "hi"),
            tool_use_msg(),
            tool_result_msg(),
            tool_result2,
            text_msg(Role::Assistant, "done"),
        ];
        assert_eq!(adjust(&msgs, 4), 2);
    }

    #[test]
    fn preserve_tail_zero_no_tool_result_unchanged() {
        // [sys, user, assistant]  compact_end=3 (preserve_tail=0)
        // We only call with compact_end < len in practice; test a valid boundary.
        let msgs = vec![
            system_msg(),
            text_msg(Role::User, "hi"),
            text_msg(Role::Assistant, "ok"),
        ];
        assert_eq!(adjust(&msgs, 2), 2);
    }

    #[test]
    fn compact_end_equals_len_does_not_panic() {
        // preserve_tail=0 → compact_end = messages.len(); must not panic on out-of-bounds.
        let msgs = vec![
            system_msg(),
            text_msg(Role::User, "hi"),
            tool_use_msg(),
            tool_result_msg(),
        ];
        // compact_end = 4 = msgs.len(); bounds guard must fire and return unchanged.
        assert_eq!(adjust(&msgs, msgs.len()), msgs.len());
    }

    #[test]
    fn compact_end_already_one_returns_one() {
        let msgs = vec![system_msg(), tool_result_msg()];
        assert_eq!(adjust(&msgs, 1), 1);
    }

    #[test]
    fn only_tool_pairs_degenerate_returns_one() {
        // All non-system messages are tool pairs; compact_end points into the pair.
        let msgs = vec![system_msg(), tool_use_msg(), tool_result_msg()];
        // compact_end=2 → messages[2]=tool_result → absorb → end=1; tool_use at [1]
        // preceding = messages[0] = system (not tool_use) so no extra absorption
        // end = 1, max(1) = 1
        assert_eq!(adjust(&msgs, 2), 1);
    }
}
