// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Pure helper functions for context assembly.
//!
//! These functions are called by `assembly.rs` in `zeph-core` (via a module alias)
//! and by the [`crate::service::ContextService`] stubs that will be filled in during
//! subsequent migration steps.
//!
//! All functions operate on [`crate::state::ContextAssemblyView`] instead of the
//! `zeph-core`-internal `MemoryState`, keeping this crate free of `zeph-core` types.

use std::fmt::Write as _;
use std::time::Instant;

use zeph_config::ContextFormat;
use zeph_llm::provider::{Message, MessagePart, Role};
use zeph_memory::{RetrievalFailureRecord, RetrievalFailureType, TokenCounter};

use crate::error::ContextError;
use crate::state::ContextAssemblyView;

/// System message prefix for persona context injected into the system prompt.
pub const PERSONA_PREFIX: &str = "[Persona context]\n";
/// System message prefix for trajectory (past experience) context.
pub const TRAJECTORY_PREFIX: &str = "[Past experience]\n";
/// System message prefix for tree-based memory summaries.
pub const TREE_MEMORY_PREFIX: &str = "[Memory summary]\n";
/// System message prefix for reasoning strategy context.
pub const REASONING_PREFIX: &str = "[Reasoning Strategy]\n";

/// System message prefix for graph memory facts injected into context.
pub const GRAPH_FACTS_PREFIX: &str = "[known facts]\n";
/// System message prefix for semantic recall entries.
pub const RECALL_PREFIX: &str = "[semantic recall]\n";
/// System message prefix for session summary entries.
pub const SUMMARY_PREFIX: &str = "[conversation summaries]\n";
/// System message prefix for cross-session context entries.
pub const CROSS_SESSION_PREFIX: &str = "[cross-session context]\n";

/// System message prefix for past user corrections injected into context.
pub const CORRECTIONS_PREFIX: &str = "[past corrections]\n";
/// System message prefix for code-context (repo-map / file context) injections.
pub const CODE_CONTEXT_PREFIX: &str = "[code context]\n";
/// User message prefix for session digest summaries from the previous interaction.
pub const SESSION_DIGEST_PREFIX: &str = "[Session digest from previous interaction]\n";
/// System message prefix for LSP context notes (diagnostics, hover data, etc.).
pub const LSP_NOTE_PREFIX: &str = "[lsp ";
/// System message prefix for document RAG results.
pub const DOCUMENT_RAG_PREFIX: &str = "## Relevant documents\n";

/// Truncate `s` to at most `max_chars` Unicode scalar values.
///
/// Delegates to `zeph_common::text::truncate_to_chars` which respects UTF-8 boundaries.
#[must_use]
pub fn truncate_chars(s: &str, max_chars: usize) -> String {
    zeph_common::text::truncate_to_chars(s, max_chars)
}

/// Format a user correction as a single bullet point for injection into the system prompt.
///
/// The `correction_text` must already be scrubbed by the caller before being passed here.
/// Truncated to 200 characters to avoid inflating the context with verbose correction notes.
#[must_use]
pub fn format_correction_note(correction_text: &str) -> String {
    format!(
        "- Past user correction: \"{}\"",
        truncate_chars(correction_text, 200)
    )
}

/// Return the effective spreading-activation recall timeout in milliseconds.
///
/// A configured value of `0` would silently disable recall; this function clamps it to
/// `100ms` and emits a warning so operators notice the misconfiguration without a crash.
pub fn effective_recall_timeout_ms(configured: u64) -> u64 {
    if configured == 0 {
        tracing::warn!(
            "recall_timeout_ms is 0, which would disable spreading activation recall; \
             clamping to 100ms"
        );
        100
    } else {
        configured
    }
}

