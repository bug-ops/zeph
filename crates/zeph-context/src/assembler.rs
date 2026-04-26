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

use futures::StreamExt as _;
use futures::stream::FuturesUnordered;

use zeph_llm::provider::{Message, MessageMetadata, MessagePart, Role};
use zeph_memory::TokenCounter;

use crate::error::ContextError;
use crate::input::ContextAssemblyInput;
use crate::slot::ContextSlot;

/// Prefix for past-session summary injections.
pub const SUMMARY_PREFIX: &str = "[conversation summaries]\n";
/// Prefix for cross-session context injections.
pub const CROSS_SESSION_PREFIX: &str = "[cross-session context]\n";
/// Prefix for semantic recall injections.
pub const RECALL_PREFIX: &str = "[semantic recall]\n";
/// Prefix for past-correction injections.
pub const CORRECTIONS_PREFIX: &str = "[past corrections]\n";
/// Prefix for document RAG injections.
pub const DOCUMENT_RAG_PREFIX: &str = "## Relevant documents\n";
/// Prefix for knowledge graph fact injections.
pub const GRAPH_FACTS_PREFIX: &str = "[known facts]\n";

/// Result of one context-assembly pass.
///
/// All source fields are `Option` — `None` means disabled, empty, or budget-exhausted.
/// `session_digest` is excluded: it is a cached value injected by `Agent::apply_prepared_context`.
pub struct PreparedContext {
    /// Knowledge graph fact recall.
    pub graph_facts: Option<Message>,
    /// Document RAG context.
    pub doc_rag: Option<Message>,
    /// Past user corrections.
    pub corrections: Option<Message>,
    /// Semantic recall results.
    pub recall: Option<Message>,
    /// Top-1 similarity score from semantic recall.
    pub recall_confidence: Option<f32>,
    /// Cross-session memory context.
    pub cross_session: Option<Message>,
    /// Past-conversation summaries.
    pub summaries: Option<Message>,
    /// Code-index RAG context (repo map or file context).
    pub code_context: Option<String>,
    /// Persona memory facts.
    pub persona_facts: Option<Message>,
    /// Trajectory hints.
    pub trajectory_hints: Option<Message>,
    /// `TiMem` tree memory summary.
    pub tree_memory: Option<Message>,
    /// Distilled reasoning strategies from the `ReasoningBank` (#3343).
    pub reasoning_hints: Option<Message>,
    /// Whether the memory-first context strategy is active for this turn.
    pub memory_first: bool,
    /// Token budget for recent conversation history (passed to trim step in apply).
    pub recent_history_budget: usize,
}

/// Stateless coordinator for parallel context fetching.
///
/// All logic is in [`ContextAssembler::gather`]. No state is stored on this type.
pub struct ContextAssembler;

