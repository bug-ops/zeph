// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use crate::agent::Agent;
use crate::channel::Channel;

impl<C: Channel> Agent<C> {
    pub(in crate::agent) async fn maybe_summarize_tool_pair(&mut self) {
        let svc = zeph_agent_context::ContextService::new();
        let providers = self.providers();
        let mut summ = self.summarization_view();
        svc.maybe_summarize_tool_pair(&mut summ, &providers).await;
    }

    /// Batch-apply all pending deferred tool pair summaries.
    ///
    /// Returns the number of summaries applied.
    pub(in crate::agent) fn apply_deferred_summaries(&mut self) -> usize {
        let svc = zeph_agent_context::ContextService::new();
        let mut summ = self.summarization_view();
        svc.apply_deferred_summaries(&mut summ)
    }

    pub(in crate::agent) async fn flush_deferred_summaries(&mut self) {
        let svc = zeph_agent_context::ContextService::new();
        let mut summ = self.summarization_view();
        svc.flush_deferred_summaries(&mut summ).await;
    }

    pub(in crate::agent) fn maybe_apply_deferred_summaries(&mut self) {
        let svc = zeph_agent_context::ContextService::new();
        let mut summ = self.summarization_view();
        svc.maybe_apply_deferred_summaries(&mut summ);
    }
}

// ── Test-helper delegations ───────────────────────────────────────────────
//
// These methods are used exclusively by integration tests in
// `crates/zeph-core/src/agent/context/tests/`. They delegate to the
// stateless helpers in `zeph-agent-context` so tests exercise the same code
// path as production without re-implementing the logic here.
#[cfg(test)]
impl<C: Channel> Agent<C> {
    pub(in crate::agent::context) fn count_unsummarized_pairs(&self) -> usize {
        zeph_agent_context::summarization::count_unsummarized_pairs(&self.msg.messages)
    }

    pub(in crate::agent::context) fn find_oldest_unsummarized_pair(
        &self,
    ) -> Option<(usize, usize)> {
        zeph_agent_context::summarization::find_oldest_unsummarized_pair(&self.msg.messages)
    }

    pub(in crate::agent::context) fn count_deferred_summaries(&self) -> usize {
        zeph_agent_context::summarization::count_deferred_summaries(&self.msg.messages)
    }

    pub(in crate::agent::context) fn build_tool_pair_summary_prompt(
        req: &zeph_llm::provider::Message,
        res: &zeph_llm::provider::Message,
    ) -> String {
        zeph_context::summarization::build_tool_pair_summary_prompt(req, res)
    }

    pub(in crate::agent::context) fn build_chunk_prompt(
        messages: &[zeph_llm::provider::Message],
        guidelines: &str,
    ) -> String {
        zeph_context::summarization::build_chunk_prompt(messages, guidelines)
    }
}