/// Read-only inputs for [`fetch_semantic_recall_raw`]: the query, its retrieval
/// limits/format, and the confidence threshold used to flag low-confidence recall for
/// telemetry.
///
/// Distinct from [`crate::service::SemanticRecallParams`] (the service-level façade
/// struct, which additionally carries tiered-retrieval provider/config fields) — this is
/// the smaller subset of fields actually read by the flat (non-tiered) recall path.
/// `memory` and `router` are kept as separate arguments on the function since they are
/// resource handles rather than per-call query configuration.
pub struct SemanticRecallRawParams<'a> {
    /// Maximum number of memories to retrieve.
    pub recall_limit: usize,
    /// Format applied when serialising recalled memories.
    pub context_format: ContextFormat,
    /// Query string used for retrieval.
    pub query: &'a str,
    /// Maximum number of tokens the injected recall may consume.
    pub token_budget: usize,
    /// Token counter used to enforce `token_budget`.
    pub tc: &'a TokenCounter,
    /// When `Some(t)`, results with a top score below `t` are classified as
    /// low-confidence and logged via the memory's retrieval failure logger.
    pub low_confidence_threshold: Option<f32>,
}

/// Fetch semantically recalled messages using individual field arguments.
///
/// Raw-args variant used by [`fetch_semantic_recall`] and by
/// [`crate::service::ContextService`]'s flat (non-tiered) recall path.
///
/// # Errors
///
/// Returns [`zeph_memory::MemoryError`] when the memory backend returns an error.
#[tracing::instrument(
    name = "agent_context.helpers.fetch_semantic_recall_raw",
    skip_all,
    err
)]
pub async fn fetch_semantic_recall_raw(
    memory: Option<&zeph_memory::semantic::SemanticMemory>,
    params: SemanticRecallRawParams<'_>,
    router: Option<&dyn zeph_memory::AsyncMemoryRouter>,
) -> Result<(Option<Message>, Option<f32>), zeph_memory::MemoryError> {
    let Some(memory) = memory else {
        return Ok((None, None));
    };
    if params.recall_limit == 0 || params.token_budget == 0 {
        return Ok((None, None));
    }

    let t0 = Instant::now();
    let recalled = if let Some(r) = router {
        memory
            .recall_routed_async(params.query, params.recall_limit, None, r, None)
            .await?
    } else {
        memory
            .recall(params.query, params.recall_limit, None)
            .await?
    };
    let latency_ms = t0.elapsed().as_millis().try_into().unwrap_or(u64::MAX);

    if recalled.is_empty() {
        memory.log_retrieval_failure(RetrievalFailureRecord {
            conversation_id: None,
            turn_index: 0,
            failure_type: RetrievalFailureType::NoHit,
            retrieval_strategy: "semantic".to_owned(),
            query_text: params.query.to_owned(),
            query_len: params.query.len(),
            top_score: None,
            confidence_threshold: params.low_confidence_threshold,
            result_count: 0,
            latency_ms,
            edge_types: None,
            error_context: None,
        });
        return Ok((None, None));
    }

    let top_score = recalled.first().map(|r| r.score);

    if let (Some(score), Some(threshold)) = (top_score, params.low_confidence_threshold)
        && score < threshold
    {
        memory.log_retrieval_failure(RetrievalFailureRecord {
            conversation_id: None,
            turn_index: 0,
            failure_type: RetrievalFailureType::LowConfidence,
            retrieval_strategy: "semantic".to_owned(),
            query_text: params.query.to_owned(),
            query_len: params.query.len(),
            top_score: Some(score),
            confidence_threshold: Some(threshold),
            result_count: recalled.len(),
            latency_ms,
            edge_types: None,
            error_context: None,
        });
    }
    let initial_cap = (params.recall_limit * 512).min(params.token_budget * 3);
    let mut recall_text = String::with_capacity(initial_cap);
    recall_text.push_str(RECALL_PREFIX);
    let mut tokens_used = params.tc.count_tokens(&recall_text);

    for item in &recalled {
        if item.message.content.starts_with("[skipped]")
            || item.message.content.starts_with("[stopped]")
        {
            continue;
        }
        let entry = match params.context_format {
            ContextFormat::Structured => format_structured_recall_entry(item),
            _ => format_plain_recall_entry(item),
        };
        let entry_tokens = params.tc.count_tokens(&entry);
        if tokens_used + entry_tokens > params.token_budget {
            break;
        }
        recall_text.push_str(&entry);
        tokens_used += entry_tokens;
    }

    if tokens_used > params.tc.count_tokens(RECALL_PREFIX) {
        Ok((
            Some(Message::from_parts(
                Role::System,
                vec![MessagePart::Recall { text: recall_text }],
            )),
            top_score,
        ))
    } else {
        Ok((None, None))
    }
}

