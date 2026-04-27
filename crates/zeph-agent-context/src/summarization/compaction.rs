// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! LLM-based context compaction engine.
//!
//! Provides [`compact_context`] — the core summarization pass that drains the oldest
//! messages, invokes the LLM via [`zeph_context::summarization`] helpers, and
//! re-inserts a summary message. Focus-pinned and active-subgoal messages survive
//! compaction without being sent to the LLM.
//!
//! The caller (scheduling module) is responsible for deciding *when* to compact.
//! This module only handles the mechanics: partition → summarize → drain → reinsert.

use zeph_context::slot::cap_summary;
use zeph_llm::provider::{Message, MessageMetadata, Role};

use crate::compaction::SubgoalState;
use crate::error::ContextError;
use crate::state::ContextSummarizationView;

/// Compact the context window using LLM summarization.
///
/// Applies pending deferred summaries first (`CRIT-01`), then drains
/// `messages[1..compact_end]`, summarizes the non-pinned messages via the LLM,
/// and reinserts the summary at position 1. Focus-pinned and active-subgoal messages
/// survive compaction and are reinserted after the summary.
///
/// Returns the number of compacted messages, or `0` when there was nothing to compact.
///
/// # Errors
///
/// Returns [`ContextError::Memory`] when the `SQLite` persist call fails.
pub(crate) async fn compact_context(
    summ: &mut ContextSummarizationView<'_>,
    max_summary_tokens: Option<usize>,
) -> Result<usize, ContextError> {
    use super::deferred::apply_deferred_summaries;

    // CRIT-01: force-apply pending deferred summaries before draining.
    let _ = apply_deferred_summaries(summ);

    let preserve_tail = summ.context_manager.compaction_preserve_tail;

    if summ.messages.len() <= preserve_tail + 1 {
        return Ok(0);
    }

    let compact_end = {
        let raw = summ.messages.len() - preserve_tail;
        adjust_compact_end_for_tool_pairs(summ.messages, raw)
    };

    if compact_end <= 1 {
        return Ok(0);
    }

    let (pinned, active_subgoal, to_compact) = partition_messages_for_compaction(summ, compact_end);

    if to_compact.is_empty() {
        return Ok(0);
    }

    let summary = summarize_messages(summ, &to_compact, max_summary_tokens).await?;

    let compacted_count = to_compact.len();

    let summary_content =
        format!("[conversation summary — {compacted_count} messages compacted]\n{summary}");

    finalize_compacted_messages(
        summ,
        compact_end,
        pinned,
        active_subgoal,
        summary_content.clone(),
        compacted_count,
        &summary,
    );

    // Persist to SQLite; non-fatal — log and continue.
    if let (Some(memory), Some(cid)) = (summ.memory.as_deref(), summ.conversation_id) {
        let sqlite = memory.sqlite().clone();
        let ids = sqlite
            .oldest_message_ids(cid, u32::try_from(compacted_count + 1).unwrap_or(u32::MAX))
            .await;
        match ids {
            Ok(ids) if ids.len() >= 2 => {
                let start = ids[1];
                let end = ids[compacted_count.min(ids.len() - 1)];
                if let Err(e) = sqlite
                    .replace_conversation(cid, start..=end, "system", &summary_content)
                    .await
                {
                    tracing::warn!("failed to persist compaction in sqlite: {e:#}");
                }
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!("failed to get message ids for compaction: {e:#}");
            }
        }
    }

    Ok(compacted_count)
}

// ── Private helpers ───────────────────────────────────────────────────────────

/// Partition `messages[1..compact_end]` into pinned, active-subgoal, and to-compact slices.
fn partition_messages_for_compaction(
    summ: &ContextSummarizationView<'_>,
    compact_end: usize,
) -> (Vec<Message>, Vec<Message>, Vec<Message>) {
    let pinned: Vec<Message> = summ.messages[1..compact_end]
        .iter()
        .filter(|m| m.metadata.focus_pinned)
        .cloned()
        .collect();

    let is_subgoal = summ
        .context_manager
        .compression
        .pruning_strategy
        .is_subgoal();

    let active_subgoal: Vec<Message> = if is_subgoal {
        summ.messages[1..compact_end]
            .iter()
            .enumerate()
            .filter(|(slice_i, m)| {
                let actual_i = slice_i + 1;
                !m.metadata.focus_pinned
                    && matches!(
                        summ.subgoal_registry.subgoal_state(actual_i),
                        Some(SubgoalState::Active)
                    )
            })
            .map(|(_, m)| m.clone())
            .collect()
    } else {
        vec![]
    };

    let to_compact: Vec<Message> = if is_subgoal {
        summ.messages[1..compact_end]
            .iter()
            .enumerate()
            .filter(|(slice_i, m)| {
                let actual_i = slice_i + 1;
                !m.metadata.focus_pinned
                    && !matches!(
                        summ.subgoal_registry.subgoal_state(actual_i),
                        Some(SubgoalState::Active)
                    )
            })
            .map(|(_, m)| m.clone())
            .collect()
    } else {
        summ.messages[1..compact_end]
            .iter()
            .filter(|m| !m.metadata.focus_pinned)
            .cloned()
            .collect()
    };

    (pinned, active_subgoal, to_compact)
}

