// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Pure prompt-building, compaction helpers, and async LLM summarization for context.
//!
//! Stateless functions take only `Message` slices and configuration values; they contain
//! no agent state access. The `SummarizationDeps` struct provides explicit LLM dependencies
//! for the async summarization functions, avoiding coupling to `Agent<C>`.
//!
//! The orchestration layer (`Agent::compact_context`, `Agent::maybe_compact`, etc.)
//! lives in `zeph-core` and calls these helpers.

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt as _;
use zeph_common::OVERFLOW_NOTICE_PREFIX;
use zeph_llm::LlmProvider as _;
use zeph_llm::any::AnyProvider;
use zeph_llm::provider::{Message, MessageMetadata, MessagePart, Role};
use zeph_memory::{AnchoredSummary, TokenCounter};

/// Explicit LLM dependencies for async summarization, avoiding coupling to `Agent<C>`.
///
/// Passed to [`single_pass_summary`], [`summarize_with_llm`], and [`summarize_structured`]
/// so these functions can be called from `zeph-context` without depending on `zeph-core`.
#[derive(Clone)]
pub struct SummarizationDeps {
    /// LLM provider used for all summarization calls.
    pub provider: AnyProvider,
    /// Timeout applied to each individual LLM call.
    pub llm_timeout: Duration,
    /// Token counter for chunking message slices.
    pub token_counter: Arc<TokenCounter>,
    /// Whether to attempt structured `AnchoredSummary` output before prose.
    pub structured_summaries: bool,
    /// Optional callback invoked with the `AnchoredSummary` result and a `fallback` flag.
    ///
    /// Used by `zeph-core` to write debug dumps without the `SummarizationDeps` knowing
    /// about `DebugDumper`. Pass `None` when debug dumps are not needed.
    #[allow(clippy::type_complexity)]
    pub on_anchored_summary: Option<Arc<dyn Fn(&AnchoredSummary, bool) + Send + Sync>>,
}

/// Attempt structured summarization via `chat_typed_erased::<AnchoredSummary>()`.
///
/// Returns `Ok(AnchoredSummary)` on success, `Err` when mandatory fields are missing
/// or the LLM fails. The caller is responsible for falling back to prose on `Err`.
///
/// # Errors
/// Returns [`zeph_llm::LlmError`] when the LLM call fails, times out, or the
/// returned summary is incomplete.
pub async fn summarize_structured(
    deps: &SummarizationDeps,
    messages: &[Message],
    guidelines: &str,
) -> Result<AnchoredSummary, zeph_llm::LlmError> {
    let prompt = build_anchored_summary_prompt(messages, guidelines);
    let msgs = [Message {
        role: Role::User,
        content: prompt,
        parts: vec![],
        metadata: MessageMetadata::default(),
    }];
    let summary: AnchoredSummary = tokio::time::timeout(
        deps.llm_timeout,
        deps.provider.chat_typed_erased::<AnchoredSummary>(&msgs),
    )
    .await
    .map_err(|_| zeph_llm::LlmError::Timeout)??;

    if !summary.files_modified.is_empty() && summary.decisions_made.is_empty() {
        tracing::warn!("structured summary: decisions_made is empty");
    } else if summary.files_modified.is_empty() {
        tracing::warn!(
            "structured summary: files_modified is empty (may be a pure discussion session)"
        );
    }

    if !summary.is_complete() {
        tracing::warn!(
            session_intent_empty = summary.session_intent.trim().is_empty(),
            next_steps_empty = summary.next_steps.is_empty(),
            "structured summary incomplete: mandatory fields missing, falling back to prose"
        );
        return Err(zeph_llm::LlmError::Other(
            "structured summary missing mandatory fields".into(),
        ));
    }

    if let Err(msg) = summary.validate() {
        tracing::warn!(
            error = %msg,
            "structured summary failed field validation, falling back to prose"
        );
        return Err(zeph_llm::LlmError::Other(msg));
    }

    Ok(summary)
}

