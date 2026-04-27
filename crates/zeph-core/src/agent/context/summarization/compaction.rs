// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use zeph_agent_context::state::CompactionOutcome;

use crate::agent::Agent;
use crate::channel::Channel;

impl<C: Channel> Agent<C> {
    /// Compact the context window using LLM summarization.
    ///
    /// Thin shim that wires the four `Agent<C>`-specific adapters (probe, archive,
    /// persistence, metrics) into a [`ContextSummarizationView`] and delegates all
    /// structural logic to [`zeph_agent_context::ContextService::compact_context`].
    ///
    /// The optional Qdrant session-summary future returned by the service is dispatched
    /// through `BackgroundSupervisor::spawn_summarization` so the `JoinHandle` is tracked
    /// and bounded per Await Discipline rule 2.
    ///
    /// # Errors
    ///
    /// Returns [`AgentError`] if LLM summarization fails.
    ///
    /// [`ContextSummarizationView`]: zeph_agent_context::state::ContextSummarizationView
    /// [`AgentError`]: crate::agent::error::AgentError
    pub(in crate::agent) async fn compact_context(
        &mut self,
    ) -> Result<CompactionOutcome, crate::agent::error::AgentError> {
        let guidelines = self.load_compression_guidelines_for_compact().await; // 1
        let mut adapters = super::adapters::CompactionAdapters::new(self); // 2
        let tokens_before = self.runtime.providers.cached_prompt_tokens; // 3

        let mut summ = self
            .summarization_view()
            .with_compression_guidelines(guidelines); // 4
        adapters.populate(&mut summ); // 5

        let svc = zeph_agent_context::ContextService::new();
        let mut outcome = svc
            .compact_context(&mut summ, None)
            .await
            .map_err(|e| crate::agent::error::AgentError::ContextError(format!("{e:#}")))?; // 6

        if outcome.is_compacted() {
            self.update_metrics(|m| m.context_compactions += 1); // 7
        }

        if let Some(fut) = outcome.qdrant_future_take() {
            self.runtime
                .lifecycle
                .supervisor
                .spawn_summarization("persist-session-summary", fut); // 8+9
        }

        self.emit_compaction_status_signal(tokens_before).await; // 10
        Ok(outcome) // 11
    }

    /// Compact context and return a user-visible status string.
    ///
    /// This wrapper exists to give `agent_access_impl` a single `async fn(&mut self)` call
    /// point that the HRTB checker can verify as `Send`. The inner `compact_context()` method
    /// uses only owned data and cloned `Arc`s across every `.await` point.
    pub(in crate::agent) async fn compact_context_command(
        &mut self,
    ) -> Result<String, zeph_commands::CommandError> {
        if self.msg.messages.len() <= self.context_manager.compaction_preserve_tail + 1 {
            return Ok("Nothing to compact.".to_owned());
        }
        match self
            .compact_context()
            .await
            .map_err(|e| zeph_commands::CommandError::new(e.to_string()))?
        {
            CompactionOutcome::Compacted { .. } | CompactionOutcome::NoChange => {
                Ok("Context compacted successfully.".to_owned())
            }
            CompactionOutcome::CompactedWithPersistError { .. } => {
                Ok("Context compacted, but persistence to storage failed (see logs).".to_owned())
            }
            CompactionOutcome::ProbeRejected => {
                Ok("Compaction rejected: summary quality below threshold. \
                 Original context preserved."
                    .to_owned())
            }
        }
    }

    /// Load compression guidelines, returning `None` when the feature is disabled.
    ///
    /// Extracts all fields from `&self` synchronously before the first `.await` so
    /// `&self` is not held across the await boundary (required for `Send` futures).
    pub(super) async fn load_compression_guidelines_for_compact(&mut self) -> Option<String> {
        let enabled = self
            .services
            .memory
            .compaction
            .compression_guidelines_config
            .enabled;
        let memory = self.services.memory.persistence.memory.clone();
        let conv_id = self.services.memory.persistence.conversation_id;
        // Drop the borrow on &self before awaiting.
        let text = Self::load_compression_guidelines(enabled, memory, conv_id).await;
        if text.is_empty() { None } else { Some(text) }
    }
}

