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
use zeph_memory::{ConversationId, RecallView as MemRecallView};

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
            Ok(rows
                .into_iter()
                .map(|r| MemPersonaFact {
                    category: r.category,
                    content: r.content,
                })
                .collect())
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
            Ok(rows
                .into_iter()
                .map(|r| MemTrajectoryEntry {
                    intent: r.intent,
                    outcome: r.outcome,
                    confidence: r.confidence,
                })
                .collect())
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
            Ok(rows
                .into_iter()
                .map(|r| MemTreeNode { content: r.content })
                .collect())
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
            Ok(rows
                .into_iter()
                .map(|r| MemSummary {
                    first_message_id: r.first_message_id.map(|m| m.0),
                    last_message_id: r.last_message_id.map(|m| m.0),
                    content: r.content,
                })
                .collect())
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
            Ok(strategies
                .into_iter()
                .map(|s| MemReasoningStrategy {
                    id: s.id,
                    outcome: s.outcome.as_str().to_owned(),
                    summary: s.summary,
                })
                .collect())
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
            Ok(corrections
                .into_iter()
                .map(|c| MemCorrection {
                    correction_text: c.correction_text,
                })
                .collect())
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
            Ok(recalled
                .into_iter()
                .map(|r| {
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
                })
                .collect())
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
            Ok(recalled
                .into_iter()
                .map(|rf| MemGraphFact {
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
                })
                .collect())
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
            Ok(results
                .into_iter()
                .map(|r| MemSessionSummary {
                    summary_text: r.summary_text,
                    score: r.score,
                })
                .collect())
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