/// Single-pass LLM summarization over a message slice.
///
/// # Errors
/// Returns [`zeph_llm::LlmError`] when the LLM call fails or times out.
pub async fn single_pass_summary(
    deps: &SummarizationDeps,
    messages: &[Message],
    guidelines: &str,
) -> Result<String, zeph_llm::LlmError> {
    let prompt = build_chunk_prompt(messages, guidelines);
    let msgs = [Message {
        role: Role::User,
        content: prompt,
        parts: vec![],
        metadata: MessageMetadata::default(),
    }];
    tokio::time::timeout(deps.llm_timeout, deps.provider.chat(&msgs))
        .await
        .map_err(|_| zeph_llm::LlmError::Timeout)?
}

/// Chunked multi-pass LLM summarization with bounded concurrency.
///
/// Splits `messages` into token-bounded chunks and summarizes each chunk with the LLM.
/// Partial results are consolidated into a final summary. Falls back to single-pass on
/// chunk failures or context length errors.
///
/// # Errors
/// Returns [`zeph_llm::LlmError`] when all summarization attempts fail.
#[allow(clippy::too_many_lines)]
pub async fn summarize_with_llm(
    deps: &SummarizationDeps,
    messages: &[Message],
    guidelines: &str,
) -> Result<String, zeph_llm::LlmError> {
    const CHUNK_TOKEN_BUDGET: usize = 4096;
    const OVERSIZED_THRESHOLD: usize = CHUNK_TOKEN_BUDGET / 2;

    let chunks = crate::slot::chunk_messages(
        messages,
        CHUNK_TOKEN_BUDGET,
        OVERSIZED_THRESHOLD,
        &deps.token_counter,
    );

    if chunks.len() <= 1 {
        return single_pass_summary(deps, messages, guidelines).await;
    }

    // Summarize chunks with bounded concurrency to prevent runaway API calls
    let provider = deps.provider.clone();
    let guidelines_owned = guidelines.to_string();
    let timeout = deps.llm_timeout;
    let results: Vec<_> = futures::stream::iter(chunks.into_iter().map(|chunk| {
        let guidelines_ref = guidelines_owned.clone();
        let prompt = build_chunk_prompt(&chunk, &guidelines_ref);
        let p = provider.clone();
        async move {
            tokio::time::timeout(
                timeout,
                p.chat(&[Message {
                    role: Role::User,
                    content: prompt,
                    parts: vec![],
                    metadata: MessageMetadata::default(),
                }]),
            )
            .await
            .map_err(|_| zeph_llm::LlmError::Timeout)?
        }
    }))
    .buffer_unordered(4)
    .collect()
    .await;

    let partial_summaries: Vec<String> = results
        .into_iter()
        .collect::<Result<Vec<_>, zeph_llm::LlmError>>()
        .unwrap_or_else(|e| {
            tracing::warn!(
                "chunked compaction: one or more chunks failed: {e:#}, falling back to single-pass"
            );
            Vec::new()
        });

    if partial_summaries.is_empty() {
        return single_pass_summary(deps, messages, guidelines).await;
    }

    // Consolidate partial summaries
    let numbered = {
        let cap: usize = partial_summaries.iter().map(|s| s.len() + 8).sum();
        let mut buf = String::with_capacity(cap);
        for (i, s) in partial_summaries.iter().enumerate() {
            if i > 0 {
                buf.push_str("\n\n");
            }
            let _ = write!(buf, "{}. {s}", i + 1);
        }
        buf
    };

    if deps.structured_summaries {
        let anchored_prompt = format!(
            "<analysis>\n\
             Merge these partial conversation summaries into a single structured summary.\n\
             </analysis>\n\
             \n\
             Produce a JSON object with exactly these 5 fields:\n\
             - session_intent: string — what the user is trying to accomplish\n\
             - files_modified: string[] — file paths, function names, structs touched\n\
             - decisions_made: string[] — each entry: \"Decision: X — Reason: Y\"\n\
             - open_questions: string[] — unresolved questions or blockers\n\
             - next_steps: string[] — concrete next actions\n\
             \n\
             Partial summaries:\n{numbered}"
        );
        let anchored_msgs = [Message {
            role: Role::User,
            content: anchored_prompt,
            parts: vec![],
            metadata: MessageMetadata::default(),
        }];
        match tokio::time::timeout(
            timeout,
            deps.provider
                .chat_typed_erased::<AnchoredSummary>(&anchored_msgs),
        )
        .await
        {
            Ok(Ok(anchored)) if anchored.is_complete() => {
                if let Some(ref cb) = deps.on_anchored_summary {
                    cb(&anchored, false);
                }
                return Ok(crate::slot::cap_summary(anchored.to_markdown(), 16_000));
            }
            Ok(Ok(anchored)) => {
                tracing::warn!(
                    "chunked consolidation: structured summary incomplete, falling back to prose"
                );
                if let Some(ref cb) = deps.on_anchored_summary {
                    cb(&anchored, true);
                }
            }
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "chunked consolidation: structured output failed, falling back to prose");
            }
            Err(_) => {
                tracing::warn!(
                    "chunked consolidation: structured output timed out, falling back to prose"
                );
            }
        }
    }

    let consolidation_prompt = format!(
        "<analysis>\n\
         Merge these partial conversation summaries into a single structured compaction note.\n\
         Produce exactly these 9 sections covering all partial summaries:\n\
         1. User Intent\n2. Technical Concepts\n3. Files & Code\n4. Errors & Fixes\n\
         5. Problem Solving\n6. User Messages\n7. Pending Tasks\n8. Current Work\n9. Next Step\n\
         </analysis>\n\n\
         Partial summaries:\n{numbered}"
    );

    let consolidation_msgs = [Message {
        role: Role::User,
        content: consolidation_prompt,
        parts: vec![],
        metadata: MessageMetadata::default(),
    }];
    tokio::time::timeout(timeout, deps.provider.chat(&consolidation_msgs))
        .await
        .map_err(|_| zeph_llm::LlmError::Timeout)?
}

