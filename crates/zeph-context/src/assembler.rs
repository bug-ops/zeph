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

use zeph_common::memory::{AsyncMemoryRouter, CompressionLevel, GraphRecallParams, TokenCounting};
use zeph_llm::provider::{Message, MessageMetadata, MessagePart, Role};

use crate::error::ContextError;
use crate::input::ContextAssemblyInput;
use crate::slot::ContextSlot;

/// Map a slice of active compression levels to per-tier boolean flags.
///
/// Returns `(episodic_active, procedural_active, declarative_active)`.
///
/// An empty slice means "no tier filtering": all three flags are `true`. This is the defensive
/// default — passing an empty slice preserves legacy behaviour instead of silently suppressing
/// all memory recall.
pub(crate) fn levels_to_flags(levels: &[CompressionLevel]) -> (bool, bool, bool) {
    if levels.is_empty() {
        return (true, true, true);
    }
    let episodic = levels.contains(&CompressionLevel::Episodic);
    let procedural = levels.contains(&CompressionLevel::Procedural);
    let declarative = levels.contains(&CompressionLevel::Declarative);
    (episodic, procedural, declarative)
}

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

type CtxFuture<'a> = Pin<Box<dyn Future<Output = Result<ContextSlot, ContextError>> + Send + 'a>>;

fn empty_prepared_context() -> PreparedContext {
    PreparedContext {
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
    }
}

// TODO(critic): consider impl Default for PreparedContext to make this constructor obsolete (#3442 follow-up).

fn resolve_effective_strategy(
    memory: &crate::input::ContextMemoryView,
    sidequest_turn_counter: u64,
) -> zeph_config::ContextStrategy {
    match memory.context_strategy {
        zeph_config::ContextStrategy::FullHistory => zeph_config::ContextStrategy::FullHistory,
        zeph_config::ContextStrategy::MemoryFirst => zeph_config::ContextStrategy::MemoryFirst,
        zeph_config::ContextStrategy::Adaptive => {
            if sidequest_turn_counter >= u64::from(memory.crossover_turn_threshold) {
                zeph_config::ContextStrategy::MemoryFirst
            } else {
                zeph_config::ContextStrategy::FullHistory
            }
        }
    }
}

fn correction_params(cfg: Option<&crate::input::CorrectionConfig>) -> (usize, f32) {
    cfg.filter(|c| c.correction_detection)
        .map_or((3, 0.75), |c| {
            (
                c.correction_recall_limit as usize,
                c.correction_min_similarity,
            )
        })
}

/// Schedules all enabled context fetchers and returns them as a set of concurrent futures.
///
/// `router_ref` borrows from `router`, which is a local owned by `gather`. Using a separate
/// lifetime `'r` for `router_ref` avoids tying it to `'a` (the input lifetime), which would
/// require `router` to outlive `input`. All `usize` budget values are passed by copy so the
/// returned futures do not borrow from `alloc`.
#[allow(clippy::too_many_arguments)]
fn schedule_context_fetchers<'r>(
    memory: &'r crate::input::ContextMemoryView,
    tc: &'r dyn TokenCounting,
    query: &'r str,
    scrub: fn(&str) -> std::borrow::Cow<'_, str>,
    index: Option<&'r dyn crate::input::IndexAccess>,
    router_ref: &'r dyn AsyncMemoryRouter,
    summaries_budget: usize,
    cross_session_budget: usize,
    semantic_recall_budget: usize,
    code_context_budget: usize,
    graph_facts_budget: usize,
    recall_limit: usize,
    min_sim: f32,
    active_levels: &[CompressionLevel],
) -> FuturesUnordered<CtxFuture<'r>> {
    // TODO(critic): episodic_active currently gates summaries + cross-session + recall + doc_rag
    // together. If future RetrievalPolicy variants ever drop Episodic, the cheap summary fetchers
    // will be silently disabled — split into raw vs compressed sub-tiers. (#3455 follow-up)
    let (episodic_active, procedural_active, declarative_active) = levels_to_flags(active_levels);

    let fetchers: FuturesUnordered<CtxFuture<'r>> = FuturesUnordered::new();

    if episodic_active && summaries_budget > 0 {
        fetchers.push(Box::pin(async move {
            fetch_summaries(memory, summaries_budget, tc)
                .await
                .map(ContextSlot::Summaries)
        }));
    }
    if episodic_active && cross_session_budget > 0 {
        fetchers.push(Box::pin(async move {
            fetch_cross_session(memory, query, cross_session_budget, tc)
                .await
                .map(ContextSlot::CrossSession)
        }));
    }
    if episodic_active && semantic_recall_budget > 0 {
        fetchers.push(Box::pin(async move {
            fetch_semantic_recall(memory, query, semantic_recall_budget, tc, Some(router_ref))
                .await
                .map(|(msg, score)| ContextSlot::SemanticRecall(msg, score))
        }));
        fetchers.push(Box::pin(async move {
            fetch_document_rag(memory, query, semantic_recall_budget, tc)
                .await
                .map(ContextSlot::DocumentRag)
        }));
    }
    // Corrections are safety-critical and never budget-gated or tier-gated.
    fetchers.push(Box::pin(async move {
        fetch_corrections(memory, query, recall_limit, min_sim, scrub)
            .await
            .map(ContextSlot::Corrections)
    }));
    // Code RAG is request-driven, not memory-tier; exempt from tier filtering.
    if code_context_budget > 0
        && let Some(idx) = index
    {
        fetchers.push(Box::pin(async move {
            let result: Result<Option<String>, ContextError> =
                idx.fetch_code_rag(query, code_context_budget).await;
            result.map(ContextSlot::CodeContext)
        }));
    }
    if declarative_active && graph_facts_budget > 0 {
        fetchers.push(Box::pin(async move {
            fetch_graph_facts(memory, query, graph_facts_budget, tc)
                .await
                .map(ContextSlot::GraphFacts)
        }));
    }
    if declarative_active && memory.persona_config.context_budget_tokens > 0 {
        fetchers.push(Box::pin(async move {
            let persona_budget = memory.persona_config.context_budget_tokens;
            fetch_persona_facts(memory, persona_budget, tc)
                .await
                .map(ContextSlot::PersonaFacts)
        }));
    }
    if procedural_active && memory.trajectory_config.context_budget_tokens > 0 {
        fetchers.push(Box::pin(async move {
            let tbudget = memory.trajectory_config.context_budget_tokens;
            fetch_trajectory_hints(memory, tbudget, tc)
                .await
                .map(ContextSlot::TrajectoryHints)
        }));
    }
    if declarative_active && memory.tree_config.context_budget_tokens > 0 {
        fetchers.push(Box::pin(async move {
            let tbudget = memory.tree_config.context_budget_tokens;
            fetch_tree_memory(memory, tbudget, tc)
                .await
                .map(ContextSlot::TreeMemory)
        }));
    }
    if procedural_active
        && memory.reasoning_config.enabled
        && memory.reasoning_config.context_budget_tokens > 0
    {
        fetchers.push(Box::pin(async move {
            let rbudget = memory.reasoning_config.context_budget_tokens;
            let top_k = memory.reasoning_config.top_k;
            fetch_reasoning_strategies(memory, query, rbudget, top_k, tc)
                .await
                .map(ContextSlot::ReasoningStrategies)
        }));
    }

    fetchers
}

async fn drive_fetchers(
    mut fetchers: FuturesUnordered<CtxFuture<'_>>,
    prepared: &mut PreparedContext,
) -> Result<(), ContextError> {
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
    Ok(())
}

