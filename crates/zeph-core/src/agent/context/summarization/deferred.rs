// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use zeph_llm::provider::{LlmProvider, Message, MessageMetadata, MessagePart, Role};

use crate::agent::Agent;
use crate::agent::context_manager::CompactionTier;
use crate::channel::Channel;

impl<C: Channel> Agent<C> {
    pub(in crate::agent::context) fn count_unsummarized_pairs(&self) -> usize {
        let mut count = 0usize;
        let mut i = 1; // skip system prompt
        while i < self.msg.messages.len() {
            let msg = &self.msg.messages[i];
            if !msg.metadata.visibility.is_agent_visible() {
                i += 1;
                continue;
            }
            let is_tool_request = msg.role == Role::Assistant
                && msg
                    .parts
                    .iter()
                    .any(|p| matches!(p, MessagePart::ToolUse { .. }));
            if is_tool_request && i + 1 < self.msg.messages.len() {
                let next = &self.msg.messages[i + 1];
                if next.metadata.visibility.is_agent_visible()
                    && next.role == Role::User
                    && next.parts.iter().any(|p| {
                        matches!(
                            p,
                            MessagePart::ToolResult { .. } | MessagePart::ToolOutput { .. }
                        )
                    })
                    && next.metadata.deferred_summary.is_none()
                {
                    count += 1;
                    i += 2;
                    continue;
                }
            }
            i += 1;
        }
        count
    }

    /// Find the oldest tool request/response pair that has not yet been summarized.
    ///
    /// Skips pairs where:
    /// - `deferred_summary` is already set (already queued for application), or
    /// - the response content was pruned (all ToolResult/ToolOutput bodies are empty or
    ///   contain only `"[pruned]"`), which would produce a useless summary (IMP-03 fix).
    pub(in crate::agent::context) fn find_oldest_unsummarized_pair(
        &self,
    ) -> Option<(usize, usize)> {
        let mut i = 1; // skip system prompt
        while i < self.msg.messages.len() {
            let msg = &self.msg.messages[i];
            if !msg.metadata.visibility.is_agent_visible() {
                i += 1;
                continue;
            }
            let is_tool_request = msg.role == Role::Assistant
                && msg
                    .parts
                    .iter()
                    .any(|p| matches!(p, MessagePart::ToolUse { .. }));
            if is_tool_request && i + 1 < self.msg.messages.len() {
                let next = &self.msg.messages[i + 1];
                if next.metadata.visibility.is_agent_visible()
                    && next.role == Role::User
                    && next.parts.iter().any(|p| {
                        matches!(
                            p,
                            MessagePart::ToolResult { .. } | MessagePart::ToolOutput { .. }
                        )
                    })
                    && next.metadata.deferred_summary.is_none()
                {
                    // Skip pairs whose response content has been fully pruned — summarizing
                    // "[pruned]" produces a useless result (IMP-03).
                    let all_pruned = next.parts.iter().all(|p| match p {
                        MessagePart::ToolOutput { body, .. } => body.is_empty(),
                        MessagePart::ToolResult { content, .. } => {
                            content.trim() == "[pruned]" || content.is_empty()
                        }
                        _ => true,
                    });
                    if !all_pruned {
                        return Some((i, i + 1));
                    }
                }
            }
            i += 1;
        }
        None
    }

    pub(in crate::agent::context) fn count_deferred_summaries(&self) -> usize {
        self.msg
            .messages
            .iter()
            .filter(|m| m.metadata.deferred_summary.is_some())
            .count()
    }

    pub(in crate::agent::context) fn build_tool_pair_summary_prompt(
        req: &Message,
        res: &Message,
    ) -> String {
        zeph_context::summarization::build_tool_pair_summary_prompt(req, res)
    }

    pub(in crate::agent) async fn maybe_summarize_tool_pair(&mut self) {
        // Drain the entire backlog above cutoff in one turn so that a resumed session
        // with many accumulated pairs catches up before Tier 1 pruning fires.
        let cutoff = self.services.memory.persistence.tool_call_cutoff;
        let llm_timeout = std::time::Duration::from_secs(self.runtime.config.timeouts.llm_seconds);
        let mut summarized = 0usize;
        loop {
            let pair_count = self.count_unsummarized_pairs();
            if pair_count <= cutoff {
                break;
            }
            let Some((req_idx, resp_idx)) = self.find_oldest_unsummarized_pair() else {
                break;
            };
            let prompt = Self::build_tool_pair_summary_prompt(
                &self.msg.messages[req_idx],
                &self.msg.messages[resp_idx],
            );
            let msgs = [Message {
                role: Role::User,
                content: prompt,
                parts: vec![],
                metadata: MessageMetadata::default(),
            }];
            let _ = self.channel.send_status("summarizing output...").await;
            let chat_fut = self.summary_or_primary_provider().chat(&msgs);
            let summary = match tokio::time::timeout(llm_timeout, chat_fut).await {
                Ok(Ok(s)) => s,
                Ok(Err(e)) => {
                    tracing::warn!(%e, "tool pair summarization failed, stopping batch");
                    let _ = self.channel.send_status("").await;
                    break;
                }
                Err(_elapsed) => {
                    tracing::warn!(
                        timeout_secs = self.runtime.config.timeouts.llm_seconds,
                        "tool pair summarization timed out, stopping batch"
                    );
                    let _ = self.channel.send_status("").await;
                    break;
                }
            };
            // DEFERRED: store summary on response metadata instead of immediately mutating the
            // array. Applied lazily by apply_deferred_summaries() when context pressure rises,
            // preserving the message prefix for Claude API cache hits.
            let summary =
                super::super::cap_summary(self.maybe_redact(&summary).into_owned(), 8_000);
            self.msg.messages[resp_idx].metadata.deferred_summary = Some(summary.clone());
            summarized += 1;
            tracing::debug!(
                pair_count,
                cutoff,
                req_idx,
                resp_idx,
                summary_len = summary.len(),
                "deferred tool pair summary stored"
            );
        }
        let _ = self.channel.send_status("").await;
        if summarized > 0 {
            tracing::info!(summarized, "batch-summarized tool pairs above cutoff");
        }
    }

