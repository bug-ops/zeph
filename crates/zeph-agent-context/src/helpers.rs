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

use zeph_config::ContextFormat;
use zeph_llm::provider::{Message, MessagePart, Role};
use zeph_memory::TokenCounter;

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

/// Fetch graph memory facts for the given query and inject them into the context budget.
///
/// Delegates to [`fetch_graph_facts_raw`] using fields from `view`.
///
/// Returns `None` when graph recall is disabled, the budget is zero, no memory is
/// attached, or the recalled fact set is empty after budget enforcement.
///
/// # Errors
///
/// Returns [`ContextError::Memory`] when the graph recall backend returns an error.
pub async fn fetch_graph_facts(
    view: &ContextAssemblyView<'_>,
    query: &str,
    budget_tokens: usize,
    tc: &TokenCounter,
) -> Result<Option<Message>, ContextError> {
    fetch_graph_facts_raw(
        view.memory.as_deref(),
        &view.graph_config,
        query,
        budget_tokens,
        tc,
    )
    .await
    .map_err(ContextError::Memory)
}

/// Fetch graph memory facts using individual field arguments.
///
/// This is the raw-args variant used by `zeph-core` test bridge methods and by
/// [`fetch_graph_facts`] internally. It accepts only the fields that the graph recall
/// logic actually accesses, avoiding the need to construct a full [`ContextAssemblyView`]
/// in test harnesses.
///
/// # Errors
///
/// Returns [`zeph_memory::MemoryError`] when the graph recall backend returns an error.
#[allow(clippy::too_many_lines, clippy::items_after_statements)]
pub async fn fetch_graph_facts_raw(
    memory: Option<&zeph_memory::semantic::SemanticMemory>,
    graph_config: &zeph_config::GraphConfig,
    query: &str,
    budget_tokens: usize,
    tc: &TokenCounter,
) -> Result<Option<Message>, zeph_memory::MemoryError> {
    if budget_tokens == 0 || !graph_config.enabled {
        return Ok(None);
    }
    let Some(memory) = memory else {
        return Ok(None);
    };
    let recall_limit = graph_config.recall_limit;
    let temporal_decay_rate = graph_config.temporal_decay_rate;
    let edge_types = zeph_memory::classify_graph_subgraph(query);
    let sa_config = &graph_config.spreading_activation;

    let mut body = String::from(GRAPH_FACTS_PREFIX);
    let mut tokens_so_far = tc.count_tokens(&body);
    let max_hops = graph_config.max_hops;

    use zeph_config::memory::GraphRetrievalStrategy;
    let effective_strategy = if sa_config.enabled {
        GraphRetrievalStrategy::Synapse
    } else {
        graph_config.retrieval_strategy
    };

    let _span = tracing::info_span!("memory.graph.dispatch", ?effective_strategy).entered();

    match effective_strategy {
        GraphRetrievalStrategy::Synapse => {
            let sa_params = zeph_memory::graph::SpreadingActivationParams {
                decay_lambda: sa_config.decay_lambda,
                max_hops: sa_config.max_hops,
                activation_threshold: sa_config.activation_threshold,
                inhibition_threshold: sa_config.inhibition_threshold,
                max_activated_nodes: sa_config.max_activated_nodes,
                temporal_decay_rate,
                seed_structural_weight: sa_config.seed_structural_weight,
                seed_community_cap: sa_config.seed_community_cap,
            };
            let timeout_ms = effective_recall_timeout_ms(sa_config.recall_timeout_ms);
            let activated_facts = match tokio::time::timeout(
                std::time::Duration::from_millis(timeout_ms),
                memory.recall_graph_activated(query, recall_limit, sa_params, &edge_types),
            )
            .await
            {
                Ok(Ok(facts)) => facts,
                Ok(Err(e)) => {
                    tracing::warn!("spreading activation recall failed: {e:#}");
                    Vec::new()
                }
                Err(_) => {
                    tracing::warn!("spreading activation recall timed out ({timeout_ms}ms)");
                    Vec::new()
                }
            };
            if activated_facts.is_empty() {
                return Ok(None);
            }
            for f in &activated_facts {
                let fact_text = f.edge.fact.replace(['\n', '\r', '<', '>'], " ");
                let line = format!(
                    "- {} (confidence: {:.2}, activation: {:.2})\n",
                    fact_text, f.edge.confidence, f.activation_score
                );
                let line_tokens = tc.count_tokens(&line);
                if tokens_so_far + line_tokens > budget_tokens {
                    break;
                }
                body.push_str(&line);
                tokens_so_far += line_tokens;
            }
        }
        GraphRetrievalStrategy::Bfs => {
            let facts = memory
                .recall_graph(
                    query,
                    recall_limit,
                    max_hops,
                    None,
                    temporal_decay_rate,
                    &edge_types,
                )
                .await?;
            if facts.is_empty() {
                return Ok(None);
            }
            for f in &facts {
                let fact_text = f.fact.replace(['\n', '\r', '<', '>'], " ");
                let line = format!("- {} (confidence: {:.2})\n", fact_text, f.confidence);
                let line_tokens = tc.count_tokens(&line);
                if tokens_so_far + line_tokens > budget_tokens {
                    break;
                }
                body.push_str(&line);
                tokens_so_far += line_tokens;
            }
        }
        GraphRetrievalStrategy::AStar => {
            let facts = memory
                .recall_graph_astar(
                    query,
                    recall_limit,
                    max_hops,
                    temporal_decay_rate,
                    &edge_types,
                )
                .await?;
            if facts.is_empty() {
                return Ok(None);
            }
            for f in &facts {
                let fact_text = f.fact.replace(['\n', '\r', '<', '>'], " ");
                let line = format!("- {} (confidence: {:.2})\n", fact_text, f.confidence);
                let line_tokens = tc.count_tokens(&line);
                if tokens_so_far + line_tokens > budget_tokens {
                    break;
                }
                body.push_str(&line);
                tokens_so_far += line_tokens;
            }
        }
        GraphRetrievalStrategy::WaterCircles => {
            let ring_limit = graph_config.watercircles.ring_limit;
            let facts = memory
                .recall_graph_watercircles(
                    query,
                    recall_limit,
                    max_hops,
                    ring_limit,
                    temporal_decay_rate,
                    &edge_types,
                )
                .await?;
            if facts.is_empty() {
                return Ok(None);
            }
            for f in &facts {
                let fact_text = f.fact.replace(['\n', '\r', '<', '>'], " ");
                let line = format!("- {} (confidence: {:.2})\n", fact_text, f.confidence);
                let line_tokens = tc.count_tokens(&line);
                if tokens_so_far + line_tokens > budget_tokens {
                    break;
                }
                body.push_str(&line);
                tokens_so_far += line_tokens;
            }
        }
        GraphRetrievalStrategy::BeamSearch => {
            let beam_width = graph_config.beam_search.beam_width;
            let facts = memory
                .recall_graph_beam(
                    query,
                    recall_limit,
                    beam_width,
                    max_hops,
                    temporal_decay_rate,
                    &edge_types,
                )
                .await?;
            if facts.is_empty() {
                return Ok(None);
            }
            for f in &facts {
                let fact_text = f.fact.replace(['\n', '\r', '<', '>'], " ");
                let line = format!("- {} (confidence: {:.2})\n", fact_text, f.confidence);
                let line_tokens = tc.count_tokens(&line);
                if tokens_so_far + line_tokens > budget_tokens {
                    break;
                }
                body.push_str(&line);
                tokens_so_far += line_tokens;
            }
        }
        GraphRetrievalStrategy::Hybrid => {
            const CLASSIFIER_TIMEOUT_MS: u64 = 2_000;
            let classified = tokio::time::timeout(
                std::time::Duration::from_millis(CLASSIFIER_TIMEOUT_MS),
                memory.classify_graph_strategy(query),
            )
            .await
            .unwrap_or_else(|_| {
                tracing::warn!(
                    "hybrid strategy classifier timed out after {CLASSIFIER_TIMEOUT_MS}ms, \
                     falling back to synapse"
                );
                "synapse".to_owned()
            });
            tracing::debug!(classified_strategy = %classified, "hybrid dispatch: classified");
            let facts = match classified.as_str() {
                "astar" => {
                    memory
                        .recall_graph_astar(
                            query,
                            recall_limit,
                            max_hops,
                            temporal_decay_rate,
                            &edge_types,
                        )
                        .await?
                }
                "watercircles" => {
                    let ring_limit = graph_config.watercircles.ring_limit;
                    memory
                        .recall_graph_watercircles(
                            query,
                            recall_limit,
                            max_hops,
                            ring_limit,
                            temporal_decay_rate,
                            &edge_types,
                        )
                        .await?
                }
                "beam_search" => {
                    let beam_width = graph_config.beam_search.beam_width;
                    memory
                        .recall_graph_beam(
                            query,
                            recall_limit,
                            beam_width,
                            max_hops,
                            temporal_decay_rate,
                            &edge_types,
                        )
                        .await?
                }
                _ => {
                    let sa_params = zeph_memory::graph::SpreadingActivationParams {
                        decay_lambda: sa_config.decay_lambda,
                        max_hops: sa_config.max_hops,
                        activation_threshold: sa_config.activation_threshold,
                        inhibition_threshold: sa_config.inhibition_threshold,
                        max_activated_nodes: sa_config.max_activated_nodes,
                        temporal_decay_rate,
                        seed_structural_weight: sa_config.seed_structural_weight,
                        seed_community_cap: sa_config.seed_community_cap,
                    };
                    memory
                        .recall_graph_activated(query, recall_limit, sa_params, &edge_types)
                        .await?
                        .into_iter()
                        .map(|f| zeph_memory::graph::types::GraphFact {
                            entity_name: f.edge.source_entity_id.to_string(),
                            relation: f.edge.relation.clone(),
                            target_name: f.edge.target_entity_id.to_string(),
                            fact: f.edge.fact.clone(),
                            entity_match_score: f.activation_score,
                            hop_distance: 0,
                            confidence: f.edge.confidence,
                            valid_from: Some(f.edge.valid_from.clone()),
                            edge_type: f.edge.edge_type,
                            retrieval_count: f.edge.retrieval_count,
                        })
                        .collect()
                }
            };
            if facts.is_empty() {
                return Ok(None);
            }
            for f in &facts {
                let fact_text = f.fact.replace(['\n', '\r', '<', '>'], " ");
                let line = format!("- {} (confidence: {:.2})\n", fact_text, f.confidence);
                let line_tokens = tc.count_tokens(&line);
                if tokens_so_far + line_tokens > budget_tokens {
                    break;
                }
                body.push_str(&line);
                tokens_so_far += line_tokens;
            }
        }
    }

    if body == GRAPH_FACTS_PREFIX {
        return Ok(None);
    }

    Ok(Some(Message::from_legacy(Role::System, body)))
}