impl ContextAssembler {
    /// Gather all context sources concurrently and return a [`PreparedContext`].
    ///
    /// Returns an empty `PreparedContext` immediately when `context_manager.budget` is `None`.
    ///
    /// # Errors
    ///
    /// Propagates errors from any async fetch operation.
    pub async fn gather(input: &ContextAssemblyInput<'_>) -> Result<PreparedContext, ContextError> {
        let Some(ref budget) = input.context_manager.budget else {
            return Ok(empty_prepared_context());
        };

        let memory = input.memory;
        let tc = input.token_counter;

        let effective_strategy = resolve_effective_strategy(memory, input.sidequest_turn_counter);
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

        let alloc = budget.allocate_with_opts(
            system_prompt,
            input.skills_prompt,
            tc,
            memory.graph_config.enabled,
            digest_tokens,
            memory_first,
        );

        let (recall_limit, min_sim) = correction_params(input.correction_config.as_ref());

        let router_ref: &dyn AsyncMemoryRouter = input.router.as_ref();

        tracing::debug!(
            active_sources = alloc.active_sources(),
            active_levels = ?input.active_levels,
            "context budget allocated"
        );

        let fetchers = schedule_context_fetchers(
            memory,
            tc,
            input.query,
            input.scrub,
            input.index,
            router_ref,
            alloc.summaries,
            alloc.cross_session,
            alloc.semantic_recall,
            alloc.code_context,
            alloc.graph_facts,
            recall_limit,
            min_sim,
            input.active_levels,
        );

        let mut prepared = empty_prepared_context();
        prepared.memory_first = memory_first;
        prepared.recent_history_budget = alloc.recent_history;

        drive_fetchers(fetchers, &mut prepared).await?;
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

#[allow(clippy::too_many_lines)] // single-pass view-aware enrichment pipeline
pub(crate) async fn fetch_graph_facts(
    memory: &ContextMemoryView,
    query: &str,
    budget_tokens: usize,
    tc: &dyn TokenCounting,
) -> Result<Option<Message>, ContextError> {
    use zeph_common::memory::{RecallView, SpreadingActivationParams, classify_graph_subgraph};

    if budget_tokens == 0 || !memory.graph_config.enabled {
        return Ok(None);
    }
    let Some(ref mem) = memory.memory else {
        return Ok(None);
    };
    let recall_limit = memory.graph_config.recall_limit;
    let temporal_decay_rate = memory.graph_config.temporal_decay_rate;
    let sa_config = &memory.graph_config.spreading_activation;

    // Fuse MemCoT semantic state into the recall query (spec §A8: state ≤ 2 × query.len()).
    let fused_query;
    let effective_query = if let Some(ref state) = memory.memcot_state {
        let max_state_chars = 2 * query.len();
        let state_slice = if state.len() > max_state_chars {
            let boundary = state.floor_char_boundary(max_state_chars);
            &state[..boundary]
        } else {
            state.as_str()
        };
        fused_query = format!("[state] {state_slice}\n{query}");
        &fused_query as &str
    } else {
        query
    };

    let edge_types = classify_graph_subgraph(effective_query);

    let view = match memory.memcot_config.recall_view {
        zeph_config::RecallViewConfig::Head => RecallView::Head,
        zeph_config::RecallViewConfig::ZoomIn => RecallView::ZoomIn,
        zeph_config::RecallViewConfig::ZoomOut => RecallView::ZoomOut,
    };

    let sa_params = if sa_config.enabled {
        Some(SpreadingActivationParams {
            decay_lambda: sa_config.decay_lambda,
            max_hops: sa_config.max_hops,
            activation_threshold: sa_config.activation_threshold,
            inhibition_threshold: sa_config.inhibition_threshold,
            max_activated_nodes: sa_config.max_activated_nodes,
            temporal_decay_rate,
            seed_structural_weight: sa_config.seed_structural_weight,
            seed_community_cap: sa_config.seed_community_cap,
        })
    } else {
        None
    };

    let timeout_ms = effective_recall_timeout_ms(sa_config.recall_timeout_ms);
    let recall_fut = mem.recall_graph_facts(
        effective_query,
        GraphRecallParams {
            limit: recall_limit,
            view,
            zoom_out_neighbor_cap: memory.memcot_config.zoom_out_neighbor_cap,
            max_hops: memory.graph_config.max_hops,
            temporal_decay_rate,
            edge_types: &edge_types,
            spreading_activation: sa_params,
        },
    );
    let recalled = match tokio::time::timeout(
        std::time::Duration::from_millis(timeout_ms),
        recall_fut,
    )
    .await
    {
        Ok(Ok(facts)) => facts,
        Ok(Err(e)) => {
            tracing::warn!("graph recall failed: {e:#}");
            Vec::new()
        }
        Err(_) => {
            tracing::warn!("graph recall timed out ({timeout_ms}ms)");
            Vec::new()
        }
    };

    if recalled.is_empty() {
        return Ok(None);
    }

    let mut body = String::from(GRAPH_FACTS_PREFIX);
    let mut tokens_so_far = tc.count_tokens(&body);

    for rf in &recalled {
        let fact_text = rf.fact.replace(['\n', '\r', '<', '>'], " ");
        let line = if let Some(score) = rf.activation_score {
            format!(
                "- {} (confidence: {:.2}, activation: {:.2})\n",
                fact_text, rf.confidence, score
            )
        } else {
            format!("- {} (confidence: {:.2})\n", fact_text, rf.confidence)
        };
        let line_tokens = tc.count_tokens(&line);
        if tokens_so_far + line_tokens > budget_tokens {
            break;
        }
        body.push_str(&line);
        tokens_so_far += line_tokens;

        // Append ZoomOut neighbors after the head fact.
        for nb in &rf.neighbors {
            let nb_text = nb.fact.replace(['\n', '\r', '<', '>'], " ");
            let nb_line = format!("  ~ {} (confidence: {:.2})\n", nb_text, nb.confidence);
            let nb_tokens = tc.count_tokens(&nb_line);
            if tokens_so_far + nb_tokens > budget_tokens {
                break;
            }
            body.push_str(&nb_line);
            tokens_so_far += nb_tokens;
        }

        // Append ZoomIn provenance snippet if present.
        if let Some(ref snippet) = rf.provenance_snippet {
            let snip_line = format!(
                "  [source: {}]\n",
                snippet.replace(['\n', '\r', '<', '>'], " ")
            );
            let snip_tokens = tc.count_tokens(&snip_line);
            if tokens_so_far + snip_tokens <= budget_tokens {
                body.push_str(&snip_line);
                tokens_so_far += snip_tokens;
            }
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
    tc: &dyn TokenCounting,
) -> Result<Option<Message>, ContextError> {
    if budget_tokens == 0 || !memory.persona_config.enabled {
        return Ok(None);
    }
    let Some(ref mem) = memory.memory else {
        return Ok(None);
    };

    let min_confidence = memory.persona_config.min_confidence;
    let facts = mem
        .load_persona_facts(min_confidence)
        .await
        .map_err(ContextError::Memory)?;

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
    tc: &dyn TokenCounting,
) -> Result<Option<Message>, ContextError> {
    if budget_tokens == 0 || !memory.trajectory_config.enabled {
        return Ok(None);
    }
    let Some(ref mem) = memory.memory else {
        return Ok(None);
    };

    let top_k = memory.trajectory_config.recall_top_k;
    let min_conf = memory.trajectory_config.min_confidence;
    // Load procedural trajectory entries via the backend abstraction.
    // The "procedural" filter maps to the same tier used by the original
    // sqlite().load_trajectory_entries(Some("procedural"), top_k) call.
    let entries = mem
        .load_trajectory_entries(Some("procedural"), top_k)
        .await
        .map_err(ContextError::Memory)?;

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
    tc: &dyn TokenCounting,
) -> Result<Option<Message>, ContextError> {
    if budget_tokens == 0 || !memory.tree_config.enabled {
        return Ok(None);
    }
    let Some(ref mem) = memory.memory else {
        return Ok(None);
    };

    let top_k = memory.tree_config.recall_top_k;
    let nodes = mem
        .load_tree_nodes(1, top_k)
        .await
        .map_err(ContextError::Memory)?;

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
    tc: &dyn TokenCounting,
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
        let line = format!("- [{}] {}\n", s.outcome, safe_summary);
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
    if !injected_ids.is_empty() {
        let mem_clone = mem.clone();
        tokio::spawn(async move {
            if let Err(e) = mem_clone.mark_reasoning_used(&injected_ids).await {
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
        .retrieve_corrections(query, limit, min_score)
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
    tc: &dyn TokenCounting,
    router: Option<&dyn AsyncMemoryRouter>,
) -> Result<(Option<Message>, Option<f32>), ContextError> {
    let Some(ref mem) = memory.memory else {
        return Ok((None, None));
    };
    if memory.recall_limit == 0 || token_budget == 0 {
        return Ok((None, None));
    }

    let recalled = mem
        .recall(query, memory.recall_limit, router)
        .await
        .map_err(ContextError::Memory)?;
    if recalled.is_empty() {
        return Ok((None, None));
    }

    let top_score = recalled.first().map(|r| r.score);

    let mut recall_text = String::with_capacity(token_budget * 3);
    recall_text.push_str(RECALL_PREFIX);
    let mut tokens_used = tc.count_tokens(&recall_text);

    for item in &recalled {
        if item.content.starts_with("[skipped]") || item.content.starts_with("[stopped]") {
            continue;
        }
        let entry = format!("- [{}] {}\n", item.role, item.content);
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
    tc: &dyn TokenCounting,
) -> Result<Option<Message>, ContextError> {
    if !memory.document_config.rag_enabled || token_budget == 0 {
        return Ok(None);
    }
    let Some(ref mem) = memory.memory else {
        return Ok(None);
    };

    let collection = &memory.document_config.collection;
    let top_k = memory.document_config.top_k;
    let chunks = mem
        .search_document_collection(collection, query, top_k)
        .await
        .map_err(ContextError::Memory)?;
    if chunks.is_empty() {
        return Ok(None);
    }

    let mut text = String::from(DOCUMENT_RAG_PREFIX);
    let mut tokens_used = tc.count_tokens(&text);

    for chunk in &chunks {
        if chunk.text.is_empty() {
            continue;
        }
        let entry = format!("{}\n", chunk.text);
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
    tc: &dyn TokenCounting,
) -> Result<Option<Message>, ContextError> {
    let (Some(mem), Some(cid)) = (&memory.memory, memory.conversation_id) else {
        return Ok(None);
    };
    if token_budget == 0 {
        return Ok(None);
    }

    let summaries = mem
        .load_summaries(cid)
        .await
        .map_err(ContextError::Memory)?;
    if summaries.is_empty() {
        return Ok(None);
    }

    let mut summary_text = String::from(SUMMARY_PREFIX);
    let mut tokens_used = tc.count_tokens(&summary_text);

    for summary in summaries.iter().rev() {
        let first = summary.first_message_id.unwrap_or(0);
        let last = summary.last_message_id.unwrap_or(0);
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
    tc: &dyn TokenCounting,
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
        .await
        .map_err(ContextError::Memory)?
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
    use zeph_common::memory::CompressionLevel;
    use zeph_config::{
        ContextStrategy, DocumentConfig, GraphConfig, PersonaConfig, ReasoningConfig,
        TrajectoryConfig, TreeConfig,
    };

    struct NaiveTokenCounter;
    impl zeph_common::memory::TokenCounting for NaiveTokenCounter {
        fn count_tokens(&self, text: &str) -> usize {
            text.split_whitespace().count()
        }
        fn count_tool_schema_tokens(&self, schema: &serde_json::Value) -> usize {
            schema.to_string().split_whitespace().count()
        }
    }

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
            memcot_config: zeph_config::MemCotConfig::default(),
            memcot_state: None,
            tree_config: TreeConfig::default(),
        }
    }

    // ── fetch_graph_facts ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn fetch_graph_facts_returns_none_when_memory_is_none() {
        let view = empty_view();
        let tc = NaiveTokenCounter;
        let result = fetch_graph_facts(&view, "test", 1000, &tc).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn fetch_graph_facts_returns_none_when_budget_zero() {
        let mut view = empty_view();
        view.graph_config.enabled = true;
        let tc = NaiveTokenCounter;
        let result = fetch_graph_facts(&view, "test", 0, &tc).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn fetch_graph_facts_returns_none_when_graph_disabled() {
        let mut view = empty_view();
        view.graph_config.enabled = false;
        let tc = NaiveTokenCounter;
        let result = fetch_graph_facts(&view, "test", 1000, &tc).await.unwrap();
        assert!(result.is_none());
    }

    // ── fetch_persona_facts ───────────────────────────────────────────────────

    #[tokio::test]
    async fn fetch_persona_facts_returns_none_when_memory_is_none() {
        let view = empty_view();
        let tc = NaiveTokenCounter;
        let result = fetch_persona_facts(&view, 1000, &tc).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn fetch_persona_facts_returns_none_when_budget_zero() {
        let mut view = empty_view();
        view.persona_config.enabled = true;
        let tc = NaiveTokenCounter;
        let result = fetch_persona_facts(&view, 0, &tc).await.unwrap();
        assert!(result.is_none());
    }

    // ── fetch_trajectory_hints ────────────────────────────────────────────────

    #[tokio::test]
    async fn fetch_trajectory_hints_returns_none_when_memory_is_none() {
        let view = empty_view();
        let tc = NaiveTokenCounter;
        let result = fetch_trajectory_hints(&view, 1000, &tc).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn fetch_trajectory_hints_returns_none_when_budget_zero() {
        let mut view = empty_view();
        view.trajectory_config.enabled = true;
        let tc = NaiveTokenCounter;
        let result = fetch_trajectory_hints(&view, 0, &tc).await.unwrap();
        assert!(result.is_none());
    }

    // ── fetch_tree_memory ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn fetch_tree_memory_returns_none_when_memory_is_none() {
        let view = empty_view();
        let tc = NaiveTokenCounter;
        let result = fetch_tree_memory(&view, 1000, &tc).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn fetch_tree_memory_returns_none_when_budget_zero() {
        let mut view = empty_view();
        view.tree_config.enabled = true;
        let tc = NaiveTokenCounter;
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
        let tc = NaiveTokenCounter;
        let result = fetch_semantic_recall(&view, "test", 1000, &tc, None)
            .await
            .unwrap();
        assert!(result.0.is_none() && result.1.is_none());
    }

    #[tokio::test]
    async fn fetch_semantic_recall_returns_none_when_budget_zero() {
        let view = empty_view();
        let tc = NaiveTokenCounter;
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
        let tc = NaiveTokenCounter;
        let result = fetch_document_rag(&view, "test", 1000, &tc).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn fetch_document_rag_returns_none_when_rag_disabled() {
        let view = empty_view();
        let tc = NaiveTokenCounter;
        let result = fetch_document_rag(&view, "test", 1000, &tc).await.unwrap();
        assert!(result.is_none());
    }

    // ── fetch_summaries ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn fetch_summaries_returns_none_when_memory_is_none() {
        let view = empty_view();
        let tc = NaiveTokenCounter;
        let result = fetch_summaries(&view, 1000, &tc).await.unwrap();
        assert!(result.is_none());
    }

    // ── fetch_cross_session ───────────────────────────────────────────────────

    #[tokio::test]
    async fn fetch_cross_session_returns_none_when_memory_is_none() {
        let view = empty_view();
        let tc = NaiveTokenCounter;
        let result = fetch_cross_session(&view, "test", 1000, &tc).await.unwrap();
        assert!(result.is_none());
    }

    // ── levels_to_flags ───────────────────────────────────────────────────────

    #[test]
    fn levels_to_flags_empty_slice_enables_all_tiers() {
        let (e, p, d) = levels_to_flags(&[]);
        assert!(e, "episodic should be active for empty slice");
        assert!(p, "procedural should be active for empty slice");
        assert!(d, "declarative should be active for empty slice");
    }

    #[test]
    fn levels_to_flags_full_set_enables_all_tiers() {
        let all = &[
            CompressionLevel::Episodic,
            CompressionLevel::Procedural,
            CompressionLevel::Declarative,
        ];
        let (e, p, d) = levels_to_flags(all);
        assert!(e);
        assert!(p);
        assert!(d);
    }

    #[test]
    fn levels_to_flags_episodic_only() {
        let (e, p, d) = levels_to_flags(&[CompressionLevel::Episodic]);
        assert!(e);
        assert!(!p, "procedural should be inactive");
        assert!(!d, "declarative should be inactive");
    }

    #[test]
    fn levels_to_flags_episodic_and_procedural() {
        let (e, p, d) =
            levels_to_flags(&[CompressionLevel::Episodic, CompressionLevel::Procedural]);
        assert!(e);
        assert!(p);
        assert!(!d, "declarative should be inactive");
    }

    #[test]
    fn levels_to_flags_declarative_only() {
        let (e, p, d) = levels_to_flags(&[CompressionLevel::Declarative]);
        assert!(!e, "episodic should be inactive");
        assert!(!p, "procedural should be inactive");
        assert!(d);
    }

    // ── fetch_reasoning_strategies ────────────────────────────────────────────

    #[tokio::test]
    async fn fetch_reasoning_strategies_returns_none_when_memory_is_none() {
        let mut view = empty_view();
        view.reasoning_config.enabled = true;
        let tc = NaiveTokenCounter;
        let result = fetch_reasoning_strategies(&view, "query", 1000, 3, &tc)
            .await
            .unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn fetch_reasoning_strategies_returns_none_when_budget_zero() {
        let mut view = empty_view();
        view.reasoning_config.enabled = true;
        let tc = NaiveTokenCounter;
        let result = fetch_reasoning_strategies(&view, "query", 0, 3, &tc)
            .await
            .unwrap();
        assert!(result.is_none());
    }

    // ── MockMemoryBackend ─────────────────────────────────────────────────────

    use std::sync::{Arc, Mutex};
    use zeph_common::memory::{
        ContextMemoryBackend, GraphRecallParams, MemCorrection, MemDocumentChunk, MemGraphFact,
        MemPersonaFact, MemReasoningStrategy, MemRecalledMessage, MemSessionSummary, MemSummary,
        MemTrajectoryEntry, MemTreeNode,
    };

    /// Known method names accepted by [`MockMemoryBackend::fail_on`].
    const KNOWN_FAIL_ON: &[&str] = &[
        "load_persona_facts",
        "load_trajectory_entries",
        "load_tree_nodes",
        "load_summaries",
        "retrieve_reasoning_strategies",
        "mark_reasoning_used",
        "retrieve_corrections",
        "recall",
        "recall_graph_facts",
        "search_session_summaries",
        "search_document_collection",
    ];

    #[derive(Default)]
    struct MockMemoryBackend {
        persona_facts: Vec<MemPersonaFact>,
        trajectory_entries: Vec<MemTrajectoryEntry>,
        tree_nodes: Vec<MemTreeNode>,
        summaries: Vec<MemSummary>,
        reasoning_strategies: Vec<MemReasoningStrategy>,
        corrections: Vec<MemCorrection>,
        recalled: Vec<MemRecalledMessage>,
        graph_facts: Vec<MemGraphFact>,
        session_summaries: Vec<MemSessionSummary>,
        document_chunks: Vec<MemDocumentChunk>,
        /// When `Some("method_name")`, that method returns `Err(...)`.
        fail_on: Option<&'static str>,
        /// Tracks IDs passed to `mark_reasoning_used`.
        marked_ids: Mutex<Vec<String>>,
    }

    impl MockMemoryBackend {
        fn with_fail_on(method: &'static str) -> Self {
            debug_assert!(
                KNOWN_FAIL_ON.contains(&method),
                "unknown fail_on method name: {method}"
            );
            Self {
                fail_on: Some(method),
                ..Default::default()
            }
        }

        fn fail_err(method: &str) -> Box<dyn std::error::Error + Send + Sync> {
            format!("mock error in {method}").into()
        }
    }

    impl ContextMemoryBackend for MockMemoryBackend {
        fn load_persona_facts<'a>(
            &'a self,
            _min_confidence: f64,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<
                            Vec<MemPersonaFact>,
                            Box<dyn std::error::Error + Send + Sync>,
                        >,
                    > + Send
                    + 'a,
            >,
        > {
            let result = if self.fail_on == Some("load_persona_facts") {
                Err(Self::fail_err("load_persona_facts"))
            } else {
                Ok(self.persona_facts.clone())
            };
            Box::pin(async move { result })
        }

        fn load_trajectory_entries<'a>(
            &'a self,
            _tier: Option<&'a str>,
            _top_k: usize,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<
                            Vec<MemTrajectoryEntry>,
                            Box<dyn std::error::Error + Send + Sync>,
                        >,
                    > + Send
                    + 'a,
            >,
        > {
            let result = if self.fail_on == Some("load_trajectory_entries") {
                Err(Self::fail_err("load_trajectory_entries"))
            } else {
                Ok(self.trajectory_entries.clone())
            };
            Box::pin(async move { result })
        }

        fn load_tree_nodes<'a>(
            &'a self,
            _level: u32,
            _top_k: usize,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<Vec<MemTreeNode>, Box<dyn std::error::Error + Send + Sync>>,
                    > + Send
                    + 'a,
            >,
        > {
            let result = if self.fail_on == Some("load_tree_nodes") {
                Err(Self::fail_err("load_tree_nodes"))
            } else {
                Ok(self.tree_nodes.clone())
            };
            Box::pin(async move { result })
        }

        fn load_summaries<'a>(
            &'a self,
            _conversation_id: i64,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<Vec<MemSummary>, Box<dyn std::error::Error + Send + Sync>>,
                    > + Send
                    + 'a,
            >,
        > {
            let result = if self.fail_on == Some("load_summaries") {
                Err(Self::fail_err("load_summaries"))
            } else {
                Ok(self.summaries.clone())
            };
            Box::pin(async move { result })
        }

        fn retrieve_reasoning_strategies<'a>(
            &'a self,
            _query: &'a str,
            _top_k: usize,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<
                            Vec<MemReasoningStrategy>,
                            Box<dyn std::error::Error + Send + Sync>,
                        >,
                    > + Send
                    + 'a,
            >,
        > {
            let result = if self.fail_on == Some("retrieve_reasoning_strategies") {
                Err(Self::fail_err("retrieve_reasoning_strategies"))
            } else {
                Ok(self.reasoning_strategies.clone())
            };
            Box::pin(async move { result })
        }

        fn mark_reasoning_used<'a>(
            &'a self,
            ids: &'a [String],
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<(), Box<dyn std::error::Error + Send + Sync>>,
                    > + Send
                    + 'a,
            >,
        > {
            if self.fail_on == Some("mark_reasoning_used") {
                return Box::pin(async move { Err(Self::fail_err("mark_reasoning_used")) });
            }
            let mut guard = self.marked_ids.lock().expect("marked_ids poisoned");
            guard.extend_from_slice(ids);
            Box::pin(async move { Ok(()) })
        }

        fn retrieve_corrections<'a>(
            &'a self,
            _query: &'a str,
            _limit: usize,
            _min_score: f32,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<
                            Vec<MemCorrection>,
                            Box<dyn std::error::Error + Send + Sync>,
                        >,
                    > + Send
                    + 'a,
            >,
        > {
            let result = if self.fail_on == Some("retrieve_corrections") {
                Err(Self::fail_err("retrieve_corrections"))
            } else {
                Ok(self.corrections.clone())
            };
            Box::pin(async move { result })
        }

        fn recall<'a>(
            &'a self,
            _query: &'a str,
            _limit: usize,
            _router: Option<&'a dyn zeph_common::memory::AsyncMemoryRouter>,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<
                            Vec<MemRecalledMessage>,
                            Box<dyn std::error::Error + Send + Sync>,
                        >,
                    > + Send
                    + 'a,
            >,
        > {
            let result = if self.fail_on == Some("recall") {
                Err(Self::fail_err("recall"))
            } else {
                Ok(self.recalled.clone())
            };
            Box::pin(async move { result })
        }

        fn recall_graph_facts<'a>(
            &'a self,
            _query: &'a str,
            _params: GraphRecallParams<'a>,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<
                            Vec<MemGraphFact>,
                            Box<dyn std::error::Error + Send + Sync>,
                        >,
                    > + Send
                    + 'a,
            >,
        > {
            let result = if self.fail_on == Some("recall_graph_facts") {
                Err(Self::fail_err("recall_graph_facts"))
            } else {
                Ok(self.graph_facts.clone())
            };
            Box::pin(async move { result })
        }

        fn search_session_summaries<'a>(
            &'a self,
            _query: &'a str,
            _limit: usize,
            _current_conversation_id: Option<i64>,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<
                            Vec<MemSessionSummary>,
                            Box<dyn std::error::Error + Send + Sync>,
                        >,
                    > + Send
                    + 'a,
            >,
        > {
            let result = if self.fail_on == Some("search_session_summaries") {
                Err(Self::fail_err("search_session_summaries"))
            } else {
                Ok(self.session_summaries.clone())
            };
            Box::pin(async move { result })
        }

        fn search_document_collection<'a>(
            &'a self,
            _collection: &'a str,
            _query: &'a str,
            _top_k: usize,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<
                            Vec<MemDocumentChunk>,
                            Box<dyn std::error::Error + Send + Sync>,
                        >,
                    > + Send
                    + 'a,
            >,
        > {
            let result = if self.fail_on == Some("search_document_collection") {
                Err(Self::fail_err("search_document_collection"))
            } else {
                Ok(self.document_chunks.clone())
            };
            Box::pin(async move { result })
        }
    }

    fn mock_view(mock: MockMemoryBackend) -> ContextMemoryView {
        let mut v = empty_view();
        v.memory = Some(Arc::new(mock));
        v
    }

    // ── fetch_graph_facts (happy path) ────────────────────────────────────────

    #[tokio::test]
    async fn fetch_graph_facts_returns_message_when_memory_present() {
        let mock = MockMemoryBackend {
            graph_facts: vec![zeph_common::memory::MemGraphFact {
                fact: "Rust is fast".to_string(),
                confidence: 0.9,
                activation_score: None,
                neighbors: vec![],
                provenance_snippet: None,
            }],
            ..Default::default()
        };
        let mut view = mock_view(mock);
        view.graph_config.enabled = true;
        // recall_timeout_ms must be non-zero or it gets clamped to 100ms
        view.graph_config.spreading_activation.recall_timeout_ms = 5000;
        let tc = NaiveTokenCounter;
        let result = fetch_graph_facts(&view, "test", 1000, &tc).await.unwrap();
        assert!(result.is_some(), "expected Some message");
        let msg = result.unwrap();
        assert!(
            msg.content.contains("Rust is fast"),
            "expected fact text in output, got: {}",
            msg.content
        );
        assert!(
            msg.content.starts_with(GRAPH_FACTS_PREFIX),
            "expected GRAPH_FACTS_PREFIX"
        );
    }

    #[tokio::test]
    async fn fetch_graph_facts_swallows_error_and_returns_none() {
        let mock = MockMemoryBackend::with_fail_on("recall_graph_facts");
        let mut view = mock_view(mock);
        view.graph_config.enabled = true;
        view.graph_config.spreading_activation.recall_timeout_ms = 5000;
        let tc = NaiveTokenCounter;
        // B1: fetch_graph_facts swallows errors via tracing::warn! and returns Ok(None)
        let result = fetch_graph_facts(&view, "test", 1000, &tc).await.unwrap();
        assert!(
            result.is_none(),
            "expected None when recall_graph_facts errors"
        );
    }

    #[tokio::test]
    async fn fetch_graph_facts_returns_none_when_facts_empty() {
        let mock = MockMemoryBackend::default(); // empty graph_facts
        let mut view = mock_view(mock);
        view.graph_config.enabled = true;
        view.graph_config.spreading_activation.recall_timeout_ms = 5000;
        let tc = NaiveTokenCounter;
        let result = fetch_graph_facts(&view, "test", 1000, &tc).await.unwrap();
        assert!(result.is_none());
    }

    // ── fetch_persona_facts ───────────────────────────────────────────────────

    #[tokio::test]
    async fn fetch_persona_facts_returns_message_when_persona_enabled() {
        let mock = MockMemoryBackend {
            persona_facts: vec![MemPersonaFact {
                category: "preference".to_string(),
                content: "prefers concise answers".to_string(),
            }],
            ..Default::default()
        };
        let mut view = mock_view(mock);
        view.persona_config.enabled = true;
        view.persona_config.context_budget_tokens = 1000;
        let tc = NaiveTokenCounter;
        let result = fetch_persona_facts(&view, 1000, &tc).await.unwrap();
        assert!(result.is_some());
        let msg = result.unwrap();
        assert!(msg.content.contains("preference"));
        assert!(msg.content.contains("prefers concise answers"));
        assert!(msg.content.starts_with(crate::slot::PERSONA_PREFIX));
    }

    #[tokio::test]
    async fn fetch_persona_facts_propagates_error() {
        let mock = MockMemoryBackend::with_fail_on("load_persona_facts");
        let mut view = mock_view(mock);
        view.persona_config.enabled = true;
        let tc = NaiveTokenCounter;
        let result = fetch_persona_facts(&view, 1000, &tc).await;
        assert!(
            result.is_err(),
            "expected Err from load_persona_facts failure"
        );
    }

    // ── fetch_trajectory_hints ────────────────────────────────────────────────

    #[tokio::test]
    async fn fetch_trajectory_hints_returns_message_when_trajectory_enabled() {
        let mock = MockMemoryBackend {
            trajectory_entries: vec![MemTrajectoryEntry {
                intent: "summarize code".to_string(),
                outcome: "produced concise summary".to_string(),
                confidence: 0.9,
            }],
            ..Default::default()
        };
        let mut view = mock_view(mock);
        view.trajectory_config.enabled = true;
        view.trajectory_config.context_budget_tokens = 1000;
        view.trajectory_config.min_confidence = 0.5;
        let tc = NaiveTokenCounter;
        let result = fetch_trajectory_hints(&view, 1000, &tc).await.unwrap();
        assert!(result.is_some());
        let msg = result.unwrap();
        assert!(msg.content.contains("summarize code"));
        assert!(msg.content.starts_with(crate::slot::TRAJECTORY_PREFIX));
    }

    #[tokio::test]
    async fn fetch_trajectory_hints_passes_tier_filter() {
        // I1: confidence filtering — entry below min_confidence must be excluded,
        // entry above must be present. Verifies the .filter(|e| e.confidence >= min_conf) branch.
        let mock = MockMemoryBackend {
            trajectory_entries: vec![
                MemTrajectoryEntry {
                    intent: "debug async code".to_string(),
                    outcome: "fixed deadlock".to_string(),
                    confidence: 0.85,
                },
                MemTrajectoryEntry {
                    intent: "low confidence task".to_string(),
                    outcome: "irrelevant".to_string(),
                    confidence: 0.3,
                },
            ],
            ..Default::default()
        };
        let mut view = mock_view(mock);
        view.trajectory_config.enabled = true;
        view.trajectory_config.context_budget_tokens = 1000;
        view.trajectory_config.min_confidence = 0.5;
        let tc = NaiveTokenCounter;
        let result = fetch_trajectory_hints(&view, 1000, &tc).await.unwrap();
        assert!(result.is_some(), "expected Some message");
        let msg = result.unwrap();
        assert!(
            msg.content.contains("debug async code"),
            "high-confidence entry must be included"
        );
        assert!(
            !msg.content.contains("low confidence task"),
            "entry below min_confidence must be filtered out"
        );
    }

    #[tokio::test]
    async fn fetch_trajectory_hints_propagates_error() {
        let mock = MockMemoryBackend::with_fail_on("load_trajectory_entries");
        let mut view = mock_view(mock);
        view.trajectory_config.enabled = true;
        let tc = NaiveTokenCounter;
        let result = fetch_trajectory_hints(&view, 1000, &tc).await;
        assert!(result.is_err());
    }

    // ── fetch_tree_memory ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn fetch_tree_memory_returns_message_when_tree_enabled() {
        let mock = MockMemoryBackend {
            tree_nodes: vec![MemTreeNode {
                content: "Topic: async Rust patterns".to_string(),
            }],
            ..Default::default()
        };
        let mut view = mock_view(mock);
        view.tree_config.enabled = true;
        view.tree_config.context_budget_tokens = 1000;
        let tc = NaiveTokenCounter;
        let result = fetch_tree_memory(&view, 1000, &tc).await.unwrap();
        assert!(result.is_some());
        let msg = result.unwrap();
        assert!(msg.content.contains("async Rust patterns"));
        assert!(msg.content.starts_with(crate::slot::TREE_MEMORY_PREFIX));
    }

    #[tokio::test]
    async fn fetch_tree_memory_propagates_error() {
        let mock = MockMemoryBackend::with_fail_on("load_tree_nodes");
        let mut view = mock_view(mock);
        view.tree_config.enabled = true;
        let tc = NaiveTokenCounter;
        let result = fetch_tree_memory(&view, 1000, &tc).await;
        assert!(result.is_err());
    }

    // ── fetch_corrections ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn fetch_corrections_returns_message_when_corrections_present() {
        let mock = MockMemoryBackend {
            corrections: vec![MemCorrection {
                correction_text: "use snake_case not camelCase".to_string(),
            }],
            ..Default::default()
        };
        let view = mock_view(mock);
        let result = fetch_corrections(&view, "query", 10, 0.5, |s| s.into())
            .await
            .unwrap();
        assert!(result.is_some());
        let msg = result.unwrap();
        assert!(msg.content.contains("snake_case"));
        assert!(msg.content.starts_with(CORRECTIONS_PREFIX));
    }

    #[tokio::test]
    async fn fetch_corrections_swallows_error_returns_none() {
        // fetch_corrections uses unwrap_or_default() so retrieve_corrections errors
        // are swallowed: error → empty vec → None. This documents the production behavior.
        let mock = MockMemoryBackend::with_fail_on("retrieve_corrections");
        let view = mock_view(mock);
        let result = fetch_corrections(&view, "query", 10, 0.5, |s| s.into())
            .await
            .unwrap();
        assert!(result.is_none());
    }

    // ── fetch_semantic_recall ─────────────────────────────────────────────────

    #[tokio::test]
    async fn fetch_semantic_recall_returns_message_with_content() {
        let mock = MockMemoryBackend {
            recalled: vec![
                MemRecalledMessage {
                    role: "user".to_string(),
                    content: "how does tokio work".to_string(),
                    score: 0.95,
                },
                MemRecalledMessage {
                    role: "assistant".to_string(),
                    content: "tokio is an async runtime".to_string(),
                    score: 0.88,
                },
            ],
            ..Default::default()
        };
        let mut view = mock_view(mock);
        view.recall_limit = 10;
        let tc = NaiveTokenCounter;
        let (msg, score) = fetch_semantic_recall(&view, "tokio", 1000, &tc, None)
            .await
            .unwrap();
        assert!(msg.is_some(), "expected Some message");
        // I4: verify score equals first message's score
        assert!(score.is_some_and(|s| (s - 0.95_f32).abs() < f32::EPSILON));
        let msg = msg.unwrap();
        // content is in parts.Recall so check parts
        let has_recall_part = msg.parts.iter().any(|p| {
            if let zeph_llm::provider::MessagePart::Recall { text } = p {
                text.contains("how does tokio work")
            } else {
                false
            }
        });
        assert!(has_recall_part, "expected recalled content in Recall part");
    }

    #[tokio::test]
    async fn fetch_semantic_recall_returns_none_when_recalled_empty() {
        let mock = MockMemoryBackend::default();
        let mut view = mock_view(mock);
        view.recall_limit = 10;
        let tc = NaiveTokenCounter;
        let (msg, score) = fetch_semantic_recall(&view, "query", 1000, &tc, None)
            .await
            .unwrap();
        assert!(msg.is_none());
        assert!(score.is_none());
    }

    #[tokio::test]
    async fn fetch_semantic_recall_propagates_error() {
        let mock = MockMemoryBackend::with_fail_on("recall");
        let mut view = mock_view(mock);
        view.recall_limit = 10;
        let tc = NaiveTokenCounter;
        let result = fetch_semantic_recall(&view, "query", 1000, &tc, None).await;
        assert!(result.is_err());
    }

    // ── fetch_document_rag ────────────────────────────────────────────────────

    #[tokio::test]
    async fn fetch_document_rag_returns_message_when_rag_enabled() {
        let mock = MockMemoryBackend {
            document_chunks: vec![MemDocumentChunk {
                text: "Rust ownership rules prevent data races".to_string(),
            }],
            ..Default::default()
        };
        let mut view = mock_view(mock);
        view.document_config.rag_enabled = true;
        let tc = NaiveTokenCounter;
        let result = fetch_document_rag(&view, "ownership", 1000, &tc)
            .await
            .unwrap();
        assert!(result.is_some());
        let msg = result.unwrap();
        assert!(msg.content.contains("ownership rules"));
        assert!(msg.content.starts_with(DOCUMENT_RAG_PREFIX));
    }

    #[tokio::test]
    async fn fetch_document_rag_propagates_error() {
        let mock = MockMemoryBackend::with_fail_on("search_document_collection");
        let mut view = mock_view(mock);
        view.document_config.rag_enabled = true;
        let tc = NaiveTokenCounter;
        let result = fetch_document_rag(&view, "query", 1000, &tc).await;
        assert!(result.is_err());
    }

    // ── fetch_summaries ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn fetch_summaries_returns_message_when_summaries_present() {
        let mock = MockMemoryBackend {
            summaries: vec![MemSummary {
                first_message_id: Some(1),
                last_message_id: Some(5),
                content: "User asked about async Rust".to_string(),
            }],
            ..Default::default()
        };
        let mut view = mock_view(mock);
        view.conversation_id = Some(42);
        let tc = NaiveTokenCounter;
        let result = fetch_summaries(&view, 1000, &tc).await.unwrap();
        assert!(result.is_some());
        let msg = result.unwrap();
        let has_summary_part = msg.parts.iter().any(|p| {
            if let zeph_llm::provider::MessagePart::Summary { text } = p {
                text.contains("Messages 1-5") && text.contains("async Rust")
            } else {
                false
            }
        });
        assert!(
            has_summary_part,
            "expected Summary part with messages range"
        );
    }

    #[tokio::test]
    async fn fetch_summaries_returns_none_without_conversation_id() {
        let mock = MockMemoryBackend {
            summaries: vec![MemSummary {
                first_message_id: Some(1),
                last_message_id: Some(5),
                content: "some content".to_string(),
            }],
            ..Default::default()
        };
        let mut view = mock_view(mock);
        view.conversation_id = None; // no conversation_id → must return None
        let tc = NaiveTokenCounter;
        let result = fetch_summaries(&view, 1000, &tc).await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn fetch_summaries_propagates_error() {
        let mock = MockMemoryBackend::with_fail_on("load_summaries");
        let mut view = mock_view(mock);
        view.conversation_id = Some(42);
        let tc = NaiveTokenCounter;
        let result = fetch_summaries(&view, 1000, &tc).await;
        assert!(result.is_err());
    }

    // ── fetch_cross_session ───────────────────────────────────────────────────

    #[tokio::test]
    async fn fetch_cross_session_returns_message_when_results_present() {
        let mock = MockMemoryBackend {
            session_summaries: vec![MemSessionSummary {
                summary_text: "Previous session: debugging tokio deadlock".to_string(),
                score: 0.9,
            }],
            ..Default::default()
        };
        let mut view = mock_view(mock);
        view.conversation_id = Some(1);
        view.cross_session_score_threshold = 0.5;
        let tc = NaiveTokenCounter;
        let result = fetch_cross_session(&view, "async", 1000, &tc)
            .await
            .unwrap();
        assert!(result.is_some());
        let msg = result.unwrap();
        let has_cross_session_part = msg.parts.iter().any(|p| {
            if let zeph_llm::provider::MessagePart::CrossSession { text } = p {
                text.contains("tokio deadlock")
            } else {
                false
            }
        });
        assert!(has_cross_session_part);
    }

    #[tokio::test]
    async fn fetch_cross_session_propagates_error() {
        let mock = MockMemoryBackend::with_fail_on("search_session_summaries");
        let mut view = mock_view(mock);
        view.conversation_id = Some(1);
        let tc = NaiveTokenCounter;
        let result = fetch_cross_session(&view, "query", 1000, &tc).await;
        assert!(result.is_err());
    }

    // ── fetch_reasoning_strategies (happy path + mark_used) ──────────────────

    #[tokio::test]
    async fn fetch_reasoning_strategies_returns_message_and_marks_used() {
        let mock = Arc::new(MockMemoryBackend {
            reasoning_strategies: vec![
                MemReasoningStrategy {
                    id: "strat-1".to_string(),
                    outcome: "success".to_string(),
                    summary: "break the problem into small steps".to_string(),
                },
                MemReasoningStrategy {
                    id: "strat-2".to_string(),
                    outcome: "success".to_string(),
                    summary: "use tracing spans for debugging".to_string(),
                },
            ],
            ..Default::default()
        });
        let marked_ids = Arc::clone(&mock);
        let mut view = empty_view();
        view.memory = Some(mock);
        view.reasoning_config.enabled = true;
        view.reasoning_config.context_budget_tokens = 1000;
        let tc = NaiveTokenCounter;
        let result = fetch_reasoning_strategies(&view, "debug", 1000, 5, &tc)
            .await
            .unwrap();
        assert!(result.is_some());
        let msg = result.unwrap();
        assert!(msg.content.starts_with(crate::slot::REASONING_PREFIX));
        assert!(msg.content.contains("break the problem"));

        // B2: yield to let the spawned tokio::spawn task complete before asserting marked_ids.
        // Two yields are sufficient under the default single-threaded #[tokio::test] runtime.
        // If the flavor changes to multi_thread, replace with a short sleep or JoinHandle tracking.
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;

        let ids = marked_ids.marked_ids.lock().expect("marked_ids poisoned");
        assert!(
            ids.contains(&"strat-1".to_string()),
            "expected strat-1 marked"
        );
        assert!(
            ids.contains(&"strat-2".to_string()),
            "expected strat-2 marked"
        );
    }

    #[tokio::test]
    async fn fetch_reasoning_strategies_propagates_error() {
        let mock = MockMemoryBackend::with_fail_on("retrieve_reasoning_strategies");
        let mut view = mock_view(mock);
        view.reasoning_config.enabled = true;
        let tc = NaiveTokenCounter;
        let result = fetch_reasoning_strategies(&view, "query", 1000, 3, &tc).await;
        assert!(result.is_err());
    }

    // ── edge cases ────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn fetch_semantic_recall_skips_skipped_and_stopped_messages() {
        let mock = MockMemoryBackend {
            recalled: vec![
                MemRecalledMessage {
                    role: "user".to_string(),
                    content: "[skipped] some content".to_string(),
                    score: 0.95,
                },
                MemRecalledMessage {
                    role: "user".to_string(),
                    content: "[stopped] other content".to_string(),
                    score: 0.90,
                },
                MemRecalledMessage {
                    role: "user".to_string(),
                    content: "valid content to recall".to_string(),
                    score: 0.85,
                },
            ],
            ..Default::default()
        };
        let mut view = mock_view(mock);
        view.recall_limit = 10;
        let tc = NaiveTokenCounter;
        let (msg, _) = fetch_semantic_recall(&view, "query", 1000, &tc, None)
            .await
            .unwrap();
        assert!(msg.is_some());
        let msg = msg.unwrap();
        let full_text = msg.parts.iter().find_map(|p| {
            if let zeph_llm::provider::MessagePart::Recall { text } = p {
                Some(text.clone())
            } else {
                None
            }
        });
        let text = full_text.unwrap_or_default();
        assert!(
            !text.contains("[skipped]"),
            "skipped messages must be excluded"
        );
        assert!(
            !text.contains("[stopped]"),
            "stopped messages must be excluded"
        );
        assert!(
            text.contains("valid content to recall"),
            "valid messages must be included"
        );
    }

    #[tokio::test]
    async fn fetch_cross_session_filters_below_threshold() {
        let mock = MockMemoryBackend {
            session_summaries: vec![
                MemSessionSummary {
                    summary_text: "high relevance session".to_string(),
                    score: 0.9,
                },
                MemSessionSummary {
                    summary_text: "low relevance session".to_string(),
                    score: 0.2,
                },
            ],
            ..Default::default()
        };
        let mut view = mock_view(mock);
        view.conversation_id = Some(1);
        view.cross_session_score_threshold = 0.5;
        let tc = NaiveTokenCounter;
        let result = fetch_cross_session(&view, "query", 1000, &tc)
            .await
            .unwrap();
        assert!(result.is_some());
        let msg = result.unwrap();
        let text = msg
            .parts
            .iter()
            .find_map(|p| {
                if let zeph_llm::provider::MessagePart::CrossSession { text } = p {
                    Some(text.clone())
                } else {
                    None
                }
            })
            .unwrap_or_default();
        assert!(
            text.contains("high relevance"),
            "high score must be included"
        );
        assert!(
            !text.contains("low relevance"),
            "low score must be filtered out"
        );
    }

    #[tokio::test]
    async fn fetch_document_rag_skips_empty_chunks() {
        let mock = MockMemoryBackend {
            document_chunks: vec![
                MemDocumentChunk {
                    text: String::new(),
                }, // empty — must be skipped
                MemDocumentChunk {
                    text: "real content here".to_string(),
                },
            ],
            ..Default::default()
        };
        let mut view = mock_view(mock);
        view.document_config.rag_enabled = true;
        let tc = NaiveTokenCounter;
        let result = fetch_document_rag(&view, "query", 1000, &tc).await.unwrap();
        assert!(result.is_some());
        let msg = result.unwrap();
        assert!(msg.content.contains("real content here"));
        // empty chunk text should not produce an empty line before prefix
        assert!(!msg.content.contains("\n\n\n"));
    }

    #[tokio::test]
    async fn fetch_graph_facts_sanitizes_injection_payloads() {
        // I3: newlines and angle brackets are replaced with spaces
        let mock = MockMemoryBackend {
            graph_facts: vec![zeph_common::memory::MemGraphFact {
                fact: "fact with <script>alert(1)</script> and\nnewline".to_string(),
                confidence: 0.8,
                activation_score: None,
                neighbors: vec![],
                provenance_snippet: None,
            }],
            ..Default::default()
        };
        let mut view = mock_view(mock);
        view.graph_config.enabled = true;
        view.graph_config.spreading_activation.recall_timeout_ms = 5000;
        let tc = NaiveTokenCounter;
        let result = fetch_graph_facts(&view, "test", 1000, &tc).await.unwrap();
        assert!(result.is_some());
        let msg = result.unwrap();
        assert!(
            !msg.content.contains('<'),
            "angle brackets must be sanitized"
        );
        // The formatter adds trailing \n to each line, but embedded \n in fact text is replaced
        // with spaces. Verify no double-newline sequences exist (would indicate unsanitized \n).
        assert!(
            !msg.content.contains("\n\n"),
            "embedded newlines must be sanitized, no double-newline sequences expected"
        );
    }

    #[tokio::test]
    async fn fetch_reasoning_strategies_sanitizes_injection_payloads() {
        // I3: newlines and angle brackets are replaced with spaces in strategy summaries
        let mock = MockMemoryBackend {
            reasoning_strategies: vec![MemReasoningStrategy {
                id: "s1".to_string(),
                outcome: "success".to_string(),
                summary: "strategy with <b>bold</b> and\nnewline".to_string(),
            }],
            ..Default::default()
        };
        let mut view = mock_view(mock);
        view.reasoning_config.enabled = true;
        let tc = NaiveTokenCounter;
        let result = fetch_reasoning_strategies(&view, "query", 1000, 3, &tc)
            .await
            .unwrap();
        assert!(result.is_some());
        let msg = result.unwrap();
        assert!(
            !msg.content.contains('<'),
            "angle brackets must be sanitized in strategy summaries"
        );
    }

    // ── budget truncation (CR-1) ──────────────────────────────────────────────

    #[tokio::test]
    async fn fetch_persona_facts_truncates_at_budget() {
        let tc = NaiveTokenCounter;
        // Tight budget: fits prefix + exactly 1 fact line, second must be omitted.
        let first_line = "[pref] brief\n";
        let budget = tc.count_tokens(crate::slot::PERSONA_PREFIX) + tc.count_tokens(first_line);
        let mock = MockMemoryBackend {
            persona_facts: vec![
                MemPersonaFact {
                    category: "pref".to_string(),
                    content: "brief".to_string(),
                },
                MemPersonaFact {
                    category: "lang".to_string(),
                    content: "english".to_string(),
                },
            ],
            ..Default::default()
        };
        let mut view = mock_view(mock);
        view.persona_config.enabled = true;
        let result = fetch_persona_facts(&view, budget, &tc).await.unwrap();
        let msg = result.unwrap();
        assert!(msg.content.contains("brief"), "first fact must be included");
        assert!(
            !msg.content.contains("english"),
            "second fact must be truncated by budget"
        );
    }

    #[tokio::test]
    async fn fetch_semantic_recall_truncates_at_budget() {
        let tc = NaiveTokenCounter;
        // Tight budget: fits prefix + exactly 1 recall entry, second must be omitted.
        let first_entry = "- [user] first message\n";
        let budget = tc.count_tokens(RECALL_PREFIX) + tc.count_tokens(first_entry);
        let mock = MockMemoryBackend {
            recalled: vec![
                MemRecalledMessage {
                    role: "user".to_string(),
                    content: "first message".to_string(),
                    score: 0.95,
                },
                MemRecalledMessage {
                    role: "user".to_string(),
                    content: "second message that should be truncated".to_string(),
                    score: 0.80,
                },
            ],
            ..Default::default()
        };
        let mut view = mock_view(mock);
        view.recall_limit = 10;
        let (msg, _) = fetch_semantic_recall(&view, "query", budget, &tc, None)
            .await
            .unwrap();
        assert!(msg.is_some());
        let text = msg
            .unwrap()
            .parts
            .iter()
            .find_map(|p| {
                if let zeph_llm::provider::MessagePart::Recall { text } = p {
                    Some(text.clone())
                } else {
                    None
                }
            })
            .unwrap_or_default();
        assert!(
            text.contains("first message"),
            "first entry must be included"
        );
        assert!(
            !text.contains("second message"),
            "second entry must be truncated by budget"
        );
    }

    // ── provenance_snippet sanitization (CR-2 test) ───────────────────────────

    #[tokio::test]
    async fn fetch_graph_facts_sanitizes_provenance_snippet() {
        use zeph_common::memory::MemGraphNeighbor;
        let mock = MockMemoryBackend {
            graph_facts: vec![zeph_common::memory::MemGraphFact {
                fact: "safe fact".to_string(),
                confidence: 0.9,
                activation_score: None,
                neighbors: vec![MemGraphNeighbor {
                    fact: "neighbor".to_string(),
                    confidence: 0.7,
                }],
                provenance_snippet: Some("source with <injection>\nand newline".to_string()),
            }],
            ..Default::default()
        };
        let mut view = mock_view(mock);
        view.graph_config.enabled = true;
        view.graph_config.spreading_activation.recall_timeout_ms = 5000;
        let tc = NaiveTokenCounter;
        let result = fetch_graph_facts(&view, "test", 1000, &tc).await.unwrap();
        assert!(result.is_some());
        let msg = result.unwrap();
        assert!(
            !msg.content.contains('<'),
            "angle brackets in provenance_snippet must be sanitized"
        );
        assert!(
            !msg.content.contains("\n\n"),
            "newlines in provenance_snippet must be sanitized"
        );
        assert!(
            msg.content.contains("[source:"),
            "provenance snippet must be rendered"
        );
    }
}