/// Fetch session summaries using individual field arguments.
///
/// Raw-args variant used by `zeph-core` test bridge methods and by [`fetch_summaries`].
///
/// # Errors
///
/// Returns [`zeph_memory::MemoryError`] when the memory backend returns an error.
#[tracing::instrument(name = "agent_context.helpers.fetch_summaries_raw", skip_all, err)]
pub async fn fetch_summaries_raw(
    memory: Option<&zeph_memory::semantic::SemanticMemory>,
    conversation_id: Option<zeph_memory::ConversationId>,
    token_budget: usize,
    tc: &TokenCounter,
) -> Result<Option<Message>, zeph_memory::MemoryError> {
    let (Some(memory), Some(cid)) = (memory, conversation_id) else {
        return Ok(None);
    };
    if token_budget == 0 {
        return Ok(None);
    }

    let summaries = memory.load_summaries(cid).await?;
    if summaries.is_empty() {
        return Ok(None);
    }

    let mut summary_text = String::from(SUMMARY_PREFIX);
    let mut tokens_used = tc.count_tokens(&summary_text);

    for summary in summaries.iter().rev() {
        let first = summary.first_message_id.map_or(0, |m| m.0);
        let last = summary.last_message_id.map_or(0, |m| m.0);
        let entry = format!("- Messages {first}-{last}: {}\n", summary.content);
        let cost = tc.count_tokens(&entry);
        if tokens_used + cost > token_budget {
            break;
        }
        summary_text.push_str(&entry);
        tokens_used += cost;
    }

    if tokens_used > tc.count_tokens(SUMMARY_PREFIX) {
        Ok(Some(Message::from_parts(
            Role::System,
            vec![MessagePart::Summary { text: summary_text }],
        )))
    } else {
        Ok(None)
    }
}

/// Fetch cross-session context summaries using individual field arguments.
///
/// Raw-args variant used by `zeph-core` test bridge methods and by [`fetch_cross_session`].
///
/// # Errors
///
/// Returns [`zeph_memory::MemoryError`] when the memory backend returns an error.
#[tracing::instrument(name = "agent_context.helpers.fetch_cross_session_raw", skip_all, err)]
pub async fn fetch_cross_session_raw(
    memory: Option<&zeph_memory::semantic::SemanticMemory>,
    conversation_id: Option<zeph_memory::ConversationId>,
    cross_session_score_threshold: f32,
    query: &str,
    token_budget: usize,
    tc: &TokenCounter,
) -> Result<Option<Message>, zeph_memory::MemoryError> {
    let (Some(memory), Some(cid)) = (memory, conversation_id) else {
        return Ok(None);
    };
    if token_budget == 0 {
        return Ok(None);
    }

    let results: Vec<_> = memory
        .search_session_summaries(query, 5, Some(cid))
        .await?
        .into_iter()
        .filter(|r| r.score >= cross_session_score_threshold)
        .collect();
    if results.is_empty() {
        return Ok(None);
    }

    let mut text = String::from(CROSS_SESSION_PREFIX);
    let mut tokens_used = tc.count_tokens(&text);

    for item in &results {
        let entry = format!("- {}\n", item.summary_text);
        let cost = tc.count_tokens(&entry);
        if tokens_used + cost > token_budget {
            break;
        }
        text.push_str(&entry);
        tokens_used += cost;
    }

    if tokens_used > tc.count_tokens(CROSS_SESSION_PREFIX) {
        Ok(Some(Message::from_parts(
            Role::System,
            vec![MessagePart::CrossSession { text }],
        )))
    } else {
        Ok(None)
    }
}

