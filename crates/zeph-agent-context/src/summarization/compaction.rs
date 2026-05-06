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
//! This module handles: archive → partition → summarize → probe → drain → reinsert → persist.

use std::sync::Arc;

use zeph_context::slot::cap_summary;
use zeph_context::typed_page::{
    BatchAssertions, CompactedPageRecord, PageOrigin, PageType, TypedPage, TypedPagesState,
    classify_with_role, detect_schema_hint,
};
use zeph_llm::provider::{Message, MessageMetadata, MessagePart, Role};

use crate::compaction::SubgoalState;
use crate::error::ContextError;
use crate::state::{CompactionOutcome, ContextSummarizationView, ProbeOutcome};

/// Compact the context window using LLM summarization.
///
/// Pipeline:
/// 1. Apply pending deferred summaries (`CRIT-01`).
/// 2. Partition `messages[1..compact_end]` into pinned, active-subgoal, and to-compact.
/// 3. Call `summ.archive` to save tool output bodies before summarization (Memex #2432).
/// 4. Invoke the LLM via `summarize_messages`.
/// 5. Call `summ.probe` to validate the summary quality; abort on `HardFail`.
/// 6. Finalize: drain the range, reinsert summary + protected messages.
/// 7. Call `summ.persistence` to persist the result; bubble the Qdrant future.
///
/// Returns [`CompactionOutcome`]`::NoChange` when there is nothing to compact.
///
/// # Errors
///
/// Returns [`ContextError`] if LLM summarization fails.
#[allow(clippy::too_many_lines)]
pub(crate) async fn compact_context(
    summ: &mut ContextSummarizationView<'_>,
    max_summary_tokens: Option<usize>,
) -> Result<CompactionOutcome, ContextError> {
    use super::deferred::apply_deferred_summaries;

    // CRIT-01: force-apply pending deferred summaries before draining.
    let _ = apply_deferred_summaries(summ);

    let preserve_tail = summ.context_manager.compaction_preserve_tail;

    if summ.messages.len() <= preserve_tail + 1 {
        return Ok(CompactionOutcome::NoChange);
    }

    let compact_end = {
        let raw = summ.messages.len() - preserve_tail;
        adjust_compact_end_for_tool_pairs(summ.messages, raw)
    };

    if compact_end <= 1 {
        return Ok(CompactionOutcome::NoChange);
    }

    let (pinned, active_subgoal, mut to_compact) =
        partition_messages_for_compaction(summ, compact_end);

    if to_compact.is_empty() {
        return Ok(CompactionOutcome::NoChange);
    }

    // Step 2.5: classify to_compact messages into TypedPages.
    let typed_pages_state = summ.typed_pages.clone();
    let (typed_pages_vec, batch_assertions) = if let Some(ref state) = typed_pages_state {
        let _span = tracing::info_span!(
            "context.typed_page.classify_batch",
            message_count = to_compact.len()
        )
        .entered();
        classify_to_compact_batch(&to_compact, state)
    } else {
        (Vec::new(), BatchAssertions::default())
    };

    // Step 3: archive tool outputs before summarization (Memex #2432).
    // Extract the archive pointer before .await so no &summ crosses the await boundary.
    // References are appended as a postfix AFTER the LLM call so the LLM never sees them.
    let archived_refs: Vec<String> = if let Some(archive) = summ.archive.as_ref() {
        archive.archive(&to_compact).await
    } else {
        Vec::new()
    };

    // Step 3.5: in active enforcement mode, pointer-replace SystemContext pages.
    let is_active = typed_pages_state.as_ref().is_some_and(|s| s.is_active);
    if is_active {
        let span = tracing::info_span!(
            "context.typed_page.pointer_replace",
            replaced_count = tracing::field::Empty
        )
        .entered();
        pointer_replace_system_pages(&mut to_compact, &typed_pages_vec, &span);
    }

    // Step 4: LLM summarization.
    // In active mode, if every message in to_compact was pointer-replaced (all-SystemContext
    // batch), skip the LLM call: the stubs carry no semantic content the LLM can summarize,
    // and sending only `[system-ptr:…]` lines would produce a meaningless injected summary.
    let all_stubs = is_active
        && !typed_pages_vec.is_empty()
        && typed_pages_vec
            .iter()
            .all(|p| p.page_type == PageType::SystemContext);

    // Extract deps and guidelines from summ synchronously before .await so no reference to
    // summ (which contains !Sync fields) is held across the await boundary.
    let summary = if all_stubs {
        let n = typed_pages_vec.len();
        tracing::debug!(
            n,
            "all-SystemContext batch in active mode — skipping LLM, using synthetic summary"
        );
        format!("[system context — {n} blocks pointer-replaced]")
    } else {
        let deps = summ.summarization_deps.clone();
        let guidelines = summ
            .compression_guidelines
            .as_deref()
            .unwrap_or("")
            .to_owned();
        summarize_messages(deps, &to_compact, guidelines, max_summary_tokens).await?
    };

    // Step 5: probe validation (optional).
    if let Some(probe) = summ.probe.as_mut() {
        let outcome = probe.validate(&to_compact, &summary).await;
        if outcome == ProbeOutcome::HardFail {
            return Ok(CompactionOutcome::ProbeRejected);
        }
    }

    // Step 5.5: batch assertions (observational, never blocks compaction).
    if !typed_pages_vec.is_empty() {
        let span = tracing::info_span!(
            "context.typed_page.batch_assertions",
            tool_names_checked = batch_assertions.tool_names_in_batch.len(),
            violations = tracing::field::Empty
        )
        .entered();
        let violations = batch_assertions.check(&summary);
        if !violations.is_empty() {
            tracing::warn!(
                violation_count = violations.len(),
                ?violations,
                "typed-page batch assertions failed (observational, compaction proceeds)"
            );
            span.record("violations", violations.len());
        }
    }

    let compacted_count = to_compact.len();

    // Build archive postfix (injected after LLM summary to protect [archived:UUID] markers).
    let archive_postfix = if archived_refs.is_empty() {
        String::new()
    } else {
        let refs = archived_refs.join("\n");
        format!("\n\n[archived tool outputs — retrievable via read_overflow]\n{refs}")
    };

    let summary_content = format!(
        "[conversation summary — {compacted_count} messages compacted]\n{summary}{archive_postfix}"
    );

    // Step 6: finalize — drain + reinsert.
    // CONTRACT (S3): `finalize_compacted_messages` MUST update `*summ.cached_prompt_tokens`
    // before returning. Callers that read cached_prompt_tokens for delta computation
    // (e.g. `do_hard_compaction`'s freed-tokens calculation) rely on this update.
    finalize_compacted_messages(
        summ,
        compact_end,
        pinned,
        active_subgoal,
        summary_content.clone(),
        compacted_count,
        &summary,
    );

    // Step 7: persistence (optional).
    // Extract pointer before .await so no &summ crosses the await boundary.
    let (persist_failed, qdrant_future) = if let Some(persistence) = summ.persistence.as_ref() {
        persistence
            .after_compaction(compacted_count, &summary_content, &summary)
            .await
    } else {
        (false, None)
    };

    // Step 7.5: emit audit records for classified pages (non-blocking).
    if !typed_pages_vec.is_empty()
        && let Some(state) = typed_pages_state.as_ref()
        && let Some(ref sink) = state.audit_sink
    {
        let span = tracing::info_span!(
            "context.typed_page.audit_emit",
            records_sent = tracing::field::Empty,
            dropped = tracing::field::Empty
        )
        .entered();
        let dropped_before = sink.dropped_count();
        emit_audit_records(sink, &typed_pages_vec, &summary);
        let dropped_after = sink.dropped_count();
        span.record("records_sent", typed_pages_vec.len());
        span.record("dropped", dropped_after.saturating_sub(dropped_before));
    }

    if persist_failed {
        Ok(CompactionOutcome::CompactedWithPersistError { qdrant_future })
    } else {
        Ok(CompactionOutcome::Compacted { qdrant_future })
    }
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
///
/// Updates `*summ.cached_prompt_tokens` before returning (CONTRACT S3).
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

    // CONTRACT (S3): update cached token count after mutation so callers computing
    // freed-token deltas see the correct post-compaction value.
    *summ.cached_prompt_tokens = summ
        .messages
        .iter()
        .map(|m| summ.token_counter.count_message_tokens(m) as u64)
        .sum();
}

