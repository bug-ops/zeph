// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Adapters that bridge `zeph-memory` concrete types to `zeph-common` traits consumed
//! by `zeph-context`.
//!
//! This module is the only place in the workspace where both `zeph-memory` and
//! `zeph-context` interface types are visible simultaneously — by design. `zeph-core`
//! builds adapters here at Layer 4 so that `zeph-context` (Layer 1) never imports
//! `zeph-memory` (Layer 1).

use std::pin::Pin;

use zeph_common::memory::{
    AsyncMemoryRouter, ContextMemoryBackend, GraphRecallParams, GraphRetrievalStrategy,
    MemCorrection, MemDocumentChunk, MemGraphFact, MemGraphNeighbor, MemPersonaFact,
    MemReasoningStrategy, MemRecalledMessage, MemSessionSummary, MemSummary, MemTrajectoryEntry,
    MemTreeNode, RecallView,
};
use zeph_memory::semantic::SemanticMemory;
use zeph_memory::{ConversationId, RecallView as MemRecallView, RecalledFact};

fn box_err<E: std::error::Error + Send + Sync + 'static>(
    e: E,
) -> Box<dyn std::error::Error + Send + Sync> {
    Box::new(e)
}

fn map_persona_fact(r: zeph_memory::PersonaFactRow) -> MemPersonaFact {
    MemPersonaFact {
        category: r.category,
        content: r.content,
    }
}

fn map_trajectory_entry(r: zeph_memory::TrajectoryEntryRow) -> MemTrajectoryEntry {
    MemTrajectoryEntry {
        intent: r.intent,
        outcome: r.outcome,
        confidence: r.confidence,
    }
}

fn map_tree_node(r: zeph_memory::MemoryTreeRow) -> MemTreeNode {
    MemTreeNode { content: r.content }
}

fn map_summary(r: zeph_memory::semantic::Summary) -> MemSummary {
    MemSummary {
        first_message_id: r.first_message_id.map(|m| m.0),
        last_message_id: r.last_message_id.map(|m| m.0),
        content: r.content,
    }
}

fn map_reasoning_strategy(s: zeph_memory::ReasoningStrategy) -> MemReasoningStrategy {
    MemReasoningStrategy {
        id: s.id,
        outcome: s.outcome.as_str().to_owned(),
        summary: s.summary,
    }
}

fn map_correction(c: zeph_memory::UserCorrectionRow) -> MemCorrection {
    MemCorrection {
        correction_text: c.correction_text,
    }
}

fn map_recalled_message(r: zeph_memory::RecalledMessage) -> MemRecalledMessage {
    use zeph_llm::provider::Role;
    let role = match r.message.role {
        Role::Assistant => "assistant",
        Role::System => "system",
        Role::User | _ => "user",
    }
    .to_owned();
    MemRecalledMessage {
        role,
        content: r.message.content,
        score: r.score,
    }
}

fn map_graph_fact(rf: RecalledFact) -> MemGraphFact {
    MemGraphFact {
        fact: rf.fact.fact,
        confidence: rf.fact.confidence,
        activation_score: rf.activation_score,
        neighbors: rf
            .neighbors
            .into_iter()
            .map(|n| MemGraphNeighbor {
                fact: n.fact,
                confidence: n.confidence,
            })
            .collect(),
        provenance_snippet: rf.provenance_snippet,
    }
}

fn map_session_summary(r: zeph_memory::semantic::SessionSummaryResult) -> MemSessionSummary {
    MemSessionSummary {
        summary_text: r.summary_text,
        score: r.score,
    }
}

/// Adapter that implements [`ContextMemoryBackend`] by delegating to [`SemanticMemory`].
pub struct SemanticMemoryBackend {
    inner: std::sync::Arc<SemanticMemory>,
}

impl SemanticMemoryBackend {
    /// Wrap an `Arc<SemanticMemory>` in the backend adapter.
    #[must_use]
    pub fn new(inner: std::sync::Arc<SemanticMemory>) -> Self {
        Self { inner }
    }
}

type BoxFut<'a, T> = Pin<
    Box<
        dyn std::future::Future<Output = Result<T, Box<dyn std::error::Error + Send + Sync>>>
            + Send
            + 'a,
    >,
>;