/// Fetch semantically recalled messages for the given query and enforce the token budget.
///
/// Delegates to [`fetch_semantic_recall_raw`] using fields from `view`.
///
/// Returns `(None, None)` when memory is absent, recall is disabled, the budget is zero,
/// or the recalled set is empty.
///
/// The second element of the tuple is the similarity score of the top recalled entry, used
/// by the caller to track recall confidence for telemetry.
///
/// # Errors
///
/// Returns [`ContextError::Memory`] when the memory recall backend returns an error.
#[tracing::instrument(name = "agent_context.helpers.fetch_semantic_recall", skip_all, err)]
pub async fn fetch_semantic_recall(
    view: &ContextAssemblyView<'_>,
    query: &str,
    token_budget: usize,
    tc: &TokenCounter,
    router: Option<&dyn zeph_memory::AsyncMemoryRouter>,
) -> Result<(Option<Message>, Option<f32>), ContextError> {
    fetch_semantic_recall_raw(
        view.memory.as_deref(),
        SemanticRecallRawParams {
            recall_limit: view.recall_limit,
            context_format: view.context_format,
            query,
            token_budget,
            tc,
            low_confidence_threshold: None,
        },
        router,
    )
    .await
    .map_err(ContextError::Memory)
}

fn format_plain_recall_entry(item: &zeph_memory::RecalledMessage) -> String {
    let role_label = match item.message.role {
        Role::Assistant => "assistant",
        Role::System => "system",
        Role::User | _ => "user",
    };
    format!("- [{}] {}\n", role_label, item.message.content)
}

#[allow(clippy::map_unwrap_or)]
fn format_structured_recall_entry(item: &zeph_memory::RecalledMessage) -> String {
    let source = match item.message.role {
        Role::Assistant => "assistant",
        Role::System => "system",
        Role::User | _ => "user",
    };
    // Use compacted_at as a proxy for message age when available; otherwise "unknown".
    // A full timestamp lookup from SQLite would require an async DB call in the assembler
    // and is deferred to a future enhancement (TODO: enhance when message timestamps are
    // propagated into RecalledMessage).
    let date = item
        .message
        .metadata
        .compacted_at
        .and_then(|ts| chrono::DateTime::from_timestamp(ts, 0))
        .map(|dt| dt.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "unknown".to_owned());
    format!(
        "[Memory | {} | {} | relevance: {:.2}]\n{}\n",
        source, date, item.score, item.message.content
    )
}

/// Fetch session summaries for the current conversation and enforce the token budget.
///
/// Delegates to [`fetch_summaries_raw`] using fields from `view`.
///
/// Returns `None` when memory or the conversation ID is absent, the budget is zero,
/// or no summaries exist yet.
///
/// # Errors
///
/// Returns [`ContextError::Memory`] when the memory backend returns an error.
#[tracing::instrument(name = "agent_context.helpers.fetch_summaries", skip_all, err)]
pub async fn fetch_summaries(
    view: &ContextAssemblyView<'_>,
    token_budget: usize,
    tc: &TokenCounter,
) -> Result<Option<Message>, ContextError> {
    fetch_summaries_raw(
        view.memory.as_deref(),
        view.conversation_id,
        token_budget,
        tc,
    )
    .await
    .map_err(ContextError::Memory)
}

/// Fetch cross-session context summaries for the given query and enforce the token budget.
///
/// Delegates to [`fetch_cross_session_raw`] using fields from `view`.
///
/// Results are filtered by `view.cross_session_score_threshold` before token counting,
/// and the current conversation is excluded from the search results.
///
/// Returns `None` when memory or the conversation ID is absent, the budget is zero,
/// no results exceed the threshold, or the result set is empty.
///
/// # Errors
///
/// Returns [`ContextError::Memory`] when the memory backend returns an error.
#[tracing::instrument(name = "agent_context.helpers.fetch_cross_session", skip_all, err)]
pub async fn fetch_cross_session(
    view: &ContextAssemblyView<'_>,
    query: &str,
    token_budget: usize,
    tc: &TokenCounter,
) -> Result<Option<Message>, ContextError> {
    fetch_cross_session_raw(
        view.memory.as_deref(),
        view.conversation_id,
        view.cross_session_score_threshold,
        query,
        token_budget,
        tc,
    )
    .await
    .map_err(ContextError::Memory)
}