    /// Batch-apply all pending deferred tool pair summaries.
    ///
    /// Processes in reverse index order (highest first) so that inserting a summary message
    /// at `resp_idx + 1` does not shift the indices of not-yet-processed pairs.
    ///
    /// Returns the number of summaries applied.
    pub(in crate::agent) fn apply_deferred_summaries(&mut self) -> usize {
        // Phase 1: collect (resp_idx, req_idx, summary) for all messages with deferred_summary.
        let mut targets: Vec<(usize, usize, String)> = Vec::new();
        for i in 1..self.msg.messages.len() {
            if self.msg.messages[i].metadata.deferred_summary.is_none() {
                continue;
            }
            // Verify the structural invariant: tool response preceded by matching tool request.
            if self.msg.messages[i].role == Role::User
                && self.msg.messages[i].metadata.visibility.is_agent_visible()
                && i > 0
                && self.msg.messages[i - 1].role == Role::Assistant
                && self.msg.messages[i - 1]
                    .metadata
                    .visibility
                    .is_agent_visible()
                && self.msg.messages[i - 1]
                    .parts
                    .iter()
                    .any(|p| matches!(p, MessagePart::ToolUse { .. }))
            {
                let summary = self.msg.messages[i]
                    .metadata
                    .deferred_summary
                    .clone()
                    .expect("checked above");
                targets.push((i, i - 1, summary));
            } else {
                tracing::warn!(
                    resp_idx = i,
                    "deferred summary orphaned: req message not found at resp_idx={i}"
                );
            }
        }

        if targets.is_empty() {
            return 0;
        }

        // Phase 2: sort descending by resp_idx so insertions do not invalidate lower indices.
        targets.sort_by_key(|target| std::cmp::Reverse(target.0));

        let count = targets.len();
        for (resp_idx, req_idx, summary) in targets {
            let req_db_id = self.msg.messages[req_idx].metadata.db_id;
            let resp_db_id = self.msg.messages[resp_idx].metadata.db_id;

            self.msg.messages[req_idx].metadata.visibility =
                zeph_llm::provider::MessageVisibility::UserOnly;
            self.msg.messages[resp_idx].metadata.visibility =
                zeph_llm::provider::MessageVisibility::UserOnly;
            self.msg.messages[resp_idx].metadata.deferred_summary = None;

            if let (Some(req_id), Some(resp_id)) = (req_db_id, resp_db_id) {
                self.msg.deferred_db_hide_ids.push(req_id);
                self.msg.deferred_db_hide_ids.push(resp_id);
                self.msg.deferred_db_summaries.push(summary.clone());
            }

            let content = format!("[tool summary] {summary}");
            let summary_msg = Message {
                role: Role::Assistant,
                content,
                parts: vec![MessagePart::Summary { text: summary }],
                metadata: MessageMetadata::agent_only(),
            };
            self.msg.messages.insert(resp_idx + 1, summary_msg);
        }

        self.recompute_prompt_tokens();
        tracing::info!(count, "applied deferred tool pair summaries");
        count
    }

    pub(in crate::agent) async fn flush_deferred_summaries(&mut self) {
        if self.msg.deferred_db_hide_ids.is_empty() {
            return;
        }
        let (Some(memory), Some(cid)) = (
            &self.services.memory.persistence.memory,
            self.services.memory.persistence.conversation_id,
        ) else {
            self.msg.deferred_db_hide_ids.clear();
            self.msg.deferred_db_summaries.clear();
            return;
        };
        let hide_ids = std::mem::take(&mut self.msg.deferred_db_hide_ids);
        let summaries = std::mem::take(&mut self.msg.deferred_db_summaries);
        if let Err(e) = memory
            .sqlite()
            .apply_tool_pair_summaries(cid, &hide_ids, &summaries)
            .await
        {
            tracing::warn!(error = %e, "failed to flush deferred summary batch to DB");
        }
    }

    /// Apply deferred summaries if context usage exceeds the soft compaction threshold,
    /// or when enough summaries have accumulated to prevent content loss from pruning.
    ///
    /// Two triggers:
    /// - Token pressure: `cached_prompt_tokens > budget * soft_compaction_threshold`
    /// - Count pressure: `pending >= tool_call_cutoff` (guards against pruning replacing
    ///   summaries with `[pruned]` when `prepare_context` recomputes tokens to a low value)
    ///
    /// This is Tier 0 — a pure in-memory operation with no LLM call. Intentionally
    /// does NOT set `compacted_this_turn` so that proactive/reactive compaction may
    /// also fire in the same turn if tokens remain above their respective thresholds.
    /// Called from tool execution loops on every iteration to apply summaries eagerly.
    pub(in crate::agent) fn maybe_apply_deferred_summaries(&mut self) {
        let pending = self.count_deferred_summaries();
        if pending == 0 {
            return;
        }
        let token_pressure = matches!(
            self.compaction_tier(),
            CompactionTier::Soft | CompactionTier::Hard
        );
        let count_pressure = pending >= self.services.memory.persistence.tool_call_cutoff;
        if !token_pressure && !count_pressure {
            return;
        }
        let applied = self.apply_deferred_summaries();
        if applied > 0 {
            tracing::info!(
                applied,
                token_pressure,
                count_pressure,
                "tier-0: batch-applied deferred tool summaries"
            );
        }
    }
}