impl ContextAssembler {
    /// Gather all context sources concurrently and return a [`PreparedContext`].
    ///
    /// Returns an empty `PreparedContext` immediately when `context_manager.budget` is `None`.
    ///
    /// # Errors
    ///
    /// Propagates errors from any async fetch operation.
    #[allow(clippy::too_many_lines)] // long function; decomposition would require extracting state into additional structs — TODO(#3442): decompose into smaller helpers
    pub async fn gather(input: &ContextAssemblyInput<'_>) -> Result<PreparedContext, ContextError> {
        type CtxFuture<'a> =
            Pin<Box<dyn Future<Output = Result<ContextSlot, ContextError>> + Send + 'a>>;

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
                reasoning_hints: None,
                memory_first: false,
                recent_history_budget: 0,
            });
        };

        let memory = input.memory;
        let tc = input.token_counter;

        let effective_strategy = match memory.context_strategy {
            zeph_config::ContextStrategy::FullHistory => zeph_config::ContextStrategy::FullHistory,
            zeph_config::ContextStrategy::MemoryFirst => zeph_config::ContextStrategy::MemoryFirst,
            zeph_config::ContextStrategy::Adaptive => {
                if input.sidequest_turn_counter >= u64::from(memory.crossover_turn_threshold) {
                    zeph_config::ContextStrategy::MemoryFirst
                } else {
                    zeph_config::ContextStrategy::FullHistory
                }
            }
        };
        let memory_first = effective_strategy == zeph_config::ContextStrategy::MemoryFirst;

        let system_prompt = input
            .messages
            .first()
            .filter(|m| m.role == Role::System)
            .map_or("", |m| m.content.as_str());

        let digest_tokens = memory
            .cached_session_digest
            .as_ref()
            .map_or(0, |(_, tokens)| *tokens);

        let graph_enabled = memory.graph_config.enabled;

        let alloc = budget.allocate_with_opts(
            system_prompt,
            input.skills_prompt,
            tc,
            graph_enabled,
            digest_tokens,
            memory_first,
        );

        let correction_params = input
            .correction_config
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
        let scrub = input.scrub;

        let mut fetchers: FuturesUnordered<CtxFuture<'_>> = FuturesUnordered::new();

        tracing::debug!(
            active_sources = alloc.active_sources(),
            "context budget allocated"
        );

        if alloc.summaries > 0 {
            fetchers.push(Box::pin(async {
                fetch_summaries(memory, alloc.summaries, tc)
                    .await
                    .map(ContextSlot::Summaries)
            }));
        }
        if alloc.cross_session > 0 {
            fetchers.push(Box::pin(async {
                fetch_cross_session(memory, query, alloc.cross_session, tc)
                    .await
                    .map(ContextSlot::CrossSession)
            }));
        }
        if alloc.semantic_recall > 0 {
            fetchers.push(Box::pin(async {
                fetch_semantic_recall(memory, query, alloc.semantic_recall, tc, Some(router_ref))
                    .await
                    .map(|(msg, score)| ContextSlot::SemanticRecall(msg, score))
            }));
            fetchers.push(Box::pin(async {
                fetch_document_rag(memory, query, alloc.semantic_recall, tc)
                    .await
                    .map(ContextSlot::DocumentRag)
            }));
        }
        // Corrections are safety-critical and never budget-gated.
        fetchers.push(Box::pin(async {
            fetch_corrections(memory, query, recall_limit, min_sim, scrub)
                .await
                .map(ContextSlot::Corrections)
        }));
        if alloc.code_context > 0
            && let Some(index) = input.index
        {
            let budget = alloc.code_context;
            fetchers.push(Box::pin(async move {
                let result: Result<Option<String>, ContextError> =
                    index.fetch_code_rag(query, budget).await;
                result.map(ContextSlot::CodeContext)
            }));
        }
        if alloc.graph_facts > 0 {
            fetchers.push(Box::pin(async {
                fetch_graph_facts(memory, query, alloc.graph_facts, tc)
                    .await
                    .map(ContextSlot::GraphFacts)
            }));
        }
        if memory.persona_config.context_budget_tokens > 0 {
            fetchers.push(Box::pin(async {
                let persona_budget = memory.persona_config.context_budget_tokens;
                fetch_persona_facts(memory, persona_budget, tc)
                    .await
                    .map(ContextSlot::PersonaFacts)
            }));
        }
        if memory.trajectory_config.context_budget_tokens > 0 {
            fetchers.push(Box::pin(async {
                let tbudget = memory.trajectory_config.context_budget_tokens;
                fetch_trajectory_hints(memory, tbudget, tc)
                    .await
                    .map(ContextSlot::TrajectoryHints)
            }));
        }
        if memory.tree_config.context_budget_tokens > 0 {
            fetchers.push(Box::pin(async {
                let tbudget = memory.tree_config.context_budget_tokens;
                fetch_tree_memory(memory, tbudget, tc)
                    .await
                    .map(ContextSlot::TreeMemory)
            }));
        }
        if memory.reasoning_config.enabled && memory.reasoning_config.context_budget_tokens > 0 {
            fetchers.push(Box::pin(async {
                let rbudget = memory.reasoning_config.context_budget_tokens;
                let top_k = memory.reasoning_config.top_k;
                fetch_reasoning_strategies(memory, query, rbudget, top_k, tc)
                    .await
                    .map(ContextSlot::ReasoningStrategies)
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
            reasoning_hints: None,
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
                    ContextSlot::ReasoningStrategies(msg) => prepared.reasoning_hints = msg,
                },
                Err(e) => return Err(e),
            }
        }

        Ok(prepared)
    }
}

