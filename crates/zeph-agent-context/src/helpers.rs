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
use std::future::Future;
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
#[tracing::instrument(name = "agent_context.helpers.fetch_graph_facts", skip_all, err)]
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

/// Read-only inputs threaded through the graph-retrieval-strategy call chain
/// (`dispatch_graph_strategy` → `run_graph_strategy` / `run_synapse_strategy` →
/// `run_hybrid_strategy` → `recall_by_classified_strategy`).
///
/// Bundles the query and its graph-traversal tuning knobs so they don't have to be
/// threaded positionally through every hop of the chain. `edge_types_json` is kept as a
/// separate argument alongside this struct because its ownership (moved on its last use,
/// cloned when a call site needs it again afterward) varies per call site — folding it in
/// here would force every caller to clone it even when a move would do.
#[derive(Clone, Copy)]
struct GraphStrategyParams<'a> {
    query: &'a str,
    recall_limit: usize,
    max_hops: u32,
    temporal_decay_rate: f64,
    edge_types: &'a [zeph_memory::graph::EdgeType],
    strategy_str: &'a str,
}

/// Graph-config references shared by the strategy-classification and per-strategy recall
/// calls in the graph-retrieval chain (see [`GraphStrategyParams`]).
///
/// Kept separate from `GraphStrategyParams` because it is config, not per-call query state,
/// and separate from [`GraphRecallBudget`] because it is never mutated.
#[derive(Clone, Copy)]
struct GraphRecallConfig<'a> {
    graph_config: &'a zeph_config::GraphConfig,
    sa_config: &'a zeph_config::memory::SpreadingActivationConfig,
}

/// Token-budget accumulator state shared across graph-retrieval calls: the in-progress
/// system-message body, tokens consumed so far, the total budget, and the token counter
/// used to measure both.
///
/// Threaded by unique `&mut` reference (reborrowed at each call) instead of `body`/
/// `tokens_so_far` as separate positional `&mut` arguments.
struct GraphRecallBudget<'a> {
    body: &'a mut String,
    tokens_so_far: &'a mut usize,
    budget_tokens: usize,
    tc: &'a TokenCounter,
}

/// Append graph facts to `budget.body` respecting the token budget; returns result count.
fn append_graph_facts(
    facts: &[zeph_memory::graph::types::GraphFact],
    budget: &mut GraphRecallBudget<'_>,
) -> usize {
    let mut count = 0;
    for f in facts {
        let fact_text = f.fact.replace(['\n', '\r', '<', '>'], " ");
        let line = format!("- {} (confidence: {:.2})\n", fact_text, f.confidence);
        let line_tokens = budget.tc.count_tokens(&line);
        if *budget.tokens_so_far + line_tokens > budget.budget_tokens {
            break;
        }
        budget.body.push_str(&line);
        *budget.tokens_so_far += line_tokens;
        count += 1;
    }
    count
}

