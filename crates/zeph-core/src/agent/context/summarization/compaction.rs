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
    async fn load_compression_guidelines_for_compact(&mut self) -> Option<String> {
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
