// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Stateless context assembler.
//!
//! [`ContextAssembler`] gathers all memory-sourced context for a single agent turn by running
//! all async fetch operations concurrently. It takes only borrowed references via
//! [`ContextAssemblyInput`] and returns a [`PreparedContext`] ready for injection.
//!
//! Invariants:
//! - No `Agent` field mutations inside `gather()`.
//! - No channel communication inside `gather()`.
//! - All `send_status` calls remain in `Agent::prepare_context`.
//! - `session_digest` is cached (not async) and stays in `Agent::apply_prepared_context`.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use futures::StreamExt as _;
use futures::stream::FuturesUnordered;

use zeph_llm::provider::{Message, MessageMetadata, MessagePart, Role};
use zeph_memory::TokenCounter;

use super::super::error::AgentError;
use super::super::{
    CORRECTIONS_PREFIX, CROSS_SESSION_PREFIX, DOCUMENT_RAG_PREFIX, GRAPH_FACTS_PREFIX, MemoryState,
    RECALL_PREFIX, SUMMARY_PREFIX,
};
use super::ContextSlot;
use crate::agent::context_manager::ContextManager;
use crate::agent::learning_engine::LearningEngine;
use crate::agent::state::{IndexState, SkillState};
use crate::redact::scrub_content;

/// All borrowed fields needed to assemble context for one agent turn.
///
/// All fields are shared references — `ContextAssembler::gather` never mutates state.
pub(crate) struct ContextAssemblyInput<'a> {
    pub memory_state: &'a MemoryState,
    pub context_manager: &'a ContextManager,
    pub token_counter: &'a Arc<TokenCounter>,
    pub skill_state: &'a SkillState,
    pub index: &'a IndexState,
    pub learning_engine: &'a LearningEngine,
    /// Current value of `Agent::sidequest.turn_counter`, for adaptive strategy selection.
    pub sidequest_turn_counter: u64,
    /// Message window snapshot used for strategy resolution and system-prompt extraction.
    pub messages: &'a [Message],
    /// The user query for the current turn, used as the search query for all memory lookups.
    pub query: &'a str,
}

/// Result of one context-assembly pass.
///
/// All source fields are `Option` — `None` means disabled, empty, or budget-exhausted.
/// `session_digest` is excluded: it is a cached value injected by `Agent::apply_prepared_context`.
pub(crate) struct PreparedContext {
    pub graph_facts: Option<Message>,
    pub doc_rag: Option<Message>,
    pub corrections: Option<Message>,
    pub recall: Option<Message>,
    pub recall_confidence: Option<f32>,
    pub cross_session: Option<Message>,
    pub summaries: Option<Message>,
    pub code_context: Option<String>,
    pub persona_facts: Option<Message>,
    pub trajectory_hints: Option<Message>,
    pub tree_memory: Option<Message>,
    /// Whether the memory-first context strategy is active for this turn.
    pub memory_first: bool,
    /// Token budget for recent conversation history (passed to trim step in apply).
    pub recent_history_budget: usize,
}

/// Stateless coordinator for parallel context fetching.
pub(crate) struct ContextAssembler;