/// Budget state injected into the volatile system prompt section.
///
/// All fields are optional — omitted when the corresponding data source is unavailable.
/// [`BudgetHint::format_xml`] returns `None` when all fields would be absent.
///
/// Callers should construct this from cost-tracker and tool-orchestrator state, then call
/// `format_xml` and append the result to the system prompt when `Some`.
pub struct BudgetHint {
    /// Remaining daily budget in US cents, if a daily limit is configured.
    pub remaining_cost_cents: Option<f64>,
    /// Total daily budget in US cents, if a daily limit is configured.
    pub total_budget_cents: Option<f64>,
    /// Remaining tool-call iterations this turn.
    pub remaining_tool_calls: usize,
    /// Maximum allowed tool-call iterations per turn (0 = no limit configured).
    pub max_tool_calls: usize,
}

impl BudgetHint {
    /// Render the budget hint as an XML fragment for injection into the system prompt.
    ///
    /// Returns `None` when no meaningful budget data is available — callers must skip
    /// injection rather than injecting an empty `<budget></budget>` block.
    ///
    /// # Examples
    ///
    /// ```
    /// use zeph_agent_context::helpers::BudgetHint;
    ///
    /// let hint = BudgetHint {
    ///     remaining_cost_cents: Some(50.0),
    ///     total_budget_cents: Some(100.0),
    ///     remaining_tool_calls: 8,
    ///     max_tool_calls: 10,
    /// };
    /// let xml = hint.format_xml().unwrap();
    /// assert!(xml.contains("<remaining_cost_cents>50.00</remaining_cost_cents>"));
    /// assert!(xml.contains("<remaining_tool_calls>8</remaining_tool_calls>"));
    /// ```
    #[must_use]
    pub fn format_xml(&self) -> Option<String> {
        let has_cost = self.remaining_cost_cents.is_some();
        // Always include tool call budget — max_tool_calls > 0 in any real config.
        if !has_cost && self.max_tool_calls == 0 {
            return None;
        }
        let mut s = String::from("<budget>");
        if let Some(remaining) = self.remaining_cost_cents {
            let _ = write!(
                s,
                "\n<remaining_cost_cents>{remaining:.2}</remaining_cost_cents>"
            );
        }
        if let Some(total) = self.total_budget_cents {
            let _ = write!(s, "\n<total_budget_cents>{total:.2}</total_budget_cents>");
        }
        if self.max_tool_calls > 0 {
            let _ = write!(
                s,
                "\n<remaining_tool_calls>{}</remaining_tool_calls>",
                self.remaining_tool_calls
            );
            let _ = write!(
                s,
                "\n<max_tool_calls>{}</max_tool_calls>",
                self.max_tool_calls
            );
        }
        s.push_str("\n</budget>");
        Some(s)
    }
}

#[cfg(test)]
mod budget_hint_tests {
    use super::*;

    #[test]
    fn format_xml_none_when_no_data() {
        let hint = BudgetHint {
            remaining_cost_cents: None,
            total_budget_cents: None,
            remaining_tool_calls: 0,
            max_tool_calls: 0,
        };
        assert!(hint.format_xml().is_none());
    }

    #[test]
    fn format_xml_with_cost_only() {
        let hint = BudgetHint {
            remaining_cost_cents: Some(25.5),
            total_budget_cents: Some(100.0),
            remaining_tool_calls: 0,
            max_tool_calls: 0,
        };
        let xml = hint.format_xml().unwrap();
        assert!(xml.contains("<remaining_cost_cents>25.50</remaining_cost_cents>"));
        assert!(xml.contains("<total_budget_cents>100.00</total_budget_cents>"));
    }

    #[test]
    fn format_xml_with_tool_calls_only() {
        let hint = BudgetHint {
            remaining_cost_cents: None,
            total_budget_cents: None,
            remaining_tool_calls: 3,
            max_tool_calls: 10,
        };
        let xml = hint.format_xml().unwrap();
        assert!(xml.contains("<remaining_tool_calls>3</remaining_tool_calls>"));
        assert!(xml.contains("<max_tool_calls>10</max_tool_calls>"));
    }

    #[test]
    fn format_xml_with_all_fields() {
        let hint = BudgetHint {
            remaining_cost_cents: Some(50.0),
            total_budget_cents: Some(100.0),
            remaining_tool_calls: 8,
            max_tool_calls: 10,
        };
        let xml = hint.format_xml().unwrap();
        assert!(xml.starts_with("<budget>"));
        assert!(xml.ends_with("</budget>"));
    }
}