/// Build a prose summarization prompt from a message slice and optional guidelines.
///
/// The returned string is suitable for sending to an LLM as a `User` message.
/// Guidelines are injected inside `<compression-guidelines>` XML tags when non-empty.
///
/// # Examples
///
/// ```no_run
/// use zeph_context::summarization::build_chunk_prompt;
/// let prompt = build_chunk_prompt(&[], "be concise");
/// assert!(prompt.contains("compression-guidelines"));
/// ```
#[must_use]
pub fn build_chunk_prompt(messages: &[Message], guidelines: &str) -> String {
    let history_text = format_history(messages);

    let guidelines_section = guidelines_xml(guidelines);

    format!(
        "<analysis>\n\
         Analyze this conversation and produce a structured compaction note for self-consumption.\n\
         This note replaces the original messages in your context window — be thorough.\n\
         Longer is better if it preserves actionable detail.\n\
         </analysis>\n\
         {guidelines_section}\n\
         Produce exactly these 9 sections:\n\
         1. User Intent — what the user is ultimately trying to accomplish\n\
         2. Technical Concepts — key technologies, patterns, constraints discussed\n\
         3. Files & Code — file paths, function names, structs, enums touched or relevant\n\
         4. Errors & Fixes — every error encountered and whether/how it was resolved\n\
         5. Problem Solving — approaches tried, decisions made, alternatives rejected\n\
         6. User Messages — verbatim user requests that are still pending or relevant\n\
         7. Pending Tasks — items explicitly promised or left TODO\n\
         8. Current Work — the exact task in progress at the moment of compaction\n\
         9. Next Step — the single most important action to take immediately after compaction\n\
         \n\
         Conversation:\n{history_text}"
    )
}