impl ContextAssembler {
    /// Gather all context sources concurrently and return a [`PreparedContext`].
    ///
    /// Returns an empty `PreparedContext` immediately when `context_manager.budget` is `None`.
    ///
    /// # Errors
    ///
    /// Propagates errors from any async fetch operation.
    #[allow(clippy::too_many_lines)] // parallel context gathering: memory, graph, skills — coupled async fanout
    pub(crate) async fn gather(
        input: &ContextAssemblyInput<'_>,
    ) -> Result<PreparedContext, AgentError> {
        type CtxFuture<'a> =
            Pin<Box<dyn Future<Output = Result<ContextSlot, AgentError>> + Send + 'a>>;

        let Some(ref budget) = input.context_manager.budget else {
            return Ok(PreparedContext {
                graph_facts: None,
                doc_rag: None,
                corrections: None,
                recall: None,
                recall_confidence: None,
                cross_session: None,
                summaries: None,
                code_context: None,
                persona_facts: None,
                trajectory_hints: None,
                tree_memory: None,
                memory_first: false,
                recent_history_budget: 0,
            });
        };

        let memory_state = input.memory_state;
        let tc = input.token_counter.clone();

        let effective_strategy = match memory_state.compaction.context_strategy {
            crate::config::ContextStrategy::FullHistory => {
                crate::config::ContextStrategy::FullHistory
            }
            crate::config::ContextStrategy::MemoryFirst => {
                crate::config::ContextStrategy::MemoryFirst
            }
            crate::config::ContextStrategy::Adaptive => {
                if input.sidequest_turn_counter
                    >= u64::from(memory_state.compaction.crossover_turn_threshold)
                {
                    crate::config::ContextStrategy::MemoryFirst
                } else {
                    crate::config::ContextStrategy::FullHistory
                }
            }
        };
        let memory_first = effective_strategy == crate::config::ContextStrategy::MemoryFirst;

        let system_prompt = input
            .messages
            .first()
            .filter(|m| m.role == Role::System)
            .map_or("", |m| m.content.as_str());

        let digest_tokens = memory_state
            .compaction
            .cached_session_digest
            .as_ref()
            .map_or(0, |(_, tokens)| *tokens);

        let graph_enabled = memory_state.extraction.graph_config.enabled;

        let alloc = budget.allocate_with_opts(
            system_prompt,
            &input.skill_state.last_skills_prompt,
            &tc,
            graph_enabled,
            digest_tokens,
            memory_first,
        );

        let correction_params = input
            .learning_engine
            .config
            .as_ref()
            .filter(|c| c.correction_detection)
            .map(|c| {
                (
                    c.correction_recall_limit as usize,
                    c.correction_min_similarity,
                )
            });
        let (recall_limit, min_sim) = correction_params.unwrap_or((3, 0.75));

        let router = input.context_manager.build_router();
        let router_ref: &dyn zeph_memory::AsyncMemoryRouter = router.as_ref();
        let query = input.query;

        let mut fetchers: FuturesUnordered<CtxFuture<'_>> = FuturesUnordered::new();

        tracing::debug!(
            active_sources = alloc.active_sources(),
            "context budget allocated"
        );

        if alloc.summaries > 0 {
            fetchers.push(Box::pin(async {
                fetch_summaries(memory_state, alloc.summaries, &tc)
                    .await
                    .map(ContextSlot::Summaries)
            }));
        }
        if alloc.cross_session > 0 {
            fetchers.push(Box::pin(async {
                fetch_cross_session(memory_state, query, alloc.cross_session, &tc)
                    .await
                    .map(ContextSlot::CrossSession)
            }));
        }
        if alloc.semantic_recall > 0 {
            fetchers.push(Box::pin(async {
                fetch_semantic_recall(
                    memory_state,
                    query,
                    alloc.semantic_recall,
                    &tc,
                    Some(router_ref),
                )
                .await
                .map(|(msg, score)| ContextSlot::SemanticRecall(msg, score))
            }));
            fetchers.push(Box::pin(async {
                fetch_document_rag(memory_state, query, alloc.semantic_recall, &tc)
                    .await
                    .map(ContextSlot::DocumentRag)
            }));
        }
        // Corrections are safety-critical and never budget-gated.
        fetchers.push(Box::pin(async {
            fetch_corrections(memory_state, query, recall_limit, min_sim)
                .await
                .map(ContextSlot::Corrections)
        }));
        if alloc.code_context > 0 {
            let index = input.index;
            fetchers.push(Box::pin(async {
                index
                    .fetch_code_rag(query, alloc.code_context)
                    .await
                    .map(ContextSlot::CodeContext)
            }));
        }
        if alloc.graph_facts > 0 {
            fetchers.push(Box::pin(async {
                fetch_graph_facts(memory_state, query, alloc.graph_facts, &tc)
                    .await
                    .map(ContextSlot::GraphFacts)
            }));
        }
        if memory_state.extraction.persona_config.context_budget_tokens > 0 {
            fetchers.push(Box::pin(async {
                let persona_budget = memory_state.extraction.persona_config.context_budget_tokens;
                fetch_persona_facts(memory_state, persona_budget, &tc)
                    .await
                    .map(ContextSlot::PersonaFacts)
            }));
        }
        if memory_state
            .extraction
            .trajectory_config
            .context_budget_tokens
            > 0
        {
            fetchers.push(Box::pin(async {
                let tbudget = memory_state
                    .extraction
                    .trajectory_config
                    .context_budget_tokens;
                fetch_trajectory_hints(memory_state, tbudget, &tc)
                    .await
                    .map(ContextSlot::TrajectoryHints)
            }));
        }
        if memory_state.subsystems.tree_config.context_budget_tokens > 0 {
            fetchers.push(Box::pin(async {
                let tbudget = memory_state.subsystems.tree_config.context_budget_tokens;
                fetch_tree_memory(memory_state, tbudget, &tc)
                    .await
                    .map(ContextSlot::TreeMemory)
            }));
        }

        let mut prepared = PreparedContext {
            graph_facts: None,
            doc_rag: None,
            corrections: None,
            recall: None,
            recall_confidence: None,
            cross_session: None,
            summaries: None,
            code_context: None,
            persona_facts: None,
            trajectory_hints: None,
            tree_memory: None,
            memory_first,
            recent_history_budget: alloc.recent_history,
        };

        while let Some(result) = fetchers.next().await {
            match result {
                Ok(slot) => match slot {
                    ContextSlot::Summaries(msg) => prepared.summaries = msg,
                    ContextSlot::CrossSession(msg) => prepared.cross_session = msg,
                    ContextSlot::SemanticRecall(msg, score) => {
                        prepared.recall = msg;
                        prepared.recall_confidence = score;
                    }
                    ContextSlot::DocumentRag(msg) => prepared.doc_rag = msg,
                    ContextSlot::Corrections(msg) => prepared.corrections = msg,
                    ContextSlot::CodeContext(text) => prepared.code_context = text,
                    ContextSlot::GraphFacts(msg) => prepared.graph_facts = msg,
                    ContextSlot::PersonaFacts(msg) => prepared.persona_facts = msg,
                    ContextSlot::TrajectoryHints(msg) => prepared.trajectory_hints = msg,
                    ContextSlot::TreeMemory(msg) => prepared.tree_memory = msg,
                },
                Err(e) => return Err(e),
            }
        }

        Ok(prepared)
    }
}

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