impl ContextMemoryBackend for SemanticMemoryBackend {
    fn load_persona_facts(&self, min_confidence: f64) -> BoxFut<'_, Vec<MemPersonaFact>> {
        Box::pin(async move {
            let rows = self
                .inner
                .sqlite()
                .load_persona_facts(min_confidence)
                .await
                .map_err(box_err)?;
            Ok(rows.into_iter().map(map_persona_fact).collect())
        })
    }

    fn load_trajectory_entries<'a>(
        &'a self,
        tier: Option<&'a str>,
        top_k: usize,
    ) -> BoxFut<'a, Vec<MemTrajectoryEntry>> {
        Box::pin(async move {
            let rows = self
                .inner
                .sqlite()
                .load_trajectory_entries(tier, top_k)
                .await
                .map_err(box_err)?;
            Ok(rows.into_iter().map(map_trajectory_entry).collect())
        })
    }

    fn load_tree_nodes(&self, level: u32, top_k: usize) -> BoxFut<'_, Vec<MemTreeNode>> {
        Box::pin(async move {
            let rows = self
                .inner
                .sqlite()
                .load_tree_level(level.into(), top_k)
                .await
                .map_err(box_err)?;
            Ok(rows.into_iter().map(map_tree_node).collect())
        })
    }

    fn load_summaries(&self, conversation_id: i64) -> BoxFut<'_, Vec<MemSummary>> {
        Box::pin(async move {
            let cid = ConversationId(conversation_id);
            let rows = self.inner.load_summaries(cid).await.map_err(box_err)?;
            Ok(rows.into_iter().map(map_summary).collect())
        })
    }

    fn retrieve_reasoning_strategies<'a>(
        &'a self,
        query: &'a str,
        top_k: usize,
    ) -> BoxFut<'a, Vec<MemReasoningStrategy>> {
        Box::pin(async move {
            let strategies = self
                .inner
                .retrieve_reasoning_strategies(query, top_k)
                .await
                .map_err(box_err)?;
            Ok(strategies.into_iter().map(map_reasoning_strategy).collect())
        })
    }

    fn mark_reasoning_used<'a>(&'a self, ids: &'a [String]) -> BoxFut<'a, ()> {
        Box::pin(async move {
            if let Some(ref reasoning) = self.inner.reasoning {
                reasoning.mark_used(ids).await.map_err(box_err)?;
            }
            Ok(())
        })
    }

    fn retrieve_corrections<'a>(
        &'a self,
        query: &'a str,
        limit: usize,
        min_score: f32,
    ) -> BoxFut<'a, Vec<MemCorrection>> {
        Box::pin(async move {
            let corrections = self
                .inner
                .retrieve_similar_corrections(query, limit, min_score)
                .await
                .map_err(box_err)?;
            Ok(corrections.into_iter().map(map_correction).collect())
        })
    }

    fn recall<'a>(
        &'a self,
        query: &'a str,
        limit: usize,
        router: Option<&'a dyn AsyncMemoryRouter>,
    ) -> BoxFut<'a, Vec<MemRecalledMessage>> {
        Box::pin(async move {
            let recalled = if let Some(r) = router {
                self.inner
                    .recall_routed_async(query, limit, None, r, None)
                    .await
                    .map_err(box_err)?
            } else {
                self.inner
                    .recall(query, limit, None)
                    .await
                    .map_err(box_err)?
            };
            Ok(recalled.into_iter().map(map_recalled_message).collect())
        })
    }

    #[allow(clippy::too_many_lines)] // one match arm per GraphRetrievalStrategy variant
    fn recall_graph_facts<'a>(
        &'a self,
        query: &'a str,
        params: GraphRecallParams<'a>,
    ) -> BoxFut<'a, Vec<MemGraphFact>> {
        Box::pin(async move {
            let mem_view = match params.view {
                RecallView::ZoomIn => MemRecallView::ZoomIn,
                RecallView::ZoomOut => MemRecallView::ZoomOut,
                _ => MemRecallView::Head,
            };
            let mem_edge_types: Vec<zeph_memory::EdgeType> = params
                .edge_types
                .iter()
                .map(|e| {
                    use zeph_common::memory::EdgeType as CE;
                    use zeph_memory::EdgeType as ME;
                    match e {
                        CE::Temporal => ME::Temporal,
                        CE::Causal => ME::Causal,
                        CE::Entity => ME::Entity,
                        _ => ME::Semantic,
                    }
                })
                .collect();
            let sa_params = params.spreading_activation.map(|p| {
                zeph_memory::graph::SpreadingActivationParams {
                    decay_lambda: p.decay_lambda,
                    max_hops: p.max_hops,
                    activation_threshold: p.activation_threshold,
                    inhibition_threshold: p.inhibition_threshold,
                    max_activated_nodes: p.max_activated_nodes,
                    temporal_decay_rate: p.temporal_decay_rate,
                    seed_structural_weight: p.seed_structural_weight,
                    seed_community_cap: p.seed_community_cap,
                    alpha: p.alpha,
                }
            });

            let recalled: Vec<RecalledFact> = match params.retrieval_strategy {
                GraphRetrievalStrategy::Synapse => {
                    let Some(sa_params) = sa_params else {
                        tracing::warn!(
                            "recall_graph_facts: Synapse strategy selected but no \
                             spreading_activation params supplied; returning empty result"
                        );
                        return Ok(Vec::new());
                    };
                    self.inner
                        .recall_graph_activated(query, params.limit, sa_params, &mem_edge_types)
                        .await
                        .map_err(box_err)?
                        .into_iter()
                        .map(RecalledFact::from_activated_fact)
                        .collect()
                }
                GraphRetrievalStrategy::Bfs => self
                    .inner
                    .recall_graph(
                        query,
                        params.limit,
                        params.max_hops,
                        None,
                        params.temporal_decay_rate,
                        &mem_edge_types,
                    )
                    .await
                    .map_err(box_err)?
                    .into_iter()
                    .map(RecalledFact::from_graph_fact)
                    .collect(),
                GraphRetrievalStrategy::AStar => self
                    .inner
                    .recall_graph_astar(
                        query,
                        params.limit,
                        params.max_hops,
                        params.temporal_decay_rate,
                        &mem_edge_types,
                    )
                    .await
                    .map_err(box_err)?
                    .into_iter()
                    .map(RecalledFact::from_graph_fact)
                    .collect(),
                GraphRetrievalStrategy::WaterCircles => self
                    .inner
                    .recall_graph_watercircles(
                        query,
                        params.limit,
                        params.max_hops,
                        params.ring_limit,
                        params.temporal_decay_rate,
                        &mem_edge_types,
                    )
                    .await
                    .map_err(box_err)?
                    .into_iter()
                    .map(RecalledFact::from_graph_fact)
                    .collect(),
                GraphRetrievalStrategy::BeamSearch => self
                    .inner
                    .recall_graph_beam(
                        query,
                        params.limit,
                        params.beam_width,
                        params.max_hops,
                        params.temporal_decay_rate,
                        &mem_edge_types,
                    )
                    .await
                    .map_err(box_err)?
                    .into_iter()
                    .map(RecalledFact::from_graph_fact)
                    .collect(),
                GraphRetrievalStrategy::Hybrid => {
                    let classified = self.inner.classify_graph_strategy(query).await;
                    match classified.as_str() {
                        "astar" => self
                            .inner
                            .recall_graph_astar(
                                query,
                                params.limit,
                                params.max_hops,
                                params.temporal_decay_rate,
                                &mem_edge_types,
                            )
                            .await
                            .map_err(box_err)?
                            .into_iter()
                            .map(RecalledFact::from_graph_fact)
                            .collect(),
                        "watercircles" => self
                            .inner
                            .recall_graph_watercircles(
                                query,
                                params.limit,
                                params.max_hops,
                                params.ring_limit,
                                params.temporal_decay_rate,
                                &mem_edge_types,
                            )
                            .await
                            .map_err(box_err)?
                            .into_iter()
                            .map(RecalledFact::from_graph_fact)
                            .collect(),
                        "beam_search" => self
                            .inner
                            .recall_graph_beam(
                                query,
                                params.limit,
                                params.beam_width,
                                params.max_hops,
                                params.temporal_decay_rate,
                                &mem_edge_types,
                            )
                            .await
                            .map_err(box_err)?
                            .into_iter()
                            .map(RecalledFact::from_graph_fact)
                            .collect(),
                        _ => {
                            let Some(sa_params) = sa_params else {
                                tracing::warn!(
                                    "recall_graph_facts: Hybrid classified as synapse but no \
                                     spreading_activation params supplied; returning empty result"
                                );
                                return Ok(Vec::new());
                            };
                            self.inner
                                .recall_graph_activated(
                                    query,
                                    params.limit,
                                    sa_params,
                                    &mem_edge_types,
                                )
                                .await
                                .map_err(box_err)?
                                .into_iter()
                                .map(RecalledFact::from_activated_fact)
                                .collect()
                        }
                    }
                }
            };

            // View-aware enrichment (ZoomIn provenance / ZoomOut neighbors) is orthogonal to
            // which strategy produced the base fact set, so it's applied as a single
            // post-dispatch pass shared by all 6 strategies rather than duplicated per arm.
            let enriched = self
                .inner
                .enrich_recall_view(
                    recalled,
                    mem_view,
                    params.zoom_out_neighbor_cap,
                    params.limit,
                    &mem_edge_types,
                )
                .await
                .map_err(box_err)?;
            Ok(enriched.into_iter().map(map_graph_fact).collect())
        })
    }

    fn search_session_summaries<'a>(
        &'a self,
        query: &'a str,
        limit: usize,
        current_conversation_id: Option<i64>,
    ) -> BoxFut<'a, Vec<MemSessionSummary>> {
        Box::pin(async move {
            let cid = current_conversation_id.map(ConversationId);
            let results = self
                .inner
                .search_session_summaries(query, limit, cid)
                .await
                .map_err(box_err)?;
            Ok(results.into_iter().map(map_session_summary).collect())
        })
    }

    fn search_document_collection<'a>(
        &'a self,
        collection: &'a str,
        query: &'a str,
        top_k: usize,
    ) -> BoxFut<'a, Vec<MemDocumentChunk>> {
        Box::pin(async move {
            let points = self
                .inner
                .search_document_collection(collection, query, top_k)
                .await
                .map_err(box_err)?;
            Ok(points
                .into_iter()
                .map(|p| {
                    let text = p
                        .payload
                        .get("text")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_owned();
                    MemDocumentChunk { text }
                })
                .collect())
        })
    }
}