/// Await a graph recall future, logging retrieval failures and appending any results
/// to `body` via [`append_graph_facts`].
///
/// Shared by the `Bfs`/`AStar`/`WaterCircles`/`BeamSearch` match arms and the `Hybrid`
/// arm of [`fetch_graph_facts_raw`] — they differ only in which future produces the
/// fact list. `Synapse` is handled separately because its activation-score fact
/// formatting differs from [`append_graph_facts`].
///
/// On success with a non-empty result, appends facts to `body`/`tokens_so_far`. On
/// success with an empty result, logs a `NoHit` failure record and leaves `body`
/// unchanged, which the caller's tail check (`body == GRAPH_FACTS_PREFIX`) turns into
/// `Ok(None)`. On error, logs an `Error` failure record and propagates it.
///
/// `start` is the instant recall latency should be measured from. Callers with a
/// genuinely lazy `recall` future can pass `Instant::now()` right before calling; the
/// `Hybrid` caller passes the instant captured before its own (already-awaited)
/// classification + recall, since by the time it hands off an already-resolved
/// `std::future::ready(...)` here, starting a fresh clock would measure only the time
/// to poll an already-completed future instead of real recall latency.
async fn run_graph_strategy<F>(
    memory: &zeph_memory::semantic::SemanticMemory,
    params: GraphStrategyParams<'_>,
    edge_types_json: Option<String>,
    start: Instant,
    recall: F,
    budget: &mut GraphRecallBudget<'_>,
) -> Result<(), zeph_memory::MemoryError>
where
    F: Future<Output = Result<Vec<zeph_memory::graph::types::GraphFact>, zeph_memory::MemoryError>>,
{
    let facts = match recall.await {
        Ok(f) => f,
        Err(e) => {
            let latency_ms = start.elapsed().as_millis().try_into().unwrap_or(u64::MAX);
            memory.log_retrieval_failure(RetrievalFailureRecord {
                conversation_id: None,
                turn_index: 0,
                failure_type: RetrievalFailureType::Error,
                retrieval_strategy: params.strategy_str.to_owned(),
                query_text: params.query.to_owned(),
                query_len: params.query.len(),
                top_score: None,
                confidence_threshold: None,
                result_count: 0,
                latency_ms,
                edge_types: edge_types_json,
                error_context: Some(format!("{e:#}")),
            });
            return Err(e);
        }
    };
    let latency_ms = start.elapsed().as_millis().try_into().unwrap_or(u64::MAX);
    if facts.is_empty() {
        memory.log_retrieval_failure(RetrievalFailureRecord {
            conversation_id: None,
            turn_index: 0,
            failure_type: RetrievalFailureType::NoHit,
            retrieval_strategy: params.strategy_str.to_owned(),
            query_text: params.query.to_owned(),
            query_len: params.query.len(),
            top_score: None,
            confidence_threshold: None,
            result_count: 0,
            latency_ms,
            edge_types: edge_types_json,
            error_context: None,
        });
        return Ok(());
    }
    append_graph_facts(&facts, budget);
    Ok(())
}

