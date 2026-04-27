// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! [`ContextService`] — stateless façade for agent context-assembly operations.

use crate::error::ContextError;
use crate::state::{
    ContextAssemblyView, ContextSummarizationView, MessageWindowView, ProviderHandles, StatusSink,
    TrustGate,
};

/// Stateless façade for agent context-assembly operations.
///
/// This struct has no fields. All state flows through method parameters, which allows the
/// borrow checker to see disjoint `&mut` borrows at the call site without hiding them
/// inside an opaque bundle.
///
/// Methods are `&self` — the type exists only to namespace the operations and give callers
/// a single import.
///
/// # Examples
///
/// ```no_run
/// use zeph_agent_context::service::ContextService;
///
/// let svc = ContextService::new();
/// // call svc.prepare_context(...) or svc.rebuild_system_prompt(...)
/// ```
#[derive(Debug, Default)]
pub struct ContextService;

impl ContextService {
    /// Create a new stateless `ContextService`.
    ///
    /// This is a zero-cost constructor — the struct has no fields.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Prepare the context window for the current turn.
    ///
    /// Removes stale injection messages, gathers semantic recall and graph facts,
    /// applies the configured retrieval policy, and injects the fresh context block.
    ///
    /// # Errors
    ///
    /// Returns [`ContextError::Memory`] if recall fails, [`ContextError::Assembler`]
    /// if the context assembler encounters an internal error.
    pub async fn prepare_context(
        &self,
        _query: &str,
        _window: &mut MessageWindowView<'_>,
        _view: &mut ContextAssemblyView<'_>,
        _providers: &ProviderHandles,
        _status: &(impl StatusSink + ?Sized),
    ) -> Result<(), ContextError> {
        // TODO: implement in Step 5 migration
        unimplemented!("prepare_context will be implemented in Step 5")
    }

    /// Rebuild the system prompt for the current turn.
    ///
    /// Updates the skill catalog, applies channel-skill filters, and rewrites the
    /// first message in `window.messages` with the new system prompt.
    pub async fn rebuild_system_prompt(
        &self,
        _query: &str,
        _window: &mut MessageWindowView<'_>,
        _view: &mut ContextAssemblyView<'_>,
        _providers: &ProviderHandles,
        _trust_gate: &(impl TrustGate + ?Sized),
        _status: &(impl StatusSink + ?Sized),
    ) {
        // TODO: implement in Step 6 migration
        unimplemented!("rebuild_system_prompt will be implemented in Step 6")
    }

    /// Reset the conversation history.
    ///
    /// Clears all messages except the system prompt and resets context-manager state.
    ///
    /// # Errors
    ///
    /// Returns [`ContextError::Memory`] if the persistence layer fails to record the reset.
    pub async fn reset_conversation(
        &self,
        _window: &mut MessageWindowView<'_>,
        _view: &mut ContextAssemblyView<'_>,
    ) -> Result<(), ContextError> {
        // TODO: implement in Step 7 migration
        unimplemented!("reset_conversation will be implemented in Step 7")
    }

    /// Run compaction if the token budget is exhausted.
    ///
    /// Dispatches to the appropriate compaction tier based on the current
    /// context manager state.
    ///
    /// # Errors
    ///
    /// Returns [`ContextError::Memory`] if compaction persistence fails.
    pub async fn maybe_compact(
        &self,
        _summ: &mut ContextSummarizationView<'_>,
        _providers: &ProviderHandles,
        _status: &(impl StatusSink + ?Sized),
    ) -> Result<(), ContextError> {
        // TODO: implement in Step 8 migration
        unimplemented!("maybe_compact will be implemented in Step 8")
    }

    /// Summarize the most recent tool-use/result pair if it exceeds the budget.
    pub async fn maybe_summarize_tool_pair(
        &self,
        _summ: &mut ContextSummarizationView<'_>,
        _providers: &ProviderHandles,
    ) {
        // TODO: implement in Step 8 migration
        unimplemented!("maybe_summarize_tool_pair will be implemented in Step 8")
    }

    /// Apply any deferred summaries to the message window.
    ///
    /// Returns the number of summaries applied.
    #[must_use]
    pub fn apply_deferred_summaries(&self, _summ: &mut ContextSummarizationView<'_>) -> usize {
        // TODO: implement in Step 8 migration
        unimplemented!("apply_deferred_summaries will be implemented in Step 8")
    }

    /// Flush all deferred summaries to the message window.
    pub async fn flush_deferred_summaries(&self, _summ: &mut ContextSummarizationView<'_>) {
        // TODO: implement in Step 8 migration
        unimplemented!("flush_deferred_summaries will be implemented in Step 8")
    }

    /// Apply deferred summaries if the compaction budget permits.
    pub fn maybe_apply_deferred_summaries(&self, _summ: &mut ContextSummarizationView<'_>) {
        // TODO: implement in Step 8 migration
        unimplemented!("maybe_apply_deferred_summaries will be implemented in Step 8")
    }

    /// Apply a soft compaction pass mid-iteration if required.
    pub fn maybe_soft_compact_mid_iteration(&self, _summ: &mut ContextSummarizationView<'_>) {
        // TODO: implement in Step 8 migration
        unimplemented!("maybe_soft_compact_mid_iteration will be implemented in Step 8")
    }

    /// Run proactive compression if the token usage crosses the configured threshold.
    pub async fn maybe_proactive_compress(
        &self,
        _summ: &mut ContextSummarizationView<'_>,
        _providers: &ProviderHandles,
        _status: &(impl StatusSink + ?Sized),
    ) {
        // TODO: implement in Step 8 migration
        unimplemented!("maybe_proactive_compress will be implemented in Step 8")
    }

    /// Refresh the task goal summary if it has expired.
    pub fn maybe_refresh_task_goal(&self, _summ: &mut ContextSummarizationView<'_>) {
        // TODO: implement in Step 8 migration
        unimplemented!("maybe_refresh_task_goal will be implemented in Step 8")
    }

    /// Refresh the subgoal summary if it has expired.
    pub fn maybe_refresh_subgoal(&self, _summ: &mut ContextSummarizationView<'_>) {
        // TODO: implement in Step 8 migration
        unimplemented!("maybe_refresh_subgoal will be implemented in Step 8")
    }

    /// Clear the message history, preserving the system prompt.
    pub fn clear_history(&self, _window: &mut MessageWindowView<'_>) {
        // TODO: implement in Step 4 migration
        unimplemented!("clear_history will be implemented in Step 4")
    }

    /// Remove semantic recall messages from the window.
    pub fn remove_recall_messages(&self, _window: &mut MessageWindowView<'_>) {
        // TODO: implement in Step 4 migration
        unimplemented!("remove_recall_messages will be implemented in Step 4")
    }
}