/// Adapter implementing [`zeph_context::summarization::MessageTokenCounter`] for
/// [`zeph_memory::TokenCounter`].
pub struct TokenCounterAdapter(std::sync::Arc<zeph_memory::TokenCounter>);

impl TokenCounterAdapter {
    /// Wrap an `Arc<TokenCounter>` in the adapter.
    #[must_use]
    pub fn new(inner: std::sync::Arc<zeph_memory::TokenCounter>) -> Self {
        Self(inner)
    }
}

impl zeph_context::summarization::MessageTokenCounter for TokenCounterAdapter {
    fn count_message_tokens(&self, msg: &zeph_llm::provider::Message) -> usize {
        self.0.count_message_tokens(msg)
    }
}

/// Build a memory router from the context manager's routing configuration.
///
/// Moved from `ContextManager::build_router()` to `zeph-agent-context` (Layer 4)
/// so that `zeph-context` (Layer 1) no longer needs to import concrete router types
/// from `zeph-memory` (Layer 1).
///
/// Returns a `Box<dyn AsyncMemoryRouter>` compatible with `ContextAssemblyInput::router`.
#[must_use]
pub fn build_memory_router(
    manager: &zeph_context::manager::ContextManager,
) -> Box<dyn zeph_common::memory::AsyncMemoryRouter + Send + Sync> {
    use zeph_config::StoreRoutingStrategy;

    if !manager.routing.enabled {
        return Box::new(zeph_memory::HeuristicRouter);
    }
    let fallback = manager.routing.fallback_route;
    match manager.routing.strategy {
        StoreRoutingStrategy::Llm => {
            let Some(provider) = manager.store_routing_provider.clone() else {
                tracing::warn!(
                    "store_routing: strategy=llm but no provider resolved; \
                     falling back to heuristic"
                );
                return Box::new(zeph_memory::HeuristicRouter);
            };
            Box::new(zeph_memory::LlmRouter::new(provider, fallback))
        }
        StoreRoutingStrategy::Hybrid => {
            let Some(provider) = manager.store_routing_provider.clone() else {
                tracing::warn!(
                    "store_routing: strategy=hybrid but no provider resolved; \
                     falling back to heuristic"
                );
                return Box::new(zeph_memory::HeuristicRouter);
            };
            Box::new(zeph_memory::HybridRouter::new(
                provider,
                fallback,
                manager.routing.confidence_threshold,
            ))
        }
        _ => Box::new(zeph_memory::HeuristicRouter),
    }
}

#[cfg(test)]
mod tests {
    use zeph_llm::provider::{Message, Role};
    use zeph_memory::graph::types::{EdgeType, GraphFact};
    use zeph_memory::semantic::{SessionSummaryResult, Summary};
    use zeph_memory::types::{ConversationId, MessageId};
    use zeph_memory::{
        MemoryTreeRow, Outcome, PersonaFactRow, ReasoningStrategy, RecalledMessage,
        TrajectoryEntryRow, UserCorrectionRow,
    };

    use super::*;

    fn make_persona_row() -> PersonaFactRow {
        PersonaFactRow {
            id: 1,
            category: "preference".to_owned(),
            content: "prefers short answers".to_owned(),
            confidence: 0.9,
            evidence_count: 3,
            source_conversation_id: None,
            supersedes_id: None,
            created_at: "2026-01-01".to_owned(),
            updated_at: "2026-01-02".to_owned(),
        }
    }

    fn make_trajectory_row() -> TrajectoryEntryRow {
        TrajectoryEntryRow {
            id: 1,
            conversation_id: Some(42),
            turn_index: 5,
            kind: "procedural".to_owned(),
            intent: "read a file".to_owned(),
            outcome: "file read successfully".to_owned(),
            tools_used: "read_file".to_owned(),
            confidence: 0.85,
            created_at: "2026-01-01".to_owned(),
            updated_at: "2026-01-01".to_owned(),
        }
    }

    fn make_tree_row() -> MemoryTreeRow {
        MemoryTreeRow {
            id: 1,
            level: 0,
            parent_id: None,
            content: "node content here".to_owned(),
            source_ids: "1,2,3".to_owned(),
            token_count: 10,
            consolidated_at: None,
            created_at: "2026-01-01".to_owned(),
        }
    }

    fn make_summary() -> Summary {
        Summary {
            id: 1,
            conversation_id: ConversationId(10),
            content: "summary of the conversation".to_owned(),
            first_message_id: Some(MessageId(5)),
            last_message_id: Some(MessageId(20)),
            token_estimate: 100,
        }
    }

    fn make_reasoning_strategy() -> ReasoningStrategy {
        ReasoningStrategy {
            id: "strat-uuid-1".to_owned(),
            summary: "break the problem into parts".to_owned(),
            outcome: Outcome::Success,
            task_hint: "code refactoring task".to_owned(),
            created_at: 1_700_000_000,
            last_used_at: 1_700_000_100,
            use_count: 3,
            embedded_at: Some(1_700_000_050),
        }
    }