/// Build a structured JSON summarization prompt for `AnchoredSummary` output.
///
/// The returned string is suitable for sending to an LLM as a `User` message.
/// Guidelines are injected inside `<compression-guidelines>` XML tags when non-empty.
///
/// # Examples
///
/// ```no_run
/// use zeph_context::summarization::build_anchored_summary_prompt;
/// let prompt = build_anchored_summary_prompt(&[], "");
/// assert!(prompt.contains("session_intent"));
/// ```
#[must_use]
pub fn build_anchored_summary_prompt(messages: &[Message], guidelines: &str) -> String {
    let history_text = format_history(messages);
    let guidelines_section = guidelines_xml(guidelines);

    format!(
        "<analysis>\n\
         You are compacting a conversation into a structured summary for self-consumption.\n\
         This summary replaces the original messages in your context window.\n\
         Every field MUST be populated — empty fields mean lost information.\n\
         </analysis>\n\
         {guidelines_section}\n\
         Produce a JSON object with exactly these 5 fields:\n\
         - session_intent: string — what the user is trying to accomplish\n\
         - files_modified: string[] — file paths, function names, structs touched\n\
         - decisions_made: string[] — each entry: \"Decision: X — Reason: Y\"\n\
         - open_questions: string[] — unresolved questions or blockers\n\
         - next_steps: string[] — concrete next actions\n\
         \n\
         Be thorough. Preserve all file paths, line numbers, error messages, \
         and specific identifiers — they cannot be recovered.\n\
         \n\
         Conversation:\n{history_text}"
    )
}

/// Build a last-resort metadata summary without calling the LLM.
///
/// Used when LLM summarization repeatedly fails. The result records message counts
/// and truncated previews of the last user and assistant messages.
#[must_use]
pub fn build_metadata_summary(messages: &[Message], truncate: fn(&str, usize) -> String) -> String {
    let mut user_count = 0usize;
    let mut assistant_count = 0usize;
    let mut system_count = 0usize;
    let mut last_user = String::new();
    let mut last_assistant = String::new();

    for m in messages {
        match m.role {
            Role::User => {
                user_count += 1;
                if !m.content.is_empty() {
                    last_user.clone_from(&m.content);
                }
            }
            Role::Assistant => {
                assistant_count += 1;
                if !m.content.is_empty() {
                    last_assistant.clone_from(&m.content);
                }
            }
            Role::System => system_count += 1,
        }
    }

    let last_user_preview = truncate(&last_user, 200);
    let last_assistant_preview = truncate(&last_assistant, 200);

    format!(
        "[metadata summary — LLM compaction unavailable]\n\
         Messages compacted: {} ({} user, {} assistant, {} system)\n\
         Last user message: {last_user_preview}\n\
         Last assistant message: {last_assistant_preview}",
        messages.len(),
        user_count,
        assistant_count,
        system_count,
    )
}

/// Build a summarization prompt for a single tool-call pair.
///
/// The returned string is suitable for sending to an LLM as a `User` message.
#[must_use]
pub fn build_tool_pair_summary_prompt(req: &Message, res: &Message) -> String {
    format!(
        "Produce a concise but technically precise summary of this tool invocation.\n\
         Preserve all facts that would be needed to continue work without re-running the tool:\n\
         - Tool name and key input parameters (file paths, function names, patterns, line ranges)\n\
         - Exact findings: line numbers, struct/enum/function names, error messages, numeric values\n\
         - Outcome: what was found, changed, created, or confirmed\n\
         Do NOT omit specific identifiers, paths, or numbers — they cannot be recovered later.\n\
         Use 2-4 sentences maximum.\n\n\
         <tool_request>\n{}\n</tool_request>\n\n<tool_response>\n{}\n</tool_response>",
        req.content, res.content
    )
}