// ── Test-helper delegations ───────────────────────────────────────────────────
//
// `compact_context_with_budget` is used exclusively by integration tests in
// `crates/zeph-core/src/agent/context/summarization/mod.rs` (T-ORPHAN-01, T-ORPHAN-02).
// It delegates to `ContextService::compact_context` which exercises the same structural
// invariants (orphan-pair detection, adjust_compact_end_for_tool_pairs) without the
// Agent-specific probe/archive/persist/Qdrant logic.
#[cfg(test)]
impl<C: Channel> Agent<C> {
    pub(in crate::agent::context) async fn compact_context_with_budget(
        &mut self,
        max_summary_tokens: Option<usize>,
    ) -> Result<(), crate::agent::error::AgentError> {
        let svc = zeph_agent_context::ContextService::new();
        let mut summ = self.summarization_view();
        svc.compact_context(&mut summ, max_summary_tokens)
            .await
            .map(|_| ())
            .map_err(|e| crate::agent::error::AgentError::ContextError(format!("{e:#}")))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use zeph_llm::any::AnyProvider;
    use zeph_memory::semantic::SemanticMemory;

    use crate::agent::Agent;
    use crate::agent::agent_tests::{MockChannel, MockToolExecutor, create_test_registry};

    /// Verify that `load_compression_guidelines_for_compact` returns `Some` containing
    /// the stored guideline text when the feature is enabled and a guideline has been saved.
    ///
    /// Regression guard for Fix #2 (issue #3533): the proactive compression path in
    /// `maybe_proactive_compress` calls this function and passes the result as
    /// `.with_compression_guidelines(guidelines)` on the view. A silent regression here
    /// would mean guidelines are never loaded, causing `ContextSummarizationView::
    /// compression_guidelines` to always be `None` in the proactive path.
    #[tokio::test]
    async fn compression_guidelines_loaded_from_sqlite_when_enabled() {
        let embed_provider = AnyProvider::Mock(zeph_llm::mock::MockProvider::default());
        let memory = SemanticMemory::new(
            ":memory:",
            "http://127.0.0.1:1",
            embed_provider,
            "test-model",
        )
        .await
        .unwrap();

        let cid = memory.sqlite().create_conversation().await.unwrap();

        // Store a guideline in the in-memory SQLite DB.
        let expected = "preserve function signatures verbatim";
        memory
            .sqlite()
            .save_compression_guidelines(expected, 5, Some(cid))
            .await
            .unwrap();

        let provider = AnyProvider::Mock(zeph_llm::mock::MockProvider::with_responses(vec![]));
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_memory(
            Arc::new(memory),
            cid,
            50,
            5,
            100,
        );

        // Enable the compression guidelines feature flag — off by default.
        agent
            .services
            .memory
            .compaction
            .compression_guidelines_config
            .enabled = true;

        let result = agent.load_compression_guidelines_for_compact().await;

        assert_eq!(
            result.as_deref(),
            Some(expected),
            "compression_guidelines must be loaded from SQLite and returned as Some"
        );
    }

    /// Verify that `load_compression_guidelines_for_compact` returns `None` when the
    /// feature flag is disabled, even if a guideline is stored in the database.
    ///
    /// Prevents inadvertent activation of guidelines when the user has not opted in.
    #[tokio::test]
    async fn compression_guidelines_returns_none_when_feature_disabled() {
        let embed_provider = AnyProvider::Mock(zeph_llm::mock::MockProvider::default());
        let memory = SemanticMemory::new(
            ":memory:",
            "http://127.0.0.1:1",
            embed_provider,
            "test-model",
        )
        .await
        .unwrap();

        let cid = memory.sqlite().create_conversation().await.unwrap();
        memory
            .sqlite()
            .save_compression_guidelines("should not be loaded", 4, Some(cid))
            .await
            .unwrap();

        let provider = AnyProvider::Mock(zeph_llm::mock::MockProvider::with_responses(vec![]));
        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        // Feature flag defaults to disabled.
        let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_memory(
            Arc::new(memory),
            cid,
            50,
            5,
            100,
        );

        assert!(
            !agent
                .services
                .memory
                .compaction
                .compression_guidelines_config
                .enabled,
            "compression_guidelines_config must be disabled by default"
        );

        let result = agent.load_compression_guidelines_for_compact().await;

        assert!(
            result.is_none(),
            "must return None when feature flag is disabled; got: {result:?}"
        );
    }

    /// Verify that `compression_guidelines` stored in `SQLite` flow through the proactive
    /// compression path and are embedded in the LLM summarization prompt.
    ///
    /// Regression guard for issue #3533: `maybe_proactive_compress` now calls
    /// `load_compression_guidelines_for_compact` and wires the result through
    /// `.with_compression_guidelines(guidelines)` before delegating to
    /// `ContextService::maybe_proactive_compress`. This test confirms the full
    /// data-flow: `SQLite` → `load_compression_guidelines_for_compact` → view field →
    /// `summarize_with_llm` prompt.
    ///
    /// A regression would cause the LLM prompt to omit the `<compression-guidelines>` block,
    /// silently degrading compaction quality without any error.
    #[tokio::test]
    async fn compression_guidelines_flow_through_proactive_compress_path() {
        use zeph_config::CompressionStrategy;
        use zeph_llm::mock::MockProvider;
        use zeph_llm::provider::{Message, MessageMetadata, Role};

        let embed_provider = AnyProvider::Mock(zeph_llm::mock::MockProvider::default());
        let memory = SemanticMemory::new(
            ":memory:",
            "http://127.0.0.1:1",
            embed_provider,
            "test-model",
        )
        .await
        .unwrap();

        let cid = memory.sqlite().create_conversation().await.unwrap();

        // Store a guideline that must appear in the LLM prompt.
        let guideline = "always preserve function signatures verbatim";
        memory
            .sqlite()
            .save_compression_guidelines(guideline, 5, Some(cid))
            .await
            .unwrap();

        // Wire a recording mock so we can inspect the exact LLM request.
        let (mock, recorded) =
            MockProvider::with_responses(vec!["summary text".to_string()]).with_recording();
        let provider = AnyProvider::Mock(mock);

        let channel = MockChannel::new(vec![]);
        let registry = create_test_registry();
        let executor = MockToolExecutor::no_tools();

        let mut agent = Agent::new(provider, channel, registry, None, 5, executor).with_memory(
            Arc::new(memory),
            cid,
            50,
            5,
            100,
        );

        // Enable guidelines and disable structured summaries so the non-structured
        // single-pass path is taken (guidelines injected by `build_chunk_prompt`).
        agent
            .services
            .memory
            .compaction
            .compression_guidelines_config
            .enabled = true;
        agent.services.memory.compaction.structured_summaries = false;

        // Configure Proactive strategy with a threshold below our token count.
        agent.context_manager.compression.strategy = CompressionStrategy::Proactive {
            threshold_tokens: 500,
            max_summary_tokens: 200,
        };
        // Set token pressure above the threshold so `should_proactively_compress` fires.
        agent.runtime.providers.cached_prompt_tokens = 600;

        // Add enough messages so compactable > 1 (preserve_tail defaults to 6,
        // so we need len > 6 + 2 = 9; 10 additional messages give len = 11).
        for i in 0..10 {
            let role = if i % 2 == 0 {
                Role::User
            } else {
                Role::Assistant
            };
            agent.msg.messages.push(Message {
                role,
                content: format!("message {i}"),
                parts: vec![],
                metadata: MessageMetadata::default(),
            });
        }

        agent.maybe_proactive_compress().await.unwrap();

        // At least one LLM call must have been made (recording is non-empty).
        let calls = recorded.lock().unwrap();
        assert!(
            !calls.is_empty(),
            "LLM must be called during proactive compression"
        );

        // The guideline must appear in the messages sent to the LLM — confirming
        // `with_compression_guidelines` was correctly populated from SQLite.
        let guideline_in_prompt = calls
            .iter()
            .any(|msgs| msgs.iter().any(|m| m.content.contains(guideline)));
        assert!(
            guideline_in_prompt,
            "compression guideline must be embedded in the LLM summarization prompt; \
             recorded call contents: {:?}",
            calls
                .iter()
                .flat_map(|msgs| msgs.iter().map(|m| &m.content))
                .collect::<Vec<_>>()
        );
    }
}