    fn make_correction_row() -> UserCorrectionRow {
        UserCorrectionRow {
            id: 1,
            session_id: Some(7),
            original_output: "wrong output".to_owned(),
            correction_text: "use bullet points".to_owned(),
            skill_name: Some("formatting".to_owned()),
            correction_kind: "explicit_rejection".to_owned(),
            created_at: "2026-01-01".to_owned(),
        }
    }

    fn make_recalled_message(role: Role) -> RecalledMessage {
        RecalledMessage {
            message: Message {
                role,
                content: "hello world".to_owned(),
                ..Default::default()
            },
            score: 0.75,
        }
    }

    fn make_graph_fact() -> GraphFact {
        GraphFact {
            entity_name: "Rust".to_owned(),
            relation: "uses".to_owned(),
            target_name: "LLVM".to_owned(),
            fact: "Rust uses LLVM".to_owned(),
            entity_match_score: 0.9,
            hop_distance: 0,
            confidence: 0.95,
            valid_from: None,
            edge_type: EdgeType::Semantic,
            retrieval_count: 1,
            edge_id: Some(10),
        }
    }

    fn make_activated_fact(activation_score: f32) -> zeph_memory::graph::activation::ActivatedFact {
        zeph_memory::graph::activation::ActivatedFact {
            edge: zeph_memory::graph::types::Edge {
                fact: "Rust uses LLVM".to_owned(),
                confidence: 0.95,
                ..zeph_memory::graph::types::Edge::synthetic_anchor(1)
            },
            activation_score,
            is_implicit_conflict: false,
            conflict_candidate_id: None,
        }
    }

    fn make_session_summary() -> SessionSummaryResult {
        SessionSummaryResult {
            summary_text: "yesterday's session about Rust".to_owned(),
            score: 0.88,
            conversation_id: ConversationId(99),
        }
    }

    // ── map_persona_fact ──────────────────────────────────────────────────────

    #[test]
    fn persona_fact_maps_fields() {
        let row = make_persona_row();
        let dto = map_persona_fact(row);
        assert_eq!(dto.category, "preference");
        assert_eq!(dto.content, "prefers short answers");
    }

    // ── map_trajectory_entry ──────────────────────────────────────────────────

    #[test]
    fn trajectory_entry_maps_fields() {
        let row = make_trajectory_row();
        let dto = map_trajectory_entry(row);
        assert_eq!(dto.intent, "read a file");
        assert_eq!(dto.outcome, "file read successfully");
        assert!((dto.confidence - 0.85).abs() < f64::EPSILON);
    }

    // ── map_tree_node ─────────────────────────────────────────────────────────

    #[test]
    fn tree_node_maps_content() {
        let row = make_tree_row();
        let dto = map_tree_node(row);
        assert_eq!(dto.content, "node content here");
    }

    // ── map_summary ───────────────────────────────────────────────────────────

    #[test]
    fn summary_maps_all_fields() {
        let s = make_summary();
        let dto = map_summary(s);
        assert_eq!(dto.first_message_id, Some(5));
        assert_eq!(dto.last_message_id, Some(20));
        assert_eq!(dto.content, "summary of the conversation");
    }

    #[test]
    fn summary_none_message_ids_stay_none() {
        let s = Summary {
            id: 2,
            conversation_id: ConversationId(1),
            content: "shutdown summary".to_owned(),
            first_message_id: None,
            last_message_id: None,
            token_estimate: 50,
        };
        let dto = map_summary(s);
        assert!(dto.first_message_id.is_none());
        assert!(dto.last_message_id.is_none());
    }

    // ── map_reasoning_strategy ────────────────────────────────────────────────

    #[test]
    fn reasoning_strategy_maps_success_outcome() {
        let s = make_reasoning_strategy();
        let dto = map_reasoning_strategy(s);
        assert_eq!(dto.id, "strat-uuid-1");
        assert_eq!(dto.outcome, "success");
        assert_eq!(dto.summary, "break the problem into parts");
    }

    #[test]
    fn reasoning_strategy_maps_failure_outcome() {
        let mut s = make_reasoning_strategy();
        s.outcome = Outcome::Failure;
        let dto = map_reasoning_strategy(s);
        assert_eq!(dto.outcome, "failure");
    }

    // ── map_correction ────────────────────────────────────────────────────────

    #[test]
    fn correction_maps_text() {
        let row = make_correction_row();
        let dto = map_correction(row);
        assert_eq!(dto.correction_text, "use bullet points");
    }

    // ── map_recalled_message ──────────────────────────────────────────────────

    #[test]
    fn recalled_message_maps_user_role() {
        let rm = make_recalled_message(Role::User);
        let dto = map_recalled_message(rm);
        assert_eq!(dto.role, "user");
        assert_eq!(dto.content, "hello world");
        assert!((dto.score - 0.75).abs() < f32::EPSILON);
    }

    #[test]
    fn recalled_message_maps_assistant_role() {
        let rm = make_recalled_message(Role::Assistant);
        let dto = map_recalled_message(rm);
        assert_eq!(dto.role, "assistant");
        assert!((dto.score - 0.75).abs() < f32::EPSILON);
    }

    #[test]
    fn recalled_message_maps_system_role() {
        let rm = make_recalled_message(Role::System);
        let dto = map_recalled_message(rm);
        assert_eq!(dto.role, "system");
        assert!((dto.score - 0.75).abs() < f32::EPSILON);
    }

    // ── map_graph_fact ────────────────────────────────────────────────────────

    #[test]
    fn graph_fact_maps_basic_fields_with_no_enrichment() {
        let rf = RecalledFact::from_graph_fact(make_graph_fact());
        let dto = map_graph_fact(rf);
        assert_eq!(dto.fact, "Rust uses LLVM");
        assert!((dto.confidence - 0.95).abs() < f32::EPSILON);
        assert!(dto.activation_score.is_none());
        assert!(dto.neighbors.is_empty());
        assert!(dto.provenance_snippet.is_none());
    }

    #[test]
    fn graph_fact_maps_neighbors() {
        let mut rf = RecalledFact::from_graph_fact(make_graph_fact());
        rf.neighbors.push(GraphFact {
            entity_name: "LLVM".to_owned(),
            relation: "supports".to_owned(),
            target_name: "WebAssembly".to_owned(),
            fact: "LLVM supports WebAssembly".to_owned(),
            entity_match_score: 0.5,
            hop_distance: 1,
            confidence: 0.8,
            valid_from: None,
            edge_type: EdgeType::Semantic,
            retrieval_count: 0,
            edge_id: None,
        });
        let dto = map_graph_fact(rf);
        assert_eq!(dto.neighbors.len(), 1);
        assert_eq!(dto.neighbors[0].fact, "LLVM supports WebAssembly");
        assert!((dto.neighbors[0].confidence - 0.8).abs() < f32::EPSILON);
    }