/// Run the `Synapse` (spreading-activation) graph retrieval strategy.
///
/// Kept separate from [`run_graph_strategy`] because it has its own recall timeout
/// (in addition to the shared `Error`/`NoHit` cases, it logs a `Timeout` failure) and
/// formats facts with an extra `activation` score column that [`append_graph_facts`]
/// does not support.
///
/// On success, appends facts to `body`/`tokens_so_far`. On an empty result, logs a
/// `NoHit` failure record and leaves `body` unchanged — the caller's tail check
/// (`body == GRAPH_FACTS_PREFIX`) turns this into `Ok(None)`.
async fn run_synapse_strategy(
    memory: &zeph_memory::semantic::SemanticMemory,
    sa_config: &zeph_config::memory::SpreadingActivationConfig,
    params: GraphStrategyParams<'_>,
    edge_types_json: Option<String>,
    budget: &mut GraphRecallBudget<'_>,
) {
    let sa_params = zeph_memory::graph::SpreadingActivationParams {
        decay_lambda: sa_config.decay_lambda,
        max_hops: sa_config.max_hops,
        activation_threshold: sa_config.activation_threshold,
        inhibition_threshold: sa_config.inhibition_threshold,
        max_activated_nodes: sa_config.max_activated_nodes,
        temporal_decay_rate: params.temporal_decay_rate,
        seed_structural_weight: sa_config.seed_structural_weight,
        seed_community_cap: sa_config.seed_community_cap,
        alpha: sa_config.alpha,
    };
    let timeout_ms = effective_recall_timeout_ms(sa_config.recall_timeout_ms);
    let t0 = Instant::now();
    let activated_facts = match tokio::time::timeout(
        std::time::Duration::from_millis(timeout_ms),
        memory.recall_graph_activated(
            params.query,
            params.recall_limit,
            sa_params,
            params.edge_types,
        ),
    )
    .await
    {
        Ok(Ok(facts)) => facts,
        Ok(Err(e)) => {
            let latency_ms = t0.elapsed().as_millis().try_into().unwrap_or(u64::MAX);
            tracing::warn!("spreading activation recall failed: {e:#}");
            // TODO(#3576): conversation_id and turn_index not yet propagated into
            // context helpers; tracked for future enhancement when
            // ContextAssemblyView exposes them.
            memory.log_retrieval_failure(RetrievalFailureRecord {
                conversation_id: None,
                turn_index: 0,
                failure_type: RetrievalFailureType::Error,
                retrieval_strategy: params.strategy_str.to_owned(),
                query_text: params.query.to_owned(),
                query_len: params.query.len(),
                top_score: None,
                confidence_threshold: None,
                result_count: 0,
                latency_ms,
                edge_types: edge_types_json.clone(),
                error_context: Some(format!("{e:#}")),
            });
            Vec::new()
        }
        Err(_) => {
            let latency_ms = t0.elapsed().as_millis().try_into().unwrap_or(u64::MAX);
            tracing::warn!("spreading activation recall timed out ({timeout_ms}ms)");
            memory.log_retrieval_failure(RetrievalFailureRecord {
                conversation_id: None,
                turn_index: 0,
                failure_type: RetrievalFailureType::Timeout,
                retrieval_strategy: params.strategy_str.to_owned(),
                query_text: params.query.to_owned(),
                query_len: params.query.len(),
                top_score: None,
                confidence_threshold: None,
                result_count: 0,
                latency_ms,
                edge_types: edge_types_json.clone(),
                error_context: Some(format!("timeout after {timeout_ms}ms")),
            });
            Vec::new()
        }
    };
    let latency_ms = t0.elapsed().as_millis().try_into().unwrap_or(u64::MAX);
    if activated_facts.is_empty() {
        memory.log_retrieval_failure(RetrievalFailureRecord {
            conversation_id: None,
            turn_index: 0,
            failure_type: RetrievalFailureType::NoHit,
            retrieval_strategy: params.strategy_str.to_owned(),
            query_text: params.query.to_owned(),
            query_len: params.query.len(),
            top_score: None,
            confidence_threshold: None,
            result_count: 0,
            latency_ms,
            edge_types: edge_types_json,
            error_context: None,
        });
        return;
    }
    for f in &activated_facts {
        let fact_text = f.edge.fact.replace(['\n', '\r', '<', '>'], " ");
        let line = format!(
            "- {} (confidence: {:.2}, activation: {:.2})\n",
            fact_text, f.edge.confidence, f.activation_score
        );
        let line_tokens = budget.tc.count_tokens(&line);
        if *budget.tokens_so_far + line_tokens > budget.budget_tokens {
            break;
        }
        budget.body.push_str(&line);
        *budget.tokens_so_far += line_tokens;
    }
}

/// Classify `query` into a concrete graph sub-strategy for the `Hybrid` retrieval
/// strategy, via `memory.classify_graph_strategy` under a fixed timeout.
///
/// Falls back to `"synapse"` and logs a `Timeout` failure record if the classifier
/// doesn't resolve in time.
async fn classify_hybrid_strategy(
    memory: &zeph_memory::semantic::SemanticMemory,
    query: &str,
    edge_types_json: Option<String>,
) -> String {
    const CLASSIFIER_TIMEOUT_MS: u64 = 2_000;
    let classifier_t0 = Instant::now();
    let classified = if let Ok(s) = tokio::time::timeout(
        std::time::Duration::from_millis(CLASSIFIER_TIMEOUT_MS),
        memory.classify_graph_strategy(query),
    )
    .await
    {
        s
    } else {
        let latency_ms = classifier_t0
            .elapsed()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX);
        tracing::warn!(
            "hybrid strategy classifier timed out after {CLASSIFIER_TIMEOUT_MS}ms, \
             falling back to synapse"
        );
        memory.log_retrieval_failure(RetrievalFailureRecord {
            conversation_id: None,
            turn_index: 0,
            failure_type: RetrievalFailureType::Timeout,
            retrieval_strategy: "hybrid_classifier".to_owned(),
            query_text: query.to_owned(),
            query_len: query.len(),
            top_score: None,
            confidence_threshold: None,
            result_count: 0,
            latency_ms,
            edge_types: edge_types_json,
            error_context: Some(format!(
                "classifier timeout after {CLASSIFIER_TIMEOUT_MS}ms"
            )),
        });
        "synapse".to_owned()
    };
    tracing::debug!(classified_strategy = %classified, "hybrid dispatch: classified");
    classified
}