/// Remove a fraction of tool-response messages from a conversation using a middle-out strategy.
///
/// `fraction` is in range `(0.0, 1.0]` — fraction of tool responses to replace with compact
/// references. Tool outputs that have an overflow UUID are replaced with a `read_overflow`
/// hint; others become `[compacted]`.
///
/// Returns the modified message list.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]
#[must_use]
pub fn remove_tool_responses_middle_out(mut messages: Vec<Message>, fraction: f32) -> Vec<Message> {
    let tool_indices: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| {
            m.parts.iter().any(|p| {
                matches!(
                    p,
                    MessagePart::ToolResult { .. } | MessagePart::ToolOutput { .. }
                )
            })
        })
        .map(|(i, _)| i)
        .collect();

    if tool_indices.is_empty() {
        return messages;
    }

    let n = tool_indices.len();
    let to_remove = ((n as f32 * fraction).ceil() as usize).min(n);

    let center = n / 2;
    let mut remove_set: Vec<usize> = Vec::with_capacity(to_remove);
    let mut left = center as isize - 1;
    let mut right = center;
    let mut count = 0;

    while count < to_remove {
        if right < n {
            remove_set.push(tool_indices[right]);
            count += 1;
            right += 1;
        }
        if count < to_remove && left >= 0 {
            let idx = left as usize;
            if !remove_set.contains(&tool_indices[idx]) {
                remove_set.push(tool_indices[idx]);
                count += 1;
            }
        }
        left -= 1;
        if left < 0 && right >= n {
            break;
        }
    }

    for &msg_idx in &remove_set {
        let msg = &mut messages[msg_idx];
        for part in &mut msg.parts {
            match part {
                MessagePart::ToolResult { content, .. } => {
                    let ref_notice = extract_overflow_ref(content).map_or_else(
                        || String::from("[compacted]"),
                        |uuid| {
                            format!("[tool output pruned; use read_overflow {uuid} to retrieve]")
                        },
                    );
                    *content = ref_notice;
                }
                MessagePart::ToolOutput {
                    body, compacted_at, ..
                } if compacted_at.is_none() => {
                    let ref_notice = extract_overflow_ref(body)
                        .map(|uuid| {
                            format!("[tool output pruned; use read_overflow {uuid} to retrieve]")
                        })
                        .unwrap_or_default();
                    *body = ref_notice;
                    *compacted_at = Some(
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs()
                            .cast_signed(),
                    );
                }
                _ => {}
            }
        }
        msg.rebuild_content();
    }
    messages
}

/// Extract the overflow UUID from a tool output body, if present.
///
/// The overflow notice has the format:
/// `\n[full output stored — ID: {uuid} — {bytes} bytes, use read_overflow tool to retrieve]`
///
/// Returns the UUID substring on success, or `None` if the notice is absent.
#[must_use]
pub fn extract_overflow_ref(body: &str) -> Option<&str> {
    let start = body.find(OVERFLOW_NOTICE_PREFIX)?;
    let rest = &body[start + OVERFLOW_NOTICE_PREFIX.len()..];
    let end = rest.find(" \u{2014} ")?;
    Some(&rest[..end])
}

fn format_history(messages: &[Message]) -> String {
    let estimated_len: usize = messages
        .iter()
        .map(|m| "[assistant]: ".len() + m.content.len() + 2)
        .sum();
    let mut history_text = String::with_capacity(estimated_len);
    for (i, m) in messages.iter().enumerate() {
        if i > 0 {
            history_text.push_str("\n\n");
        }
        let role = match m.role {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::System => "system",
        };
        let _ = write!(history_text, "[{role}]: {}", m.content);
    }
    history_text
}

