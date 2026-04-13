// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Context assembly helpers for the Zeph agent.
//!
//! This module provides utility functions for fetching individual context slots
//! (summaries, cross-session context, semantic recall, etc.) used by
//! `Agent::prepare_context` and test helpers.
//!
//! The top-level gather logic is in [`zeph_context::assembler::ContextAssembler`].

#[cfg(test)]
use zeph_llm::provider::{Message, MessagePart, Role};
#[cfg(test)]
use zeph_memory::TokenCounter;

#[cfg(test)]
use super::super::error::AgentError;
#[cfg(test)]
use super::super::{
    CROSS_SESSION_PREFIX, GRAPH_FACTS_PREFIX, MemoryState, RECALL_PREFIX, SUMMARY_PREFIX,
};
#[cfg(test)]
use crate::redact::scrub_content;

#[cfg(test)]
pub(super) fn format_correction_note(_original_output: &str, correction_text: &str) -> String {
    format!(
        "- Past user correction: \"{}\"",
        super::truncate_chars(&scrub_content(correction_text), 200)
    )
}

#[cfg(test)]
pub(super) fn effective_recall_timeout_ms(configured: u64) -> u64 {
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

#[cfg(test)]
pub(super) async fn fetch_graph_facts(
    memory_state: &MemoryState,
    query: &str,
    budget_tokens: usize,
    tc: &TokenCounter,
) -> Result<Option<Message>, AgentError> {
    if budget_tokens == 0 || !memory_state.extraction.graph_config.enabled {
        return Ok(None);
    }
    let Some(ref memory) = memory_state.persistence.memory else {
        return Ok(None);
    };
    let recall_limit = memory_state.extraction.graph_config.recall_limit;
    let temporal_decay_rate = memory_state.extraction.graph_config.temporal_decay_rate;
    let edge_types = zeph_memory::classify_graph_subgraph(query);
    let sa_config = &memory_state.extraction.graph_config.spreading_activation;

    let mut body = String::from(GRAPH_FACTS_PREFIX);
    let mut tokens_so_far = tc.count_tokens(&body);

    if sa_config.enabled {
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
        let recall_fut = memory.recall_graph_activated(query, recall_limit, sa_params, &edge_types);
        let activated_facts =
            match tokio::time::timeout(std::time::Duration::from_millis(timeout_ms), recall_fut)
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
    } else {
        let max_hops = memory_state.extraction.graph_config.max_hops;
        let facts = memory
            .recall_graph(
                query,
                recall_limit,
                max_hops,
                None,
                temporal_decay_rate,
                &edge_types,
            )
            .await
            .map_err(|e| {
                tracing::warn!("graph recall failed: {e:#}");
                AgentError::Memory(e)
            })?;

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

    if body == GRAPH_FACTS_PREFIX {
        return Ok(None);
    }

    Ok(Some(Message::from_legacy(Role::System, body)))
}

#[cfg(test)]
pub(super) async fn fetch_semantic_recall(
    memory_state: &MemoryState,
    query: &str,
    token_budget: usize,
    tc: &TokenCounter,
    router: Option<&dyn zeph_memory::AsyncMemoryRouter>,
) -> Result<(Option<Message>, Option<f32>), AgentError> {
    let Some(memory) = &memory_state.persistence.memory else {
        return Ok((None, None));
    };
    if memory_state.persistence.recall_limit == 0 || token_budget == 0 {
        return Ok((None, None));
    }

    let recalled = if let Some(r) = router {
        memory
            .recall_routed_async(query, memory_state.persistence.recall_limit, None, r)
            .await?
    } else {
        memory
            .recall(query, memory_state.persistence.recall_limit, None)
            .await?
    };
    if recalled.is_empty() {
        return Ok((None, None));
    }

    let top_score = recalled.first().map(|r| r.score);

    let initial_cap = (memory_state.persistence.recall_limit * 512).min(token_budget * 3);
    let mut recall_text = String::with_capacity(initial_cap);
    recall_text.push_str(RECALL_PREFIX);
    let mut tokens_used = tc.count_tokens(&recall_text);

    for item in &recalled {
        if item.message.content.starts_with("[skipped]")
            || item.message.content.starts_with("[stopped]")
        {
            continue;
        }
        let role_label = match item.message.role {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::System => "system",
        };
        let entry = format!("- [{}] {}\n", role_label, item.message.content);
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

#[cfg(test)]
pub(super) async fn fetch_summaries(
    memory_state: &MemoryState,
    token_budget: usize,
    tc: &TokenCounter,
) -> Result<Option<Message>, AgentError> {
    let (Some(memory), Some(cid)) = (
        &memory_state.persistence.memory,
        memory_state.persistence.conversation_id,
    ) else {
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

#[cfg(test)]
pub(super) async fn fetch_cross_session(
    memory_state: &MemoryState,
    query: &str,
    token_budget: usize,
    tc: &TokenCounter,
) -> Result<Option<Message>, AgentError> {
    let (Some(memory), Some(cid)) = (
        &memory_state.persistence.memory,
        memory_state.persistence.conversation_id,
    ) else {
        return Ok(None);
    };
    if token_budget == 0 {
        return Ok(None);
    }

    let threshold = memory_state.persistence.cross_session_score_threshold;
    let results: Vec<_> = memory
        .search_session_summaries(query, 5, Some(cid))
        .await?
        .into_iter()
        .filter(|r| r.score >= threshold)
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