/// Run graph recall for the sub-strategy `classified` by [`classify_hybrid_strategy`].
///
/// The `astar`/`watercircles`/`beam_search`/synapse-fallback branches used to each
/// carry their own copy of the error-logging block; here they only need to produce a
/// `Result`, and the shared `Error`/`NoHit` logging happens once in the caller via
/// [`run_graph_strategy`].
async fn recall_by_classified_strategy(
    classified: &str,
    memory: &zeph_memory::semantic::SemanticMemory,
    config: GraphRecallConfig<'_>,
    params: GraphStrategyParams<'_>,
) -> Result<Vec<zeph_memory::graph::types::GraphFact>, zeph_memory::MemoryError> {
    match classified {
        "astar" => {
            memory
                .recall_graph_astar(
                    params.query,
                    params.recall_limit,
                    params.max_hops,
                    params.temporal_decay_rate,
                    params.edge_types,
                )
                .await
        }
        "watercircles" => {
            let ring_limit = config.graph_config.watercircles.ring_limit;
            memory
                .recall_graph_watercircles(
                    params.query,
                    params.recall_limit,
                    params.max_hops,
                    ring_limit,
                    params.temporal_decay_rate,
                    params.edge_types,
                )
                .await
        }
        "beam_search" => {
            let beam_width = config.graph_config.beam_search.beam_width;
            memory
                .recall_graph_beam(
                    params.query,
                    params.recall_limit,
                    beam_width,
                    params.max_hops,
                    params.temporal_decay_rate,
                    params.edge_types,
                )
                .await
        }
        _ => {
            let sa_params = zeph_memory::graph::SpreadingActivationParams {
                decay_lambda: config.sa_config.decay_lambda,
                max_hops: config.sa_config.max_hops,
                activation_threshold: config.sa_config.activation_threshold,
                inhibition_threshold: config.sa_config.inhibition_threshold,
                max_activated_nodes: config.sa_config.max_activated_nodes,
                temporal_decay_rate: params.temporal_decay_rate,
                seed_structural_weight: config.sa_config.seed_structural_weight,
                seed_community_cap: config.sa_config.seed_community_cap,
                alpha: config.sa_config.alpha,
            };
            memory
                .recall_graph_activated(
                    params.query,
                    params.recall_limit,
                    sa_params,
                    params.edge_types,
                )
                .await
                .map(|activated| {
                    activated
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
                            edge_id: Some(f.edge.id),
                        })
                        .collect()
                })
        }
    }
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
#[tracing::instrument(
    name = "agent_context.helpers.fetch_graph_facts_raw",
    skip_all,
    err,
    fields(effective_strategy)
)]
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

    tracing::Span::current().record(
        "effective_strategy",
        tracing::field::debug(&effective_strategy),
    );
    let strategy_str = format!("{effective_strategy:?}").to_lowercase();
    let edge_types_json = serde_json::to_string(&edge_types).ok();

    let params = GraphStrategyParams {
        query,
        recall_limit,
        max_hops,
        temporal_decay_rate,
        edge_types: &edge_types,
        strategy_str: &strategy_str,
    };
    let config = GraphRecallConfig {
        graph_config,
        sa_config,
    };
    let mut budget = GraphRecallBudget {
        body: &mut body,
        tokens_so_far: &mut tokens_so_far,
        budget_tokens,
        tc,
    };

    dispatch_graph_strategy(
        effective_strategy,
        memory,
        config,
        params,
        edge_types_json,
        &mut budget,
    )
    .await?;

    if body == GRAPH_FACTS_PREFIX {
        return Ok(None);
    }

    Ok(Some(Message::from_legacy(Role::System, body)))
}