fn guidelines_xml(guidelines: &str) -> String {
    if guidelines.is_empty() {
        String::new()
    } else {
        format!("\n<compression-guidelines>\n{guidelines}\n</compression-guidelines>\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeph_llm::provider::{Message, MessageMetadata, Role};

    fn user_msg(content: &str) -> Message {
        Message {
            role: Role::User,
            content: content.to_string(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        }
    }

    fn assistant_msg(content: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: content.to_string(),
            parts: vec![],
            metadata: MessageMetadata::default(),
        }
    }

    #[test]
    fn build_chunk_prompt_includes_guidelines_section() {
        let msgs = [user_msg("hello")];
        let prompt = build_chunk_prompt(&msgs, "be concise");
        assert!(
            prompt.contains("<compression-guidelines>"),
            "prompt must include guidelines XML"
        );
        assert!(
            prompt.contains("be concise"),
            "prompt must embed the guidelines text"
        );
    }

    #[test]
    fn build_chunk_prompt_no_guidelines_omits_section() {
        let prompt = build_chunk_prompt(&[], "");
        assert!(
            !prompt.contains("<compression-guidelines>"),
            "empty guidelines must not produce the XML section"
        );
    }

    #[test]
    fn build_anchored_summary_prompt_contains_json_fields() {
        let prompt = build_anchored_summary_prompt(&[], "");
        assert!(prompt.contains("session_intent"));
        assert!(prompt.contains("files_modified"));
        assert!(prompt.contains("next_steps"));
    }

    #[test]
    fn build_metadata_summary_counts_messages() {
        let msgs = [user_msg("hi"), assistant_msg("hello"), user_msg("bye")];
        let summary = build_metadata_summary(&msgs, |s, n| s.chars().take(n).collect());
        assert!(summary.contains("3 (2 user, 1 assistant, 0 system)"));
    }

    #[test]
    fn build_tool_pair_summary_prompt_contains_request_and_response() {
        let req = user_msg("req content");
        let res = assistant_msg("res content");
        let prompt = build_tool_pair_summary_prompt(&req, &res);
        assert!(prompt.contains("req content"));
        assert!(prompt.contains("res content"));
    }

    #[test]
    fn extract_overflow_ref_returns_uuid_when_present() {
        let uuid = "550e8400-e29b-41d4-a716-446655440000";
        let body =
            format!("some output\n[full output stored \u{2014} ID: {uuid} \u{2014} 12345 bytes]");
        assert_eq!(extract_overflow_ref(&body), Some(uuid));
    }

    #[test]
    fn extract_overflow_ref_returns_none_when_absent() {
        assert_eq!(extract_overflow_ref("normal output"), None);
    }

    fn tool_result_msg(content: &str) -> Message {
        use zeph_llm::provider::MessagePart;
        Message {
            role: Role::User,
            content: content.to_string(),
            parts: vec![
                MessagePart::ToolUse {
                    id: "t1".into(),
                    name: "bash".into(),
                    input: serde_json::Value::Null,
                },
                MessagePart::ToolResult {
                    tool_use_id: "t1".into(),
                    content: content.to_string(),
                    is_error: false,
                },
            ],
            metadata: MessageMetadata::default(),
        }
    }

    #[test]
    fn remove_tool_responses_middle_out_clears_correct_fraction() {
        // 4 tool messages, fraction=0.5 → ceil(4*0.5)=2 must be replaced with [compacted]
        let mut messages = vec![
            tool_result_msg("out0"),
            tool_result_msg("out1"),
            tool_result_msg("out2"),
            tool_result_msg("out3"),
        ];
        messages = remove_tool_responses_middle_out(messages, 0.5);

        let compacted_count = messages
            .iter()
            .flat_map(|m| m.parts.iter())
            .filter(|p| {
                if let zeph_llm::provider::MessagePart::ToolResult { content, .. } = p {
                    content == "[compacted]"
                } else {
                    false
                }
            })
            .count();

        assert_eq!(
            compacted_count, 2,
            "ceil(4 * 0.5) = 2 tool results must be replaced with [compacted]"
        );
    }

    #[test]
    fn remove_tool_responses_middle_out_no_tool_messages_returns_unchanged() {
        let messages = vec![user_msg("hello"), assistant_msg("hi")];
        let result = remove_tool_responses_middle_out(messages.clone(), 0.5);
        assert_eq!(result.len(), messages.len());
        assert!(
            result.iter().all(|m| m.parts.is_empty()),
            "non-tool messages must be unchanged"
        );
    }
}