/// Clamp recall timeout to a safe minimum.
///
/// A configured value of 0 would disable spreading activation recall entirely;
/// clamping to 100ms preserves the user's intent while preventing a silent no-op.
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

use crate::input::ContextMemoryView;

pub(crate) async fn fetch_graph_facts(
    memory: &ContextMemoryView,
    query: &str,
    budget_tokens: usize,
    tc: &TokenCounter,
) -> Result<Option<Message>, ContextError> {
    if budget_tokens == 0 || !memory.graph_config.enabled {
        return Ok(None);
    }
    let Some(ref mem) = memory.memory else {
        return Ok(None);
    };
    let recall_limit = memory.graph_config.recall_limit;
    let temporal_decay_rate = memory.graph_config.temporal_decay_rate;
    let edge_types = zeph_memory::classify_graph_subgraph(query);
    let sa_config = &memory.graph_config.spreading_activation;

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
        let recall_fut = mem.recall_graph_activated(query, recall_limit, sa_params, &edge_types);
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
        let max_hops = memory.graph_config.max_hops;
        let facts = mem
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
                ContextError::Memory(e)
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

pub(crate) async fn fetch_persona_facts(
    memory: &ContextMemoryView,
    budget_tokens: usize,
    tc: &TokenCounter,
) -> Result<Option<Message>, ContextError> {
    if budget_tokens == 0 || !memory.persona_config.enabled {
        return Ok(None);
    }
    let Some(ref mem) = memory.memory else {
        return Ok(None);
    };

    let min_confidence = memory.persona_config.min_confidence;
    let facts = mem.sqlite().load_persona_facts(min_confidence).await?;

    if facts.is_empty() {
        return Ok(None);
    }

    let mut body = String::from(crate::slot::PERSONA_PREFIX);
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

    if body == crate::slot::PERSONA_PREFIX {
        return Ok(None);
    }

    Ok(Some(Message::from_legacy(Role::System, body)))
}

pub(crate) async fn fetch_trajectory_hints(
    memory: &ContextMemoryView,
    budget_tokens: usize,
    tc: &TokenCounter,
) -> Result<Option<Message>, ContextError> {
    if budget_tokens == 0 || !memory.trajectory_config.enabled {
        return Ok(None);
    }
    let Some(ref mem) = memory.memory else {
        return Ok(None);
    };

    let top_k = memory.trajectory_config.recall_top_k;
    let min_conf = memory.trajectory_config.min_confidence;
    let entries = mem
        .sqlite()
        .load_trajectory_entries(Some("procedural"), top_k)
        .await?;

    if entries.is_empty() {
        return Ok(None);
    }

    let mut body = String::from(crate::slot::TRAJECTORY_PREFIX);
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

    if body == crate::slot::TRAJECTORY_PREFIX {
        return Ok(None);
    }

    Ok(Some(Message::from_legacy(Role::System, body)))
}

pub(crate) async fn fetch_tree_memory(
    memory: &ContextMemoryView,
    budget_tokens: usize,
    tc: &TokenCounter,
) -> Result<Option<Message>, ContextError> {
    if budget_tokens == 0 || !memory.tree_config.enabled {
        return Ok(None);
    }
    let Some(ref mem) = memory.memory else {
        return Ok(None);
    };

    let top_k = memory.tree_config.recall_top_k;
    let nodes = mem.sqlite().load_tree_level(1, top_k).await?;

    if nodes.is_empty() {
        return Ok(None);
    }

    let mut body = String::from(crate::slot::TREE_MEMORY_PREFIX);
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

    if body == crate::slot::TREE_MEMORY_PREFIX {
        return Ok(None);
    }

    Ok(Some(Message::from_legacy(Role::System, body)))
}

pub(crate) async fn fetch_reasoning_strategies(
    memory: &ContextMemoryView,
    query: &str,
    budget_tokens: usize,
    top_k: usize,
    tc: &TokenCounter,
) -> Result<Option<Message>, ContextError> {
    // S1: enforce the ≤500-token spec cap documented in ReasoningConfig.
    let budget_tokens = budget_tokens.min(500);
    if budget_tokens == 0 {
        return Ok(None);
    }
    let Some(ref mem) = memory.memory else {
        return Ok(None);
    };

    let strategies = mem
        .retrieve_reasoning_strategies(query, top_k)
        .await
        .map_err(ContextError::Memory)?;

    if strategies.is_empty() {
        return Ok(None);
    }

    let mut body = String::from(crate::slot::REASONING_PREFIX);
    let mut tokens_so_far = tc.count_tokens(&body);
    let mut injected_ids: Vec<String> = Vec::new();

    for s in strategies.iter().take(top_k) {
        // S-Med1: sanitize distilled summaries to prevent stored injection payloads
        // from reaching the system prompt (mirrors fetch_graph_facts scrub pattern).
        let safe_summary = s.summary.replace(['\n', '\r', '<', '>'], " ");
        let line = format!("- [{}] {}\n", s.outcome.as_str(), safe_summary);
        let line_tokens = tc.count_tokens(&line);
        if tokens_so_far + line_tokens > budget_tokens {
            break;
        }
        body.push_str(&line);
        tokens_so_far += line_tokens;
        injected_ids.push(s.id.clone());
    }

    if body == crate::slot::REASONING_PREFIX {
        return Ok(None);
    }

    // C4 split: mark_used only for strategies that made it past budget truncation.
    // P2-1: fire-and-forget — mark_used does not need to block the context build path.
    if let Some(ref reasoning) = mem.reasoning {
        let reasoning = reasoning.clone();
        tokio::spawn(async move {
            if let Err(e) = reasoning.mark_used(&injected_ids).await {
                tracing::warn!(error = %e, "reasoning: mark_used failed");
            }
        });
    }

    Ok(Some(Message::from_legacy(Role::System, body)))
}

pub(crate) async fn fetch_corrections(
    memory: &ContextMemoryView,
    query: &str,
    limit: usize,
    min_score: f32,
    scrub: fn(&str) -> std::borrow::Cow<'_, str>,
) -> Result<Option<Message>, ContextError> {
    let Some(ref mem) = memory.memory else {
        return Ok(None);
    };
    let corrections = mem
        .retrieve_similar_corrections(query, limit, min_score)
        .await
        .unwrap_or_default();
    if corrections.is_empty() {
        return Ok(None);
    }
    let mut text = String::from(CORRECTIONS_PREFIX);
    for c in &corrections {
        text.push_str("- Past user correction: \"");
        text.push_str(&scrub(&c.correction_text));
        text.push_str("\"\n");
    }
    Ok(Some(Message::from_legacy(Role::System, text)))
}

pub(crate) async fn fetch_semantic_recall(
    memory: &ContextMemoryView,
    query: &str,
    token_budget: usize,
    tc: &TokenCounter,
    router: Option<&dyn zeph_memory::AsyncMemoryRouter>,
) -> Result<(Option<Message>, Option<f32>), ContextError> {
    let Some(ref mem) = memory.memory else {
        return Ok((None, None));
    };
    if memory.recall_limit == 0 || token_budget == 0 {
        return Ok((None, None));
    }

    let recalled = if let Some(r) = router {
        mem.recall_routed_async(query, memory.recall_limit, None, r)
            .await?
    } else {
        mem.recall(query, memory.recall_limit, None).await?
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

pub(crate) async fn fetch_document_rag(
    memory: &ContextMemoryView,
    query: &str,
    token_budget: usize,
    tc: &TokenCounter,
) -> Result<Option<Message>, ContextError> {
    if !memory.document_config.rag_enabled || token_budget == 0 {
        return Ok(None);
    }
    let Some(ref mem) = memory.memory else {
        return Ok(None);
    };

    let collection = &memory.document_config.collection;
    let top_k = memory.document_config.top_k;
    let points = mem
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

pub(crate) async fn fetch_summaries(
    memory: &ContextMemoryView,
    token_budget: usize,
    tc: &TokenCounter,
) -> Result<Option<Message>, ContextError> {
    let (Some(mem), Some(cid)) = (&memory.memory, memory.conversation_id) else {
        return Ok(None);
    };
    if token_budget == 0 {
        return Ok(None);
    }

    let summaries = mem.load_summaries(cid).await?;
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

pub(crate) async fn fetch_cross_session(
    memory: &ContextMemoryView,
    query: &str,
    token_budget: usize,
    tc: &TokenCounter,
) -> Result<Option<Message>, ContextError> {
    let (Some(mem), Some(cid)) = (&memory.memory, memory.conversation_id) else {
        return Ok(None);
    };
    if token_budget == 0 {
        return Ok(None);
    }

    let threshold = memory.cross_session_score_threshold;
    let results: Vec<_> = mem
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

/// Maximum number of messages scanned backward by [`memory_first_keep_tail`] before
/// stopping at the next non-`ToolResult` boundary, to avoid O(N) scans on long sessions.
pub const MAX_KEEP_TAIL_SCAN: usize = 50;

/// Compute how many tail messages to keep when the `MemoryFirst` strategy is active.
///
/// Always keeps at least 2 messages. Extends the tail as long as the boundary message is
/// a `ToolResult` (user message with a `ToolResult` part) to avoid splitting a tool-call
/// round-trip. Capped at `MAX_KEEP_TAIL_SCAN` to prevent O(N) scans on long sessions.
///
/// `history_start` is the index of the first non-system message (typically 1).
#[must_use]
pub fn memory_first_keep_tail(messages: &[Message], history_start: usize) -> usize {
    use zeph_llm::provider::MessagePart;

    let mut keep_tail = 2usize;
    let len = messages.len();
    let max = len.saturating_sub(history_start);

    while keep_tail < max {
        let first_retained = &messages[len - keep_tail];
        let is_tool_result = first_retained.role == Role::User
            && first_retained
                .parts
                .iter()
                .any(|p| matches!(p, MessagePart::ToolResult { .. }));

        if is_tool_result {
            keep_tail += 1;
        } else {
            break;
        }

        if keep_tail >= MAX_KEEP_TAIL_SCAN {
            let preceding_idx = len.saturating_sub(keep_tail + 1);
            if preceding_idx >= history_start {
                let preceding = &messages[preceding_idx];
                let is_tool_use = preceding.role == Role::Assistant
                    && preceding
                        .parts
                        .iter()
                        .any(|p| matches!(p, MessagePart::ToolUse { .. }));
                if is_tool_use {
                    keep_tail += 1;
                }
            }
            break;
        }
    }

    keep_tail
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::ContextMemoryView;
    use zeph_config::{
        ContextStrategy, DocumentConfig, GraphConfig, PersonaConfig, ReasoningConfig,
        TrajectoryConfig, TreeConfig,
    };
    use zeph_memory::TokenCounter;

    fn empty_view() -> ContextMemoryView {
        ContextMemoryView {
            memory: None,
            conversation_id: None,
            recall_limit: 10,
            cross_session_score_threshold: 0.5,
            context_strategy: ContextStrategy::default(),
            crossover_turn_threshold: 5,
            cached_session_digest: None,
            graph_config: GraphConfig::default(),
            document_config: DocumentConfig::default(),
            persona_config: PersonaConfig::default(),
            trajectory_config: TrajectoryConfig::default(),
            reasoning_config: ReasoningConfig::default(),
            tree_config: TreeConfig::default(),
        }
    }

    // ── fetch_graph_facts ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn fetch_graph_facts_returns_none_when_memory_is_none() {
        let view = empty_view();
        let tc = TokenCounter::new();
        let result = fetch_graph_facts(&view, "test", 1000, &tc).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn fetch_graph_facts_returns_none_when_budget_zero() {
        let mut view = empty_view();
        view.graph_config.enabled = true;
        let tc = TokenCounter::new();
        let result = fetch_graph_facts(&view, "test", 0, &tc).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn fetch_graph_facts_returns_none_when_graph_disabled() {
        let mut view = empty_view();
        view.graph_config.enabled = false;
        let tc = TokenCounter::new();
        let result = fetch_graph_facts(&view, "test", 1000, &tc).await.unwrap();
        assert!(result.is_none());
    }

    // ── fetch_persona_facts ───────────────────────────────────────────────────

    #[tokio::test]
    async fn fetch_persona_facts_returns_none_when_memory_is_none() {
        let view = empty_view();
        let tc = TokenCounter::new();
        let result = fetch_persona_facts(&view, 1000, &tc).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn fetch_persona_facts_returns_none_when_budget_zero() {
        let mut view = empty_view();
        view.persona_config.enabled = true;
        let tc = TokenCounter::new();
        let result = fetch_persona_facts(&view, 0, &tc).await.unwrap();
        assert!(result.is_none());
    }

    // ── fetch_trajectory_hints ────────────────────────────────────────────────

    #[tokio::test]
    async fn fetch_trajectory_hints_returns_none_when_memory_is_none() {
        let view = empty_view();
        let tc = TokenCounter::new();
        let result = fetch_trajectory_hints(&view, 1000, &tc).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn fetch_trajectory_hints_returns_none_when_budget_zero() {
        let mut view = empty_view();
        view.trajectory_config.enabled = true;
        let tc = TokenCounter::new();
        let result = fetch_trajectory_hints(&view, 0, &tc).await.unwrap();
        assert!(result.is_none());
    }

    // ── fetch_tree_memory ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn fetch_tree_memory_returns_none_when_memory_is_none() {
        let view = empty_view();
        let tc = TokenCounter::new();
        let result = fetch_tree_memory(&view, 1000, &tc).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn fetch_tree_memory_returns_none_when_budget_zero() {
        let mut view = empty_view();
        view.tree_config.enabled = true;
        let tc = TokenCounter::new();
        let result = fetch_tree_memory(&view, 0, &tc).await.unwrap();
        assert!(result.is_none());
    }

    // ── fetch_corrections ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn fetch_corrections_returns_none_when_memory_is_none() {
        let view = empty_view();
        let result = fetch_corrections(&view, "test", 10, 0.5, |s| s.into())
            .await
            .unwrap();
        assert!(result.is_none());
    }

    // ── fetch_semantic_recall ─────────────────────────────────────────────────

    #[tokio::test]
    async fn fetch_semantic_recall_returns_none_when_memory_is_none() {
        let view = empty_view();
        let tc = TokenCounter::new();
        let result = fetch_semantic_recall(&view, "test", 1000, &tc, None)
            .await
            .unwrap();
        assert!(result.0.is_none() && result.1.is_none());
    }

    #[tokio::test]
    async fn fetch_semantic_recall_returns_none_when_budget_zero() {
        let view = empty_view();
        let tc = TokenCounter::new();
        let result = fetch_semantic_recall(&view, "test", 0, &tc, None)
            .await
            .unwrap();
        assert!(result.0.is_none() && result.1.is_none());
    }

    // ── fetch_document_rag ────────────────────────────────────────────────────

    #[tokio::test]
    async fn fetch_document_rag_returns_none_when_memory_is_none() {
        let mut view = empty_view();
        view.document_config.rag_enabled = true;
        let tc = TokenCounter::new();
        let result = fetch_document_rag(&view, "test", 1000, &tc).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn fetch_document_rag_returns_none_when_rag_disabled() {
        let view = empty_view();
        let tc = TokenCounter::new();
        let result = fetch_document_rag(&view, "test", 1000, &tc).await.unwrap();
        assert!(result.is_none());
    }

    // ── fetch_summaries ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn fetch_summaries_returns_none_when_memory_is_none() {
        let view = empty_view();
        let tc = TokenCounter::new();
        let result = fetch_summaries(&view, 1000, &tc).await.unwrap();
        assert!(result.is_none());
    }

    // ── fetch_cross_session ───────────────────────────────────────────────────

    #[tokio::test]
    async fn fetch_cross_session_returns_none_when_memory_is_none() {
        let view = empty_view();
        let tc = TokenCounter::new();
        let result = fetch_cross_session(&view, "test", 1000, &tc).await.unwrap();
        assert!(result.is_none());
    }

    // ── fetch_reasoning_strategies ────────────────────────────────────────────

    #[tokio::test]
    async fn fetch_reasoning_strategies_returns_none_when_memory_is_none() {
        let mut view = empty_view();
        view.reasoning_config.enabled = true;
        let tc = TokenCounter::new();
        let result = fetch_reasoning_strategies(&view, "query", 1000, 3, &tc)
            .await
            .unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn fetch_reasoning_strategies_returns_none_when_budget_zero() {
        let mut view = empty_view();
        view.reasoning_config.enabled = true;
        let tc = TokenCounter::new();
        let result = fetch_reasoning_strategies(&view, "query", 0, 3, &tc)
            .await
            .unwrap();
        assert!(result.is_none());
    }
}