/// Dispatch to the recall implementation for `effective_strategy` and append any
/// resulting facts to `body`.
///
/// One match arm per [`zeph_config::memory::GraphRetrievalStrategy`] variant; each arm
/// only selects which recall future to run (and, for `Hybrid`, first resolves the
/// classified sub-strategy) — the shared error/no-hit logging and budget-aware
/// appending live in [`run_synapse_strategy`] / [`run_graph_strategy`]. The line count
/// here comes from enumerating six variants side by side, not from duplicated logic;
/// splitting each arm into its own one-call wrapper function would trade this for five
/// near-identical trivial functions without reducing complexity, so the length lint is
/// suppressed instead.
#[allow(clippy::too_many_lines)]
async fn dispatch_graph_strategy(
    effective_strategy: zeph_config::memory::GraphRetrievalStrategy,
    memory: &zeph_memory::semantic::SemanticMemory,
    config: GraphRecallConfig<'_>,
    params: GraphStrategyParams<'_>,
    edge_types_json: Option<String>,
    budget: &mut GraphRecallBudget<'_>,
) -> Result<(), zeph_memory::MemoryError> {
    use zeph_config::memory::GraphRetrievalStrategy;
    match effective_strategy {
        GraphRetrievalStrategy::Synapse => {
            run_synapse_strategy(memory, config.sa_config, params, edge_types_json, budget).await;
        }
        GraphRetrievalStrategy::Bfs => {
            run_graph_strategy(
                memory,
                params,
                edge_types_json,
                Instant::now(),
                memory.recall_graph(
                    params.query,
                    params.recall_limit,
                    params.max_hops,
                    None,
                    params.temporal_decay_rate,
                    params.edge_types,
                ),
                budget,
            )
            .await?;
        }
        GraphRetrievalStrategy::AStar => {
            run_graph_strategy(
                memory,
                params,
                edge_types_json,
                Instant::now(),
                memory.recall_graph_astar(
                    params.query,
                    params.recall_limit,
                    params.max_hops,
                    params.temporal_decay_rate,
                    params.edge_types,
                ),
                budget,
            )
            .await?;
        }
        GraphRetrievalStrategy::WaterCircles => {
            let ring_limit = config.graph_config.watercircles.ring_limit;
            run_graph_strategy(
                memory,
                params,
                edge_types_json,
                Instant::now(),
                memory.recall_graph_watercircles(
                    params.query,
                    params.recall_limit,
                    params.max_hops,
                    ring_limit,
                    params.temporal_decay_rate,
                    params.edge_types,
                ),
                budget,
            )
            .await?;
        }
        GraphRetrievalStrategy::BeamSearch => {
            let beam_width = config.graph_config.beam_search.beam_width;
            run_graph_strategy(
                memory,
                params,
                edge_types_json,
                Instant::now(),
                memory.recall_graph_beam(
                    params.query,
                    params.recall_limit,
                    beam_width,
                    params.max_hops,
                    params.temporal_decay_rate,
                    params.edge_types,
                ),
                budget,
            )
            .await?;
        }
        GraphRetrievalStrategy::Hybrid => {
            run_hybrid_strategy(memory, config, params, edge_types_json, budget).await?;
        }
        _ => {}
    }
    Ok(())
}