    #[test]
    fn graph_fact_maps_provenance_snippet() {
        let mut rf = RecalledFact::from_graph_fact(make_graph_fact());
        rf.provenance_snippet = Some("Rust compiler snippet".to_owned());
        let dto = map_graph_fact(rf);
        assert_eq!(
            dto.provenance_snippet.as_deref(),
            Some("Rust compiler snippet")
        );
    }

    #[test]
    fn activated_fact_maps_edge_fields_and_activation_score() {
        let rf = RecalledFact::from_activated_fact(make_activated_fact(0.82));
        let dto = map_graph_fact(rf);
        assert_eq!(dto.fact, "Rust uses LLVM");
        assert!((dto.confidence - 0.95).abs() < f32::EPSILON);
        assert!(
            dto.activation_score
                .is_some_and(|s| (s - 0.82_f32).abs() < f32::EPSILON)
        );
        assert!(dto.neighbors.is_empty());
        assert!(dto.provenance_snippet.is_none());
    }

    // ── map_session_summary ───────────────────────────────────────────────────

    #[test]
    fn session_summary_maps_fields() {
        let r = make_session_summary();
        let dto = map_session_summary(r);
        assert_eq!(dto.summary_text, "yesterday's session about Rust");
        assert!((dto.score - 0.88).abs() < f32::EPSILON);
    }

    #[test]
    fn session_summary_score_zero() {
        let r = SessionSummaryResult {
            summary_text: "empty session".to_owned(),
            score: 0.0,
            conversation_id: ConversationId(1),
        };
        let dto = map_session_summary(r);
        assert!(dto.score.abs() < f32::EPSILON);
    }

    #[test]
    fn session_summary_score_one() {
        let r = SessionSummaryResult {
            summary_text: "perfect match".to_owned(),
            score: 1.0,
            conversation_id: ConversationId(1),
        };
        let dto = map_session_summary(r);
        assert!((dto.score - 1.0_f32).abs() < f32::EPSILON);
    }

    // ── recall_graph_facts strategy dispatch (issue #6566 regression) ────────

    /// Build a `SemanticMemoryBackend` over a real in-memory `SemanticMemory`, seeded with a
    /// two-hop fixture: `beamseed -> strong` (high confidence), `beamseed -> weak` (low
    /// confidence), and `weak -> hidden` (a hop-2 fact reachable only through the low-
    /// confidence branch).
    ///
    /// `graph_recall_beam` (`retrieval_beam.rs`) keeps only the top-`beam_width` scoring
    /// entities when propagating to the next hop, but does not prune the edges already
    /// collected at the current hop. So with `beam_width = 1`, hop 1 still yields both
    /// `beamseed -> strong` and `beamseed -> weak`, but only `strong` (the higher-confidence
    /// neighbor) survives to seed hop 2 — the `weak -> hidden` edge is never reached. Plain
    /// BFS (`graph_recall`) has no such pruning and reaches all three edges within
    /// `max_hops = 2`. Comparing the two strategies against the same fixture/query/limit
    /// therefore proves `retrieval_strategy` actually selects a different concrete
    /// `SemanticMemory` method, not just that the dispatch compiles.
    async fn seeded_beam_two_hop_backend() -> SemanticMemoryBackend {
        let provider = zeph_llm::any::AnyProvider::Mock(zeph_llm::mock::MockProvider::default());
        let memory = SemanticMemory::new(
            ":memory:",
            "http://127.0.0.1:1",
            None,
            provider,
            "test-model",
        )
        .await
        .unwrap();
        let graph_store =
            std::sync::Arc::new(zeph_memory::GraphStore::new(memory.sqlite().pool().clone()));

        let seed_id = graph_store
            .upsert_entity(
                "beamseed",
                "beamseed",
                zeph_memory::EntityType::Concept,
                None,
                None,
            )
            .await
            .unwrap()
            .0;
        let strong_id = graph_store
            .upsert_entity(
                "strong",
                "strong",
                zeph_memory::EntityType::Concept,
                None,
                None,
            )
            .await
            .unwrap()
            .0;
        let weak_id = graph_store
            .upsert_entity("weak", "weak", zeph_memory::EntityType::Concept, None, None)
            .await
            .unwrap()
            .0;
        let hidden_id = graph_store
            .upsert_entity(
                "hidden",
                "hidden",
                zeph_memory::EntityType::Concept,
                None,
                None,
            )
            .await
            .unwrap()
            .0;

        graph_store
            .insert_edge(
                seed_id,
                strong_id,
                "relates_to",
                "beamseed relates to strong",
                0.95,
                None,
                None,
            )
            .await
            .unwrap();
        graph_store
            .insert_edge(
                seed_id,
                weak_id,
                "relates_to",
                "beamseed relates to weak",
                0.2,
                None,
                None,
            )
            .await
            .unwrap();
        graph_store
            .insert_edge(
                weak_id,
                hidden_id,
                "relates_to",
                "weak relates to hidden",
                0.9,
                None,
                None,
            )
            .await
            .unwrap();

        let memory = std::sync::Arc::new(memory.with_graph_store(graph_store));
        SemanticMemoryBackend::new(memory)
    }

    #[tokio::test]
    async fn recall_graph_facts_dispatches_bfs_and_beam_search_to_different_results() {
        let backend = seeded_beam_two_hop_backend().await;

        let bfs_facts = backend
            .recall_graph_facts(
                "beamseed",
                GraphRecallParams {
                    limit: 10,
                    view: RecallView::Head,
                    zoom_out_neighbor_cap: 0,
                    max_hops: 2,
                    temporal_decay_rate: 0.0,
                    edge_types: &[],
                    spreading_activation: None,
                    retrieval_strategy: GraphRetrievalStrategy::Bfs,
                    beam_width: 0,
                    ring_limit: 0,
                },
            )
            .await
            .unwrap();

        let beam_facts = backend
            .recall_graph_facts(
                "beamseed",
                GraphRecallParams {
                    limit: 10,
                    view: RecallView::Head,
                    zoom_out_neighbor_cap: 0,
                    max_hops: 2,
                    temporal_decay_rate: 0.0,
                    edge_types: &[],
                    spreading_activation: None,
                    retrieval_strategy: GraphRetrievalStrategy::BeamSearch,
                    beam_width: 1,
                    ring_limit: 0,
                },
            )
            .await
            .unwrap();

        assert!(
            !beam_facts.is_empty(),
            "beam search with width=1 should still return the top candidate"
        );
        assert!(
            bfs_facts.len() > beam_facts.len(),
            "expected unbounded BFS to return more facts than beam_width=1 beam search; \
             bfs={}, beam={}",
            bfs_facts.len(),
            beam_facts.len()
        );
    }

