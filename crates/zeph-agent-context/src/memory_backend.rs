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
    AsyncMemoryRouter, ContextMemoryBackend, GraphRecallParams, MemCorrection, MemDocumentChunk,
    MemGraphFact, MemGraphNeighbor, MemPersonaFact, MemReasoningStrategy, MemRecalledMessage,
    MemSessionSummary, MemSummary, MemTrajectoryEntry, MemTreeNode, RecallView,
};
use zeph_memory::semantic::SemanticMemory;
use zeph_memory::{ConversationId, RecallView as MemRecallView, RecalledFact};

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
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::System => "system",
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
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
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
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
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
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
            Ok(rows.into_iter().map(map_tree_node).collect())
        })
    }

    fn load_summaries(&self, conversation_id: i64) -> BoxFut<'_, Vec<MemSummary>> {
        Box::pin(async move {
            let cid = ConversationId(conversation_id);
            let rows = self
                .inner
                .load_summaries(cid)
                .await
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
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
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
            Ok(strategies.into_iter().map(map_reasoning_strategy).collect())
        })
    }

    fn mark_reasoning_used<'a>(&'a self, ids: &'a [String]) -> BoxFut<'a, ()> {
        Box::pin(async move {
            if let Some(ref reasoning) = self.inner.reasoning {
                reasoning
                    .mark_used(ids)
                    .await
                    .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
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
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
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
                    .recall_routed_async(query, limit, None, r)
                    .await
                    .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?
            } else {
                self.inner
                    .recall(query, limit, None)
                    .await
                    .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?
            };
            Ok(recalled.into_iter().map(map_recalled_message).collect())
        })
    }

    fn recall_graph_facts<'a>(
        &'a self,
        query: &'a str,
        params: GraphRecallParams<'a>,
    ) -> BoxFut<'a, Vec<MemGraphFact>> {
        Box::pin(async move {
            let mem_view = match params.view {
                RecallView::Head => MemRecallView::Head,
                RecallView::ZoomIn => MemRecallView::ZoomIn,
                RecallView::ZoomOut => MemRecallView::ZoomOut,
            };
            let mem_edge_types: Vec<zeph_memory::EdgeType> = params
                .edge_types
                .iter()
                .map(|e| {
                    use zeph_common::memory::EdgeType as CE;
                    use zeph_memory::EdgeType as ME;
                    match e {
                        CE::Semantic => ME::Semantic,
                        CE::Temporal => ME::Temporal,
                        CE::Causal => ME::Causal,
                        CE::Entity => ME::Entity,
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
                }
            });
            let recalled = self
                .inner
                .recall_graph_view(
                    query,
                    params.limit,
                    mem_view,
                    params.zoom_out_neighbor_cap,
                    params.max_hops,
                    params.temporal_decay_rate,
                    &mem_edge_types,
                    sa_params,
                )
                .await
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
            Ok(recalled.into_iter().map(map_graph_fact).collect())
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
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
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
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
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
    use zeph_common::memory::parse_route_str;
    use zeph_config::StoreRoutingStrategy;

    if !manager.routing.enabled {
        return Box::new(zeph_memory::HeuristicRouter);
    }
    let fallback = parse_route_str(
        &manager.routing.fallback_route,
        zeph_common::memory::MemoryRoute::Hybrid,
    );
    match manager.routing.strategy {
        StoreRoutingStrategy::Heuristic => Box::new(zeph_memory::HeuristicRouter),
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
    }
}

#[cfg(test)]
mod tests {
    use zeph_llm::provider::{Message, Role};
    use zeph_memory::graph::types::{EdgeType, GraphFact};
    use zeph_memory::semantic::{SessionSummaryResult, Summary};
    use zeph_memory::types::{ConversationId, MessageId};
    use zeph_memory::{
        MemoryTreeRow, Outcome, PersonaFactRow, ReasoningStrategy, RecalledFact, RecalledMessage,
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

    fn make_recalled_fact() -> RecalledFact {
        RecalledFact::from_graph_fact(make_graph_fact())
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
    fn graph_fact_maps_basic_fields() {
        let rf = make_recalled_fact();
        let dto = map_graph_fact(rf);
        assert_eq!(dto.fact, "Rust uses LLVM");
        assert!((dto.confidence - 0.95).abs() < f32::EPSILON);
        assert!(dto.activation_score.is_none());
        assert!(dto.neighbors.is_empty());
        assert!(dto.provenance_snippet.is_none());
    }

    #[test]
    fn graph_fact_maps_activation_score() {
        let mut rf = make_recalled_fact();
        rf.activation_score = Some(0.82);
        let dto = map_graph_fact(rf);
        assert!(
            dto.activation_score
                .is_some_and(|s| (s - 0.82_f32).abs() < f32::EPSILON)
        );
    }

    #[test]
    fn graph_fact_maps_neighbors() {
        let mut rf = make_recalled_fact();
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
        let mut rf = make_recalled_fact();
        rf.provenance_snippet = Some("Rust compiler snippet".to_owned());
        let dto = map_graph_fact(rf);
        assert_eq!(
            dto.provenance_snippet.as_deref(),
            Some("Rust compiler snippet")
        );
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
}