/// Drain the compaction range and reinsert the summary plus protected messages.
fn finalize_compacted_messages(
    summ: &mut ContextSummarizationView<'_>,
    compact_end: usize,
    pinned: Vec<Message>,
    active_subgoal: Vec<Message>,
    summary_content: String,
    compacted_count: usize,
    summary: &str,
) {
    summ.messages.drain(1..compact_end);

    summ.messages.insert(
        1,
        Message {
            role: Role::System,
            content: summary_content,
            parts: vec![],
            metadata: MessageMetadata::agent_only(),
        },
    );

    let pinned_count = pinned.len();
    for (i, pinned_msg) in pinned.into_iter().enumerate() {
        summ.messages.insert(2 + i, pinned_msg);
    }

    for (i, active_msg) in active_subgoal.into_iter().enumerate() {
        summ.messages.insert(2 + pinned_count + i, active_msg);
    }

    // Rebuild subgoal index map after index invalidation from drain + reinsert.
    if summ
        .context_manager
        .compression
        .pruning_strategy
        .is_subgoal()
    {
        summ.subgoal_registry
            .rebuild_after_compaction(summ.messages, compact_end);
    }

    tracing::info!(
        compacted_count,
        summary_tokens = summ.token_counter.count_tokens(summary),
        "compacted context"
    );

    // Recompute cached token count after mutation.
    *summ.cached_prompt_tokens = summ
        .messages
        .iter()
        .map(|m| summ.token_counter.count_message_tokens(m) as u64)
        .sum();
}

/// Invoke the LLM summarization path via `SummarizationDeps`.
async fn summarize_messages(
    summ: &ContextSummarizationView<'_>,
    messages: &[Message],
    max_summary_tokens: Option<usize>,
) -> Result<String, ContextError> {
    let deps = &summ.summarization_deps;
    let guidelines = String::new(); // TODO(review): load compression_guidelines via ContextSummarizationView

    let raw = zeph_context::summarization::summarize_with_llm(deps, messages, &guidelines)
        .await
        .map_err(|e| ContextError::Memory(zeph_memory::MemoryError::Llm(e)))?;

    let cap = max_summary_tokens.unwrap_or(16_000).saturating_mul(4);
    Ok(cap_summary(raw, cap))
}

/// Adjust the compaction boundary to not split tool-use / tool-result pairs.
///
/// If `raw` lands on an assistant message that has a `ToolUse` part, walks backward
/// until the boundary sits on a non-tool-use message.
pub(crate) fn adjust_compact_end_for_tool_pairs(messages: &[Message], mut raw: usize) -> usize {
    use zeph_llm::provider::MessagePart;

    while raw > 1 {
        let msg = &messages[raw - 1];
        let is_tool_use = msg
            .parts
            .iter()
            .any(|p| matches!(p, MessagePart::ToolUse { .. }));
        if is_tool_use {
            raw -= 1;
        } else {
            break;
        }
    }
    raw
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeph_llm::provider::{Message, MessageMetadata, MessagePart, Role};

    fn make_msg(role: Role, content: &str) -> Message {
        Message {
            role,
            content: content.into(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        }
    }

    fn make_tool_use_msg() -> Message {
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

    #[test]
    fn adjust_compact_end_skips_tool_use() {
        let messages = vec![
            make_msg(Role::System, "system"),
            make_msg(Role::User, "hello"),
            make_tool_use_msg(),
        ];
        // raw = 3 would split at the ToolUse message — must walk back to 2.
        let adjusted = adjust_compact_end_for_tool_pairs(&messages, 3);
        assert_eq!(adjusted, 2);
    }

    #[test]
    fn adjust_compact_end_no_change_when_not_tool_use() {
        let messages = vec![
            make_msg(Role::System, "system"),
            make_msg(Role::User, "hello"),
            make_msg(Role::Assistant, "world"),
        ];
        let adjusted = adjust_compact_end_for_tool_pairs(&messages, 3);
        assert_eq!(adjusted, 3);
    }

    #[test]
    fn adjust_compact_end_stops_at_one() {
        let mut messages = vec![make_msg(Role::System, "system")];
        // Fill with tool-use messages so the loop must stop.
        for _ in 0..5 {
            messages.push(make_tool_use_msg());
        }
        let adjusted = adjust_compact_end_for_tool_pairs(&messages, 6);
        assert_eq!(adjusted, 1);
    }
}