    // ── recall_graph_facts: AStar strategy (issue #6566) ─────────────────────

    /// Build a backend seeded with a fixture where the shortest path to `far` runs through
    /// `near` (two cheap/high-confidence edges) rather than the direct low-confidence edge
    /// `astarseed -> far`.
    ///
    /// `graph_recall_astar` (`retrieval_astar.rs`) only keeps edges that participate in some
    /// shortest path between a seed and a reachable node — edge cost is `1.0 - confidence`, so
    /// the direct low-confidence edge (`cost = 0.9`) loses to the two-hop high-confidence path
    /// (`cost = 0.1 + 0.1 = 0.2`) and is dropped entirely. Plain BFS (`graph_recall`) has no
    /// such shortest-path filtering and keeps all three edges within `max_hops`. Comparing the
    /// two proves `retrieval_strategy = AStar` actually dispatches to `recall_graph_astar`, not
    /// just that the match arm compiles.
    async fn seeded_astar_three_hop_backend() -> SemanticMemoryBackend {
        let provider = zeph_llm::any::AnyProvider::Mock(zeph_llm::mock::MockProvider::default());
        let memory = SemanticMemory::new(
            ":memory:",
            "http://127.0.0.1:1",
            None,
            provider,
            "test-model",
        )
        .await
        .unwrap();
        let graph_store =
            std::sync::Arc::new(zeph_memory::GraphStore::new(memory.sqlite().pool().clone()));

        let seed_id = graph_store
            .upsert_entity(
                "astarseed",
                "astarseed",
                zeph_memory::EntityType::Concept,
                None,
                None,
            )
            .await
            .unwrap()
            .0;
        let near_id = graph_store
            .upsert_entity("near", "near", zeph_memory::EntityType::Concept, None, None)
            .await
            .unwrap()
            .0;
        let far_id = graph_store
            .upsert_entity("far", "far", zeph_memory::EntityType::Concept, None, None)
            .await
            .unwrap()
            .0;

        graph_store
            .insert_edge(
                seed_id,
                near_id,
                "relates_to",
                "astarseed relates to near",
                0.9,
                None,
                None,
            )
            .await
            .unwrap();
        graph_store
            .insert_edge(
                seed_id,
                far_id,
                "relates_to",
                "astarseed relates to far",
                0.1,
                None,
                None,
            )
            .await
            .unwrap();
        graph_store
            .insert_edge(
                near_id,
                far_id,
                "relates_to",
                "near relates to far",
                0.9,
                None,
                None,
            )
            .await
            .unwrap();

        let memory = std::sync::Arc::new(memory.with_graph_store(graph_store));
        SemanticMemoryBackend::new(memory)
    }

    #[tokio::test]
    async fn recall_graph_facts_dispatches_bfs_and_astar_to_different_results() {
        let backend = seeded_astar_three_hop_backend().await;

        let bfs_facts = backend
            .recall_graph_facts(
                "astarseed",
                GraphRecallParams {
                    limit: 10,
                    view: RecallView::Head,
                    zoom_out_neighbor_cap: 0,
                    max_hops: 2,
                    temporal_decay_rate: 0.0,
                    edge_types: &[],
                    spreading_activation: None,
                    retrieval_strategy: GraphRetrievalStrategy::Bfs,
                    beam_width: 0,
                    ring_limit: 0,
                },
            )
            .await
            .unwrap();

        let astar_facts = backend
            .recall_graph_facts(
                "astarseed",
                GraphRecallParams {
                    limit: 10,
                    view: RecallView::Head,
                    zoom_out_neighbor_cap: 0,
                    max_hops: 2,
                    temporal_decay_rate: 0.0,
                    edge_types: &[],
                    spreading_activation: None,
                    retrieval_strategy: GraphRetrievalStrategy::AStar,
                    beam_width: 0,
                    ring_limit: 0,
                },
            )
            .await
            .unwrap();

        assert!(
            bfs_facts
                .iter()
                .any(|f| f.fact == "astarseed relates to far"),
            "expected plain BFS to include the direct low-confidence edge; facts={bfs_facts:?}"
        );
        assert!(
            !astar_facts
                .iter()
                .any(|f| f.fact == "astarseed relates to far"),
            "expected A* to exclude the direct edge in favor of the cheaper two-hop path; \
             facts={astar_facts:?}"
        );
        assert!(
            bfs_facts.len() > astar_facts.len(),
            "expected BFS to return more facts than A*'s shortest-path-only set; \
             bfs={}, astar={}",
            bfs_facts.len(),
            astar_facts.len()
        );
    }

    // ── recall_graph_facts: WaterCircles strategy (issue #6566) ──────────────

    /// Build a backend seeded with a single hop fan-out: `watercircleseed -> strong`
    /// (high confidence) and `watercircleseed -> weak` (low confidence).
    async fn seeded_watercircles_ring_backend() -> SemanticMemoryBackend {
        let provider = zeph_llm::any::AnyProvider::Mock(zeph_llm::mock::MockProvider::default());
        let memory = SemanticMemory::new(
            ":memory:",
            "http://127.0.0.1:1",
            None,
            provider,
            "test-model",
        )
        .await
        .unwrap();
        let graph_store =
            std::sync::Arc::new(zeph_memory::GraphStore::new(memory.sqlite().pool().clone()));

        let seed_id = graph_store
            .upsert_entity(
                "watercircleseed",
                "watercircleseed",
                zeph_memory::EntityType::Concept,
                None,
                None,
            )
            .await
            .unwrap()
            .0;
        let strong_id = graph_store
            .upsert_entity(
                "strong",
                "strong",
                zeph_memory::EntityType::Concept,
                None,
                None,
            )
            .await
            .unwrap()
            .0;
        let weak_id = graph_store
            .upsert_entity("weak", "weak", zeph_memory::EntityType::Concept, None, None)
            .await
            .unwrap()
            .0;

        graph_store
            .insert_edge(
                seed_id,
                strong_id,
                "relates_to",
                "watercircleseed relates to strong",
                0.95,
                None,
                None,
            )
            .await
            .unwrap();
        graph_store
            .insert_edge(
                seed_id,
                weak_id,
                "relates_to",
                "watercircleseed relates to weak",
                0.2,
                None,
                None,
            )
            .await
            .unwrap();

        let memory = std::sync::Arc::new(memory.with_graph_store(graph_store));
        SemanticMemoryBackend::new(memory)
    }