pub(super) async fn fetch_persona_facts(
    memory_state: &MemoryState,
    budget_tokens: usize,
    tc: &TokenCounter,
) -> Result<Option<Message>, AgentError> {
    if budget_tokens == 0 || !memory_state.extraction.persona_config.enabled {
        return Ok(None);
    }
    let Some(ref memory) = memory_state.persistence.memory else {
        return Ok(None);
    };

    let min_confidence = memory_state.extraction.persona_config.min_confidence;
    let facts = memory.sqlite().load_persona_facts(min_confidence).await?;

    if facts.is_empty() {
        return Ok(None);
    }

    let mut body = String::from(super::PERSONA_PREFIX);
    let mut tokens_so_far = tc.count_tokens(&body);

    for fact in &facts {
        let line = format!("[{}] {}\n", fact.category, fact.content);
        let line_tokens = tc.count_tokens(&line);
        if tokens_so_far + line_tokens > budget_tokens {
            break;
        }
        body.push_str(&line);
        tokens_so_far += line_tokens;
    }

    if body == super::PERSONA_PREFIX {
        return Ok(None);
    }

    Ok(Some(Message::from_legacy(Role::System, body)))
}

pub(super) async fn fetch_trajectory_hints(
    memory_state: &MemoryState,
    budget_tokens: usize,
    tc: &TokenCounter,
) -> Result<Option<Message>, AgentError> {
    if budget_tokens == 0 || !memory_state.extraction.trajectory_config.enabled {
        return Ok(None);
    }
    let Some(ref memory) = memory_state.persistence.memory else {
        return Ok(None);
    };

    let top_k = memory_state.extraction.trajectory_config.recall_top_k;
    let min_conf = memory_state.extraction.trajectory_config.min_confidence;
    let entries = memory
        .sqlite()
        .load_trajectory_entries(Some("procedural"), top_k)
        .await?;

    if entries.is_empty() {
        return Ok(None);
    }

    let mut body = String::from(super::TRAJECTORY_PREFIX);
    let mut tokens_so_far = tc.count_tokens(&body);

    for entry in entries
        .iter()
        .filter(|e| e.confidence >= min_conf)
        .take(top_k)
    {
        let line = format!("- {}: {}\n", entry.intent, entry.outcome);
        let line_tokens = tc.count_tokens(&line);
        if tokens_so_far + line_tokens > budget_tokens {
            break;
        }
        body.push_str(&line);
        tokens_so_far += line_tokens;
    }

    if body == super::TRAJECTORY_PREFIX {
        return Ok(None);
    }

    Ok(Some(Message::from_legacy(Role::System, body)))
}