/// Fetch semantically recalled messages using individual field arguments.
///
/// Raw-args variant used by `zeph-core` test bridge methods and by [`fetch_semantic_recall`].
///
/// # Errors
///
/// Returns [`zeph_memory::MemoryError`] when the memory backend returns an error.
pub async fn fetch_semantic_recall_raw(
    memory: Option<&zeph_memory::semantic::SemanticMemory>,
    recall_limit: usize,
    context_format: ContextFormat,
    query: &str,
    token_budget: usize,
    tc: &TokenCounter,
    router: Option<&dyn zeph_memory::AsyncMemoryRouter>,
) -> Result<(Option<Message>, Option<f32>), zeph_memory::MemoryError> {
    let Some(memory) = memory else {
        return Ok((None, None));
    };
    if recall_limit == 0 || token_budget == 0 {
        return Ok((None, None));
    }

    let recalled = if let Some(r) = router {
        memory
            .recall_routed_async(query, recall_limit, None, r)
            .await?
    } else {
        memory.recall(query, recall_limit, None).await?
    };
    if recalled.is_empty() {
        return Ok((None, None));
    }

    let top_score = recalled.first().map(|r| r.score);
    let initial_cap = (recall_limit * 512).min(token_budget * 3);
    let mut recall_text = String::with_capacity(initial_cap);
    recall_text.push_str(RECALL_PREFIX);
    let mut tokens_used = tc.count_tokens(&recall_text);

    for item in &recalled {
        if item.message.content.starts_with("[skipped]")
            || item.message.content.starts_with("[stopped]")
        {
            continue;
        }
        let entry = match context_format {
            ContextFormat::Structured => format_structured_recall_entry(item),
            ContextFormat::Plain => format_plain_recall_entry(item),
        };
        let entry_tokens = tc.count_tokens(&entry);
        if tokens_used + entry_tokens > token_budget {
            break;
        }
        recall_text.push_str(&entry);
        tokens_used += entry_tokens;
    }

    if tokens_used > tc.count_tokens(RECALL_PREFIX) {
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
pub async fn fetch_semantic_recall(
    view: &ContextAssemblyView<'_>,
    query: &str,
    token_budget: usize,
    tc: &TokenCounter,
    router: Option<&dyn zeph_memory::AsyncMemoryRouter>,
) -> Result<(Option<Message>, Option<f32>), ContextError> {
    fetch_semantic_recall_raw(
        view.memory.as_deref(),
        view.recall_limit,
        view.context_format,
        query,
        token_budget,
        tc,
        router,
    )
    .await
    .map_err(ContextError::Memory)
}

fn format_plain_recall_entry(item: &zeph_memory::RecalledMessage) -> String {
    let role_label = match item.message.role {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::System => "system",
    };
    format!("- [{}] {}\n", role_label, item.message.content)
}

#[allow(clippy::map_unwrap_or)]
fn format_structured_recall_entry(item: &zeph_memory::RecalledMessage) -> String {
    let source = match item.message.role {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::System => "system",
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