    /// Proves `retrieval_strategy = WaterCircles` dispatches to `recall_graph_watercircles`
    /// (a genuinely different code path than `Bfs`), rather than proving a specific pruning
    /// outcome.
    ///
    /// The fixture seeds two depth-1 edges from `watercircleseed`: `strong` (confidence 0.95)
    /// and `weak` (confidence 0.2). With `ring_limit = 1`, `WaterCircles` caps ring 1 to its
    /// single highest-scoring edge (`strong`), while plain `Bfs` returns both edges
    /// unfiltered — the length divergence (1 vs 2) proves the dispatch reaches
    /// `recall_graph_watercircles` rather than falling through to BFS.
    #[tokio::test]
    async fn recall_graph_facts_dispatches_bfs_and_watercircles_to_different_results() {
        let backend = seeded_watercircles_ring_backend().await;

        let bfs_facts = backend
            .recall_graph_facts(
                "watercircleseed",
                GraphRecallParams {
                    limit: 10,
                    view: RecallView::Head,
                    zoom_out_neighbor_cap: 0,
                    max_hops: 1,
                    temporal_decay_rate: 0.0,
                    edge_types: &[],
                    spreading_activation: None,
                    retrieval_strategy: GraphRetrievalStrategy::Bfs,
                    beam_width: 0,
                    ring_limit: 0,
                },
            )
            .await
            .unwrap();

        let watercircles_facts = backend
            .recall_graph_facts(
                "watercircleseed",
                GraphRecallParams {
                    limit: 10,
                    view: RecallView::Head,
                    zoom_out_neighbor_cap: 0,
                    max_hops: 1,
                    temporal_decay_rate: 0.0,
                    edge_types: &[],
                    spreading_activation: None,
                    retrieval_strategy: GraphRetrievalStrategy::WaterCircles,
                    beam_width: 0,
                    ring_limit: 1,
                },
            )
            .await
            .unwrap();

        assert_eq!(
            bfs_facts.len(),
            2,
            "expected plain BFS to return both edges; facts={bfs_facts:?}"
        );
        assert_eq!(
            watercircles_facts.len(),
            1,
            "WaterCircles ring_limit=1 should keep only the higher-scoring edge (strong); \
             facts={watercircles_facts:?}"
        );
        assert_ne!(
            bfs_facts.len(),
            watercircles_facts.len(),
            "the divergence itself proves retrieval_strategy = WaterCircles reaches \
             recall_graph_watercircles rather than silently falling through to BFS"
        );
    }

    // ── recall_graph_facts: Synapse strategy + Hybrid classifier-fallback arm ─
    // (issue #6566)

    #[tokio::test]
    async fn recall_graph_facts_dispatches_synapse_activation_when_strategy_is_synapse() {
        let backend = seeded_beam_two_hop_backend().await;
        let sa_params = zeph_common::memory::SpreadingActivationParams {
            decay_lambda: 0.85,
            max_hops: 3,
            activation_threshold: 0.1,
            inhibition_threshold: 0.8,
            max_activated_nodes: 50,
            temporal_decay_rate: 0.0,
            seed_structural_weight: 0.4,
            seed_community_cap: 3,
            alpha: 0.3,
        };

        let facts = backend
            .recall_graph_facts(
                "beamseed",
                GraphRecallParams {
                    limit: 10,
                    view: RecallView::Head,
                    zoom_out_neighbor_cap: 0,
                    max_hops: 2,
                    temporal_decay_rate: 0.0,
                    edge_types: &[],
                    spreading_activation: Some(sa_params),
                    retrieval_strategy: GraphRetrievalStrategy::Synapse,
                    beam_width: 0,
                    ring_limit: 0,
                },
            )
            .await
            .unwrap();

        assert!(
            !facts.is_empty(),
            "expected Synapse strategy to recall at least one activated fact"
        );
        assert!(
            facts.iter().all(|f| f.activation_score.is_some()),
            "expected every fact from the Synapse arm to carry an activation_score \
             (proves recall_graph_activated was called, not a BFS-family method); facts={facts:?}"
        );
    }

    /// Proves the `Hybrid` dispatch's classifier-fallback arm: when
    /// `classify_graph_strategy` returns anything other than `"astar"`/`"watercircles"`/
    /// `"beam_search"` (in practice always `"synapse"` — `classify_retrieval_strategy`
    /// normalizes any unrecognized LLM response to `"synapse"` itself), the `_` arm must
    /// call `recall_graph_activated` (Synapse), not fall through to plain BFS.
    ///
    /// `MockProvider::default()`'s `chat()` returns `"mock response"`, which the classifier
    /// does not recognize and therefore normalizes to `"synapse"` — driving the fallback arm
    /// without needing a dedicated provider fixture.
    #[tokio::test]
    async fn recall_graph_facts_hybrid_falls_back_to_synapse_when_classifier_is_unrecognized() {
        let backend = seeded_beam_two_hop_backend().await;
        let sa_params = zeph_common::memory::SpreadingActivationParams {
            decay_lambda: 0.85,
            max_hops: 3,
            activation_threshold: 0.1,
            inhibition_threshold: 0.8,
            max_activated_nodes: 50,
            temporal_decay_rate: 0.0,
            seed_structural_weight: 0.4,
            seed_community_cap: 3,
            alpha: 0.3,
        };

        let facts = backend
            .recall_graph_facts(
                "beamseed",
                GraphRecallParams {
                    limit: 10,
                    view: RecallView::Head,
                    zoom_out_neighbor_cap: 0,
                    max_hops: 2,
                    temporal_decay_rate: 0.0,
                    edge_types: &[],
                    spreading_activation: Some(sa_params),
                    retrieval_strategy: GraphRetrievalStrategy::Hybrid,
                    beam_width: 0,
                    ring_limit: 0,
                },
            )
            .await
            .unwrap();

        assert!(
            !facts.is_empty(),
            "expected the Hybrid fallback arm to recall at least one activated fact"
        );
        assert!(
            facts.iter().all(|f| f.activation_score.is_some()),
            "expected every fact from Hybrid's classifier-fallback arm to carry an \
             activation_score (proves it reached recall_graph_activated, not a BFS-family \
             method); facts={facts:?}"
        );
    }

    // ── recall_graph_facts view enrichment across strategies (issue #6566 S2) ────────
    //
    // `recall_graph_facts`'s strategy dispatch produces raw facts from whichever concrete
    // `SemanticMemory` method fired, then runs `SemanticMemory::enrich_recall_view` as a
    // single post-dispatch pass shared by all 6 strategies. These tests prove that pass
    // actually attaches `ZoomIn`/`ZoomOut` enrichment for two different strategies (Synapse
    // and BeamSearch), not just that the dispatch match compiles.