/// Run the `Hybrid` graph retrieval strategy: classify the query into a concrete
/// sub-strategy, run recall for it, then apply the shared failure-logging/append path.
async fn run_hybrid_strategy(
    memory: &zeph_memory::semantic::SemanticMemory,
    config: GraphRecallConfig<'_>,
    params: GraphStrategyParams<'_>,
    edge_types_json: Option<String>,
    budget: &mut GraphRecallBudget<'_>,
) -> Result<(), zeph_memory::MemoryError> {
    let classified = classify_hybrid_strategy(memory, params.query, edge_types_json.clone()).await;
    // Capture the start instant here, before the real recall work, rather than inside
    // run_graph_strategy: by the time facts_result is ready, the future handed to
    // run_graph_strategy below is already resolved (`std::future::ready`), so starting
    // a fresh clock there would measure only the time to poll an already-done future
    // instead of actual recall latency.
    let recall_t0 = Instant::now();
    let facts_result = recall_by_classified_strategy(&classified, memory, config, params).await;

    run_graph_strategy(
        memory,
        params,
        edge_types_json,
        recall_t0,
        std::future::ready(facts_result),
        budget,
    )
    .await
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

#[cfg(test)]
mod run_graph_strategy_latency_tests {
    use std::time::Duration;

    use tokio_util::sync::CancellationToken;
    use zeph_llm::any::AnyProvider;
    use zeph_memory::RetrievalFailureLogger;

    use super::*;

    /// Regression test for the `Hybrid` `latency_ms` telemetry drift fixed alongside
    /// this test: `run_hybrid_strategy` awaits the real recall work *before* handing an
    /// already-resolved `std::future::ready(...)` to `run_graph_strategy`. If
    /// `run_graph_strategy` captured its own `Instant::now()` internally (as it used
    /// to) instead of taking `start` as a parameter, it would measure only the
    /// near-zero time to poll an already-completed future, silently zeroing out
    /// `latency_ms` on every `Hybrid` failure/no-hit record.
    ///
    /// This reproduces that exact shape — real work happens, *then* an already-resolved
    /// future is passed to `run_graph_strategy` alongside the `start` captured before
    /// that work — using a real `SemanticMemory` + `RetrievalFailureLogger` so the
    /// persisted `latency_ms` reflects what a caller would actually observe.
    #[tokio::test]
    async fn latency_is_measured_from_caller_supplied_start_not_from_an_internal_clock() {
        let memory = zeph_memory::semantic::SemanticMemory::new(
            ":memory:",
            "http://127.0.0.1:1",
            None,
            AnyProvider::Mock(zeph_llm::mock::MockProvider::default()),
            "test-model",
        )
        .await
        .unwrap();
        let sup = zeph_common::TaskSupervisor::new(CancellationToken::new());
        let logger = RetrievalFailureLogger::new(
            memory.sqlite().clone(),
            256,
            1, // flush as soon as one record is queued, no need to wait on the interval
            Duration::from_millis(10),
            90,
            &sup,
        );
        let memory = memory.with_retrieval_failure_logger(logger);

        let mut body = String::from(GRAPH_FACTS_PREFIX);
        let mut tokens_so_far = 0usize;
        let tc = TokenCounter::new();
        let edge_types: Vec<zeph_memory::graph::EdgeType> = Vec::new();

        // Simulate the real recall work `recall_by_classified_strategy` performs inside
        // `run_hybrid_strategy` before it hands off an already-resolved future.
        let start = Instant::now();
        tokio::time::sleep(Duration::from_millis(30)).await;
        let facts_result: Result<
            Vec<zeph_memory::graph::types::GraphFact>,
            zeph_memory::MemoryError,
        > = Ok(Vec::new());

        let params = GraphStrategyParams {
            query: "test query",
            recall_limit: 0,
            max_hops: 0,
            temporal_decay_rate: 0.0,
            edge_types: &edge_types,
            strategy_str: "hybrid",
        };
        let mut budget = GraphRecallBudget {
            body: &mut body,
            tokens_so_far: &mut tokens_so_far,
            budget_tokens: 1000,
            tc: &tc,
        };

        run_graph_strategy(
            &memory,
            params,
            None,
            start,
            std::future::ready(facts_result),
            &mut budget,
        )
        .await
        .unwrap();

        // The writer flushes asynchronously; poll briefly instead of a fixed sleep guess.
        let mut latency_ms: Option<i64> = None;
        for _ in 0..50 {
            let rows: Vec<(i64,)> = sqlx::query_as(
                "SELECT latency_ms FROM memory_retrieval_failures WHERE retrieval_strategy = 'hybrid'",
            )
            .fetch_all(memory.sqlite().pool())
            .await
            .unwrap();
            if let Some(row) = rows.first() {
                latency_ms = Some(row.0);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // Drop `memory` (and the `RetrievalFailureLogger` sender it owns) before shutting
        // down the supervisor, so the writer task's `rx.recv()` observes a closed channel
        // and exits its loop promptly instead of needing a forced abort at the timeout.
        drop(memory);
        sup.shutdown_all(Duration::from_secs(5)).await;

        let latency_ms =
            latency_ms.expect("expected a hybrid NoHit failure record to be persisted");
        assert!(
            latency_ms >= 25,
            "latency_ms should reflect the ~30ms of work done before run_graph_strategy was \
             called via the `start` parameter, not ~0ms from a freshly-captured internal \
             Instant polling an already-resolved future; got {latency_ms}"
        );
    }
}