/// Invoke the LLM summarization path via `SummarizationDeps`.
///
/// Takes `deps` and `guidelines` by value (already extracted from `summ` by the caller)
/// so no reference to `ContextSummarizationView` (which contains `!Sync` fields) is held
/// across the `.await` boundary.
async fn summarize_messages(
    deps: zeph_context::summarization::SummarizationDeps,
    messages: &[Message],
    guidelines: String,
    max_summary_tokens: Option<usize>,
) -> Result<String, ContextError> {
    let cap = max_summary_tokens.unwrap_or(16_000).saturating_mul(4);

    let raw = zeph_context::summarization::summarize_with_llm(&deps, messages, &guidelines)
        .await
        .map_err(|e| ContextError::Memory(zeph_memory::MemoryError::Llm(e)))?;

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

// ── Typed-page helpers ────────────────────────────────────────────────────────

/// Classify all messages in `to_compact` and build `BatchAssertions`.
///
/// Returns `(Vec<TypedPage>, BatchAssertions)`. Index `i` in the returned vec
/// corresponds to `to_compact[i]`.
fn classify_to_compact_batch(
    to_compact: &[Message],
    _state: &TypedPagesState,
) -> (Vec<TypedPage>, BatchAssertions) {
    let mut pages = Vec::with_capacity(to_compact.len());
    let mut tool_names: Vec<String> = Vec::new();
    let mut excerpt_labels: Vec<String> = Vec::new();
    let mut has_memory_excerpt = false;

    for (i, msg) in to_compact.iter().enumerate() {
        let is_system = matches!(msg.role, Role::System);
        let body = msg.content.as_str();
        let page_type = classify_with_role(body, is_system);

        let origin = derive_origin(msg, i, page_type);
        let schema_hint = if page_type == PageType::ToolOutput {
            Some(detect_schema_hint(body, false))
        } else {
            None
        };
        let tokens = u32::try_from((body.len() / 4).min(u32::MAX as usize)).unwrap_or(u32::MAX);

        // Collect batch assertion inputs.
        match page_type {
            PageType::ToolOutput => {
                if let PageOrigin::ToolPair { ref tool_name } = origin
                    && !tool_name.is_empty()
                {
                    tool_names.push(tool_name.clone());
                }
            }
            PageType::MemoryExcerpt => {
                has_memory_excerpt = true;
                if let PageOrigin::Excerpt { ref source_label } = origin
                    && !source_label.is_empty()
                {
                    excerpt_labels.push(source_label.clone());
                }
            }
            PageType::SystemContext | PageType::ConversationTurn => {}
        }

        pages.push(TypedPage::new(
            page_type,
            origin,
            tokens,
            Arc::from(body),
            schema_hint,
        ));
    }

    let assertions = BatchAssertions {
        tool_names_in_batch: tool_names,
        has_memory_excerpt,
        excerpt_labels,
    };
    (pages, assertions)
}

/// Derive [`PageOrigin`] for a message given its pre-computed [`PageType`].
///
/// For `ToolOutput`, prefers `MessagePart::ToolOutput.tool_name` (authoritative)
/// over content-prefix heuristics.
fn derive_origin(msg: &Message, index: usize, page_type: PageType) -> PageOrigin {
    match page_type {
        PageType::ToolOutput => {
            // Authoritative: structured part carries the tool name.
            let tool_name = extract_tool_name_from_parts(&msg.parts)
                // Heuristic fallback: content prefix.
                .or_else(|| extract_tool_name_from_content(&msg.content))
                .unwrap_or_default();
            PageOrigin::ToolPair { tool_name }
        }
        PageType::MemoryExcerpt => {
            let source_label = extract_source_label_from_content(&msg.content);
            PageOrigin::Excerpt { source_label }
        }
        PageType::SystemContext => {
            let key = extract_system_key_from_content(&msg.content)
                .unwrap_or_else(|| format!("msg_{index}"));
            PageOrigin::System { key }
        }
        PageType::ConversationTurn => PageOrigin::Turn {
            message_id: index.to_string(),
        },
    }
}

/// Extract tool name from `MessagePart::ToolOutput` (primary, authoritative).
fn extract_tool_name_from_parts(parts: &[MessagePart]) -> Option<String> {
    for part in parts {
        match part {
            MessagePart::ToolOutput { tool_name, .. } => {
                return Some(tool_name.to_string());
            }
            MessagePart::ToolUse { name, .. } => {
                return Some(name.clone());
            }
            _ => {}
        }
    }
    None
}

/// Extract tool name from content prefix (heuristic fallback).
///
/// Recognises `[tool:name]` and `[tool_output] <name>` patterns.
/// Returns `None` if content does not match or first word looks like metadata
/// (e.g. `exit_code`, `exit_status`, `status`).
fn extract_tool_name_from_content(content: &str) -> Option<String> {
    // Avoid misclassifying raw output fields as tool names.
    const METADATA_WORDS: &[&str] = &["exit_code", "exit_status", "exit:", "status:", "rc:"];

    let trimmed = content.trim_start();
    if let Some(rest) = trimmed.strip_prefix("[tool:")
        && let Some(end) = rest.find(']')
    {
        return Some(rest[..end].to_string());
    }
    if let Some(rest) = trimmed.strip_prefix("[tool_output] ") {
        let first_word = rest.split_whitespace().next().unwrap_or("");
        let looks_like_metadata = METADATA_WORDS
            .iter()
            .any(|m| first_word == *m || first_word.starts_with(m.trim_end_matches(':')));
        if !first_word.is_empty() && !looks_like_metadata {
            return Some(first_word.to_string());
        }
    }
    None
}

/// Extract source label from memory-excerpt content prefix.
fn extract_source_label_from_content(content: &str) -> String {
    let trimmed = content.trim_start();
    if trimmed.starts_with("[cross-session context]") {
        return "cross_session".into();
    }
    if trimmed.starts_with("[semantic recall]") {
        return "semantic_recall".into();
    }
    if trimmed.starts_with("[known facts]") {
        return "graph_facts".into();
    }
    if trimmed.starts_with("[conversation summaries]") {
        return "summaries".into();
    }
    if trimmed.starts_with("[past corrections]") {
        return "corrections".into();
    }
    if trimmed.starts_with("## Relevant documents") {
        return "document_rag".into();
    }
    "unknown".into()
}

/// Extract a logical key from a system-context content prefix.
fn extract_system_key_from_content(content: &str) -> Option<String> {
    const KNOWN: &[(&str, &str)] = &[
        ("[Persona context]", "persona"),
        ("[Past experience]", "past_experience"),
        ("[Memory summary]", "memory_summary"),
        ("[system", "system"),
        ("[skill", "skill"),
        ("[persona", "persona"),
        ("[digest", "digest"),
        ("[compression", "compression"),
    ];
    let trimmed = content.trim_start();
    for (prefix, key) in KNOWN {
        if trimmed.starts_with(prefix) {
            return Some((*key).to_string());
        }
    }
    None
}

/// In `active` enforcement mode, replace `SystemContext` pages in `to_compact` with pointer stubs.
///
/// The original message content is replaced with `[system-ptr:{page_id}]` so the LLM never
/// paraphrases system instructions.
fn pointer_replace_system_pages(
    to_compact: &mut [Message],
    typed_pages: &[TypedPage],
    span: &tracing::span::EnteredSpan,
) {
    use zeph_context::typed_page::SYSTEM_POINTER_PREFIX;

    let mut replaced = 0usize;
    for (msg, page) in to_compact.iter_mut().zip(typed_pages.iter()) {
        if page.page_type == PageType::SystemContext {
            msg.content = format!("{SYSTEM_POINTER_PREFIX}{}]", page.page_id.0);
            replaced += 1;
        }
    }
    span.record("replaced_count", replaced);
    if replaced > 0 {
        tracing::debug!(
            replaced,
            "pointer-replaced SystemContext pages before LLM compaction"
        );
    }
}

/// Emit audit records for all classified pages to the sink (non-blocking `try_send`).
fn emit_audit_records(
    sink: &zeph_context::typed_page::CompactionAuditSink,
    typed_pages: &[TypedPage],
    summary: &str,
) {
    let ts = chrono::Utc::now().to_rfc3339();
    let turn_id = "batch".to_string();
    let compacted_tokens =
        u32::try_from((summary.len() / 4).min(u32::MAX as usize)).unwrap_or(u32::MAX);

    for page in typed_pages {
        let record = CompactedPageRecord {
            ts: ts.clone(),
            turn_id: turn_id.clone(),
            page_id: page.page_id.0.clone(),
            page_type: page.page_type,
            origin: page.origin.clone(),
            original_tokens: page.tokens,
            compacted_tokens,
            fidelity_level: "batch_summary_v1".to_string(),
            invariant_version: 1,
            provider_name: "batch".to_string(),
            violations: vec![],
            classification_fallback: matches!(page.page_type, PageType::ConversationTurn)
                && !page.body.starts_with('['),
        };
        sink.send(record);
    }
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