    /// Build a `SemanticMemoryBackend` seeded with one message and one edge carrying that
    /// message as its `episode_id` (source-message provenance), for `ZoomIn` tests. Returns
    /// the backend plus the exact snippet text the enrichment pass should surface.
    async fn seeded_zoomin_provenance_backend() -> (SemanticMemoryBackend, String) {
        let provider = zeph_llm::any::AnyProvider::Mock(zeph_llm::mock::MockProvider::default());
        let memory = SemanticMemory::new(
            ":memory:",
            "http://127.0.0.1:1",
            None,
            provider,
            "test-model",
        )
        .await
        .unwrap();
        let graph_store =
            std::sync::Arc::new(zeph_memory::GraphStore::new(memory.sqlite().pool().clone()));

        let cid = memory.sqlite().create_conversation().await.unwrap();
        let snippet = "the message that introduced this fact";
        let message_id = memory
            .sqlite()
            .save_message(cid, "user", snippet)
            .await
            .unwrap();

        let seed_id = graph_store
            .upsert_entity(
                "zoominseed",
                "zoominseed",
                zeph_memory::EntityType::Concept,
                None,
                None,
            )
            .await
            .unwrap()
            .0;
        let target_id = graph_store
            .upsert_entity(
                "zoomintarget",
                "zoomintarget",
                zeph_memory::EntityType::Concept,
                None,
                None,
            )
            .await
            .unwrap()
            .0;
        graph_store
            .insert_edge(
                seed_id,
                target_id,
                "relates_to",
                "zoominseed relates to zoomintarget",
                0.9,
                Some(message_id),
                None,
            )
            .await
            .unwrap();

        let memory = std::sync::Arc::new(memory.with_graph_store(graph_store));
        (SemanticMemoryBackend::new(memory), snippet.to_owned())
    }

    #[tokio::test]
    async fn recall_graph_facts_zoomin_enrichment_present_for_synapse_strategy() {
        let (backend, snippet) = seeded_zoomin_provenance_backend().await;
        let sa_params = zeph_common::memory::SpreadingActivationParams {
            decay_lambda: 0.85,
            max_hops: 3,
            activation_threshold: 0.1,
            inhibition_threshold: 0.8,
            max_activated_nodes: 50,
            temporal_decay_rate: 0.0,
            seed_structural_weight: 0.4,
            seed_community_cap: 3,
            alpha: 0.3,
        };

        let facts = backend
            .recall_graph_facts(
                "zoominseed",
                GraphRecallParams {
                    limit: 10,
                    view: RecallView::ZoomIn,
                    zoom_out_neighbor_cap: 0,
                    max_hops: 2,
                    temporal_decay_rate: 0.0,
                    edge_types: &[],
                    spreading_activation: Some(sa_params),
                    retrieval_strategy: GraphRetrievalStrategy::Synapse,
                    beam_width: 0,
                    ring_limit: 0,
                },
            )
            .await
            .unwrap();

        assert!(!facts.is_empty(), "expected at least one Synapse fact");
        assert!(
            facts
                .iter()
                .any(|f| f.provenance_snippet.as_deref() == Some(snippet.as_str())),
            "expected ZoomIn enrichment to attach the source-message snippet for the Synapse \
             strategy after the post-dispatch enrich_recall_view pass; facts={facts:?}"
        );
    }

    /// Build a `SemanticMemoryBackend` seeded with a fan-out from one seed entity to a
    /// high-confidence "head" target and a low-confidence "neighbor" target, for `ZoomOut`
    /// tests. With `limit = 1`, the strategy's own result is truncated to the head edge only —
    /// the neighbor edge is only surfaced via `ZoomOut`'s post-dispatch 1-hop expansion.
    async fn seeded_zoomout_neighbor_backend() -> SemanticMemoryBackend {
        let provider = zeph_llm::any::AnyProvider::Mock(zeph_llm::mock::MockProvider::default());
        let memory = SemanticMemory::new(
            ":memory:",
            "http://127.0.0.1:1",
            None,
            provider,
            "test-model",
        )
        .await
        .unwrap();
        let graph_store =
            std::sync::Arc::new(zeph_memory::GraphStore::new(memory.sqlite().pool().clone()));

        let seed_id = graph_store
            .upsert_entity(
                "zoomoutseed",
                "zoomoutseed",
                zeph_memory::EntityType::Concept,
                None,
                None,
            )
            .await
            .unwrap()
            .0;
        let head_id = graph_store
            .upsert_entity(
                "zoomouthead",
                "zoomouthead",
                zeph_memory::EntityType::Concept,
                None,
                None,
            )
            .await
            .unwrap()
            .0;
        let neighbor_id = graph_store
            .upsert_entity(
                "zoomoutneighbor",
                "zoomoutneighbor",
                zeph_memory::EntityType::Concept,
                None,
                None,
            )
            .await
            .unwrap()
            .0;

        graph_store
            .insert_edge(
                seed_id,
                head_id,
                "relates_to",
                "zoomoutseed relates to zoomouthead",
                0.95,
                None,
                None,
            )
            .await
            .unwrap();
        graph_store
            .insert_edge(
                seed_id,
                neighbor_id,
                "relates_to",
                "zoomoutseed relates to zoomoutneighbor",
                0.3,
                None,
                None,
            )
            .await
            .unwrap();

        let memory = std::sync::Arc::new(memory.with_graph_store(graph_store));
        SemanticMemoryBackend::new(memory)
    }

    #[tokio::test]
    async fn recall_graph_facts_zoomout_enrichment_present_for_beam_search_strategy() {
        let backend = seeded_zoomout_neighbor_backend().await;

        let facts = backend
            .recall_graph_facts(
                "zoomoutseed",
                GraphRecallParams {
                    limit: 1,
                    view: RecallView::ZoomOut,
                    zoom_out_neighbor_cap: 5,
                    max_hops: 1,
                    temporal_decay_rate: 0.0,
                    edge_types: &[],
                    spreading_activation: None,
                    retrieval_strategy: GraphRetrievalStrategy::BeamSearch,
                    beam_width: 1,
                    ring_limit: 0,
                },
            )
            .await
            .unwrap();

        assert_eq!(
            facts.len(),
            1,
            "limit=1 should truncate BeamSearch's own result to the single highest-confidence \
             edge; facts={facts:?}"
        );
        assert!(
            !facts[0].neighbors.is_empty(),
            "expected ZoomOut enrichment to surface the lower-confidence sibling edge as a \
             1-hop neighbor for the BeamSearch strategy after the post-dispatch \
             enrich_recall_view pass; facts={facts:?}"
        );
        assert_eq!(
            facts[0].neighbors[0].fact,
            "zoomoutseed relates to zoomoutneighbor"
        );
    }
}