pub(super) async fn fetch_tree_memory(
    memory_state: &MemoryState,
    budget_tokens: usize,
    tc: &TokenCounter,
) -> Result<Option<Message>, AgentError> {
    if budget_tokens == 0 || !memory_state.subsystems.tree_config.enabled {
        return Ok(None);
    }
    let Some(ref memory) = memory_state.persistence.memory else {
        return Ok(None);
    };

    let top_k = memory_state.subsystems.tree_config.recall_top_k;
    let nodes = memory.sqlite().load_tree_level(1, top_k).await?;

    if nodes.is_empty() {
        return Ok(None);
    }

    let mut body = String::from(super::TREE_MEMORY_PREFIX);
    let mut tokens_so_far = tc.count_tokens(&body);

    for node in nodes.iter().take(top_k) {
        let line = format!("- {}\n", node.content);
        let line_tokens = tc.count_tokens(&line);
        if tokens_so_far + line_tokens > budget_tokens {
            break;
        }
        body.push_str(&line);
        tokens_so_far += line_tokens;
    }

    if body == super::TREE_MEMORY_PREFIX {
        return Ok(None);
    }

    Ok(Some(Message::from_legacy(Role::System, body)))
}

pub(super) fn format_correction_note(_original_output: &str, correction_text: &str) -> String {
    // Never replay the faulty assistant/tool output itself into future prompts.
    format!(
        "- Past user correction: \"{}\"",
        super::truncate_chars(&scrub_content(correction_text), 200)
    )
}

pub(super) async fn fetch_corrections(
    memory_state: &MemoryState,
    query: &str,
    limit: usize,
    min_score: f32,
) -> Result<Option<Message>, AgentError> {
    let Some(ref memory) = memory_state.persistence.memory else {
        return Ok(None);
    };
    let corrections = memory
        .retrieve_similar_corrections(query, limit, min_score)
        .await
        .unwrap_or_default();
    if corrections.is_empty() {
        return Ok(None);
    }
    let mut text = String::from(CORRECTIONS_PREFIX);
    for c in &corrections {
        text.push_str(&format_correction_note(
            &c.original_output,
            &c.correction_text,
        ));
        text.push('\n');
    }
    Ok(Some(Message::from_legacy(Role::System, text)))
}

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

    let mut recall_text = String::with_capacity(token_budget * 3);
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

pub(super) async fn fetch_document_rag(
    memory_state: &MemoryState,
    query: &str,
    token_budget: usize,
    tc: &TokenCounter,
) -> Result<Option<Message>, AgentError> {
    if !memory_state.extraction.document_config.rag_enabled || token_budget == 0 {
        return Ok(None);
    }
    let Some(memory) = &memory_state.persistence.memory else {
        return Ok(None);
    };

    let collection = &memory_state.extraction.document_config.collection;
    let top_k = memory_state.extraction.document_config.top_k;
    let points = memory
        .search_document_collection(collection, query, top_k)
        .await?;
    if points.is_empty() {
        return Ok(None);
    }

    let mut text = String::from(DOCUMENT_RAG_PREFIX);
    let mut tokens_used = tc.count_tokens(&text);

    for point in &points {
        let chunk = point
            .payload
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        if chunk.is_empty() {
            continue;
        }
        let entry = format!("{chunk}\n");
        let cost = tc.count_tokens(&entry);
        if tokens_used + cost > token_budget {
            break;
        }
        text.push_str(&entry);
        tokens_used += cost;
    }

    if tokens_used > tc.count_tokens(DOCUMENT_RAG_PREFIX) {
        Ok(Some(Message {
            role: Role::System,
            content: text,
            parts: vec![],
            metadata: MessageMetadata::default(),
        }))
    } else {
        Ok(None)
    }
}

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
