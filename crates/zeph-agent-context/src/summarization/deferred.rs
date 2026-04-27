// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Deferred tool-pair summarization — queues LLM summaries and applies them lazily.
//!
//! Deferred summaries preserve the message prefix for provider cache hits: the summary is
//! stored on `MessageMetadata::deferred_summary` and only applied to the message list when
//! context pressure rises (`maybe_apply_deferred_summaries`) or is explicitly flushed.

use std::cmp::Reverse;

use zeph_llm::provider::{LlmProvider as _, Message, MessageMetadata, MessagePart, Role};

use crate::error::ContextError;
use crate::state::{ContextSummarizationView, ProviderHandles, StatusSink};
use zeph_context::manager::CompactionTier;

/// Count tool-use/result pairs that have not yet been summarized.
///
/// Iterates `messages` from index 1 (skipping the system prompt) and counts consecutive
/// assistant-ToolUse + user-ToolResult/ToolOutput pairs where the response has no
/// `deferred_summary` set and is still agent-visible.
#[must_use]
pub fn count_unsummarized_pairs(messages: &[Message]) -> usize {
    let mut count = 0usize;
    let mut i = 1; // skip system prompt
    while i < messages.len() {
        let msg = &messages[i];
        if !msg.metadata.visibility.is_agent_visible() {
            i += 1;
            continue;
        }
        let is_tool_request = msg.role == Role::Assistant
            && msg
                .parts
                .iter()
                .any(|p| matches!(p, MessagePart::ToolUse { .. }));
        if is_tool_request && i + 1 < messages.len() {
            let next = &messages[i + 1];
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

/// Find the index of the oldest unsummarized tool-use/result pair.
///
/// Skips pairs where the response already has a `deferred_summary` set, or where
/// all tool output bodies are empty/pruned (summarizing `[pruned]` is a no-op).
///
/// Returns `Some((req_idx, resp_idx))` or `None` if no eligible pair exists.
#[must_use]
pub fn find_oldest_unsummarized_pair(messages: &[Message]) -> Option<(usize, usize)> {
    let mut i = 1; // skip system prompt
    while i < messages.len() {
        let msg = &messages[i];
        if !msg.metadata.visibility.is_agent_visible() {
            i += 1;
            continue;
        }
        let is_tool_request = msg.role == Role::Assistant
            && msg
                .parts
                .iter()
                .any(|p| matches!(p, MessagePart::ToolUse { .. }));
        if is_tool_request && i + 1 < messages.len() {
            let next = &messages[i + 1];
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
                // Skip pairs whose response content has been fully pruned (IMP-03).
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

/// Count messages that carry a pending `deferred_summary`.
#[must_use]
pub fn count_deferred_summaries(messages: &[Message]) -> usize {
    messages
        .iter()
        .filter(|m| m.metadata.deferred_summary.is_some())
        .count()
}

/// Summarize the most recent tool-use/result pair if the unsummarized count exceeds `cutoff`.
///
/// Drains the entire backlog in one pass so that a resumed session with many accumulated
/// pairs catches up before Tier 1 pruning fires. Summaries are stored as `deferred_summary`
/// on the response metadata — they are not immediately applied to the message list.
///
/// # Errors
///
/// This function is infallible — LLM errors and timeouts are logged and the batch stops.
pub(crate) async fn maybe_summarize_tool_pair(
    summ: &mut ContextSummarizationView<'_>,
    providers: &ProviderHandles,
    status: &(impl StatusSink + ?Sized),
) {
    let cutoff = summ.tool_call_cutoff;
    let llm_timeout = summ.summarization_deps.llm_timeout;
    let mut summarized = 0usize;
    loop {
        let pair_count = count_unsummarized_pairs(summ.messages);
        if pair_count <= cutoff {
            break;
        }
        let Some((req_idx, resp_idx)) = find_oldest_unsummarized_pair(summ.messages) else {
            break;
        };
        let prompt = zeph_context::summarization::build_tool_pair_summary_prompt(
            &summ.messages[req_idx],
            &summ.messages[resp_idx],
        );
        let msgs = [Message {
            role: Role::User,
            content: prompt,
            parts: vec![],
            metadata: MessageMetadata::default(),
        }];
        status.send_status("summarizing output...").await;
        let chat_fut = providers.primary.chat(&msgs);
        let summary = match tokio::time::timeout(llm_timeout, chat_fut).await {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                tracing::warn!(%e, "tool pair summarization failed, stopping batch");
                status.send_status("").await;
                break;
            }
            Err(_elapsed) => {
                tracing::warn!(
                    timeout_secs = llm_timeout.as_secs(),
                    "tool pair summarization timed out, stopping batch"
                );
                status.send_status("").await;
                break;
            }
        };
        // DEFERRED: store summary on response metadata — applied lazily by apply_deferred_summaries.
        let summary = zeph_context::slot::cap_summary((summ.scrub)(&summary).into_owned(), 8_000);
        summ.messages[resp_idx].metadata.deferred_summary = Some(summary.clone());
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
    status.send_status("").await;
    if summarized > 0 {
        tracing::info!(summarized, "batch-summarized tool pairs above cutoff");
    }
}

/// Batch-apply all pending deferred tool pair summaries.
///
/// Processes in reverse index order (highest first) so that inserting a summary message
/// at `resp_idx + 1` does not shift the indices of not-yet-processed pairs. Returns the
/// number of summaries applied.
pub(crate) fn apply_deferred_summaries(summ: &mut ContextSummarizationView<'_>) -> usize {
    // Phase 1: collect (resp_idx, req_idx, summary) for all messages with deferred_summary.
    let mut targets: Vec<(usize, usize, String)> = Vec::new();
    for i in 1..summ.messages.len() {
        if summ.messages[i].metadata.deferred_summary.is_none() {
            continue;
        }
        // Verify the structural invariant: tool response preceded by matching tool request.
        if summ.messages[i].role == Role::User
            && summ.messages[i].metadata.visibility.is_agent_visible()
            && i > 0
            && summ.messages[i - 1].role == Role::Assistant
            && summ.messages[i - 1].metadata.visibility.is_agent_visible()
            && summ.messages[i - 1]
                .parts
                .iter()
                .any(|p| matches!(p, MessagePart::ToolUse { .. }))
        {
            let summary = summ.messages[i]
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
    targets.sort_by_key(|target| Reverse(target.0));

    let count = targets.len();
    for (resp_idx, req_idx, summary) in targets {
        let req_db_id = summ.messages[req_idx].metadata.db_id;
        let resp_db_id = summ.messages[resp_idx].metadata.db_id;

        summ.messages[req_idx].metadata.visibility =
            zeph_llm::provider::MessageVisibility::UserOnly;
        summ.messages[resp_idx].metadata.visibility =
            zeph_llm::provider::MessageVisibility::UserOnly;
        summ.messages[resp_idx].metadata.deferred_summary = None;

        if let (Some(req_id), Some(resp_id)) = (req_db_id, resp_db_id) {
            summ.deferred_db_hide_ids.push(req_id);
            summ.deferred_db_hide_ids.push(resp_id);
            summ.deferred_db_summaries.push(summary.clone());
        }

        let content = format!("[tool summary] {summary}");
        let summary_msg = Message {
            role: Role::Assistant,
            content,
            parts: vec![MessagePart::Summary { text: summary }],
            metadata: MessageMetadata::agent_only(),
        };
        summ.messages.insert(resp_idx + 1, summary_msg);
    }

    // Recompute token count after message list mutation.
    *summ.cached_prompt_tokens = summ
        .messages
        .iter()
        .map(|m| summ.token_counter.count_message_tokens(m) as u64)
        .sum();

    tracing::info!(count, "applied deferred tool pair summaries");
    count
}

/// Flush all deferred summary IDs to the database.
///
/// Calls `apply_tool_pair_summaries` to soft-delete the original tool pairs and persist
/// the summaries. Clears both deferred queues on success or error.
///
/// # Errors
///
/// Returns `ContextError::Memory` if the `SQLite` call fails. Both deferred queues are
/// always cleared regardless of the outcome so the agent can continue.
pub(crate) async fn flush_deferred_summaries(
    summ: &mut ContextSummarizationView<'_>,
) -> Result<(), ContextError> {
    if summ.deferred_db_hide_ids.is_empty() {
        return Ok(());
    }
    let (Some(memory), Some(cid)) = (summ.memory.as_deref(), summ.conversation_id) else {
        summ.deferred_db_hide_ids.clear();
        summ.deferred_db_summaries.clear();
        return Ok(());
    };
    let hide_ids = std::mem::take(summ.deferred_db_hide_ids);
    let summaries = std::mem::take(summ.deferred_db_summaries);
    if let Err(e) = memory
        .sqlite()
        .apply_tool_pair_summaries(cid, &hide_ids, &summaries)
        .await
    {
        tracing::warn!(error = %e, "failed to flush deferred summary batch to DB");
    }
    Ok(())
}

/// Apply deferred summaries if context usage exceeds the soft compaction threshold,
/// or when enough summaries have accumulated to prevent content loss from pruning.
///
/// Two triggers:
/// - Token pressure: `cached_prompt_tokens > budget * soft_compaction_threshold`
/// - Count pressure: `pending >= tool_call_cutoff`
///
/// This is Tier 0 — a pure in-memory operation with no LLM call. Does NOT set
/// `compacted_this_turn` so that proactive/reactive compaction may also fire
/// in the same turn if tokens remain above their respective thresholds.
pub(crate) fn maybe_apply_deferred_summaries(summ: &mut ContextSummarizationView<'_>) {
    let pending = count_deferred_summaries(summ.messages);
    if pending == 0 {
        return;
    }
    let token_pressure = matches!(
        summ.context_manager
            .compaction_tier(*summ.cached_prompt_tokens),
        CompactionTier::Soft | CompactionTier::Hard
    );
    let count_pressure = pending >= summ.tool_call_cutoff;
    if !token_pressure && !count_pressure {
        return;
    }
    let applied = apply_deferred_summaries(summ);
    if applied > 0 {
        tracing::info!(
            applied,
            token_pressure,
            count_pressure,
            "tier-0: batch-applied deferred tool summaries"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeph_llm::provider::{Message, MessageMetadata, MessagePart, Role};

    fn tool_use_msg() -> Message {
        Message {
            role: Role::Assistant,
            content: String::new(),
            parts: vec![MessagePart::ToolUse {
                id: "t1".into(),
                name: "shell".into(),
                input: serde_json::json!({}),
            }],
            metadata: MessageMetadata::default(),
        }
    }

    fn tool_result_msg() -> Message {
        Message {
            role: Role::User,
            content: "output".into(),
            parts: vec![MessagePart::ToolResult {
                tool_use_id: "t1".into(),
                content: "output".into(),
                is_error: false,
            }],
            metadata: MessageMetadata::default(),
        }
    }

    fn sys() -> Message {
        Message::from_legacy(Role::System, "system")
    }

    fn user(s: &str) -> Message {
        Message::from_legacy(Role::User, s)
    }

    fn assistant(s: &str) -> Message {
        Message::from_legacy(Role::Assistant, s)
    }

    #[test]
    fn count_unsummarized_pairs_empty_returns_zero() {
        let msgs = vec![sys()];
        assert_eq!(count_unsummarized_pairs(&msgs), 0);
    }

    #[test]
    fn count_unsummarized_pairs_counts_tool_pairs() {
        let msgs = vec![sys(), tool_use_msg(), tool_result_msg()];
        assert_eq!(count_unsummarized_pairs(&msgs), 1);
    }

    #[test]
    fn count_unsummarized_pairs_skips_already_summarized() {
        let mut resp = tool_result_msg();
        resp.metadata.deferred_summary = Some("summary".into());
        let msgs = vec![sys(), tool_use_msg(), resp];
        assert_eq!(count_unsummarized_pairs(&msgs), 0);
    }

    #[test]
    fn find_oldest_unsummarized_pair_returns_indices() {
        let msgs = vec![sys(), tool_use_msg(), tool_result_msg()];
        let result = find_oldest_unsummarized_pair(&msgs);
        assert_eq!(result, Some((1, 2)));
    }

    #[test]
    fn find_oldest_unsummarized_pair_skips_empty_pruned() {
        let mut resp = tool_result_msg();
        // Mark as pruned
        resp.parts = vec![MessagePart::ToolResult {
            tool_use_id: "t1".into(),
            content: "[pruned]".into(),
            is_error: false,
        }];
        let msgs = vec![sys(), tool_use_msg(), resp];
        assert_eq!(find_oldest_unsummarized_pair(&msgs), None);
    }

    #[test]
    fn count_deferred_summaries_returns_correct_count() {
        let mut msg = user("hello");
        msg.metadata.deferred_summary = Some("sum".into());
        let msgs = vec![sys(), msg, assistant("reply")];
        assert_eq!(count_deferred_summaries(&msgs), 1);
    }
}
