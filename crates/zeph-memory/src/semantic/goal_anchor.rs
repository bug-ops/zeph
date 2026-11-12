// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Goal-to-entity resolution for the causal-distance signal of five-signal retrieval.

use crate::error::MemoryError;
use crate::graph::retrieval::find_seed_entities;
use crate::types::EntityId;

use super::SemanticMemory;

/// Seeds fetched per query word before the top-1 pick.
const ANCHOR_SEED_LIMIT: usize = 5;

impl SemanticMemory {
    /// Resolve the text of the active goal to the graph entity that anchors causal distance.
    ///
    /// Reuses the seed search of graph retrieval (per-word FTS with an embedding fallback) with
    /// `structural_weight = 0.0`, so the pick is driven by relevance only and a high-degree hub
    /// cannot outrank the entity the goal actually names. Ties are broken by the lowest id so the
    /// result is deterministic.
    ///
    /// Returns `Ok(None)` when the graph is disabled or no entity matches.
    ///
    /// # Errors
    ///
    /// Returns an error if a graph query fails.
    #[tracing::instrument(
        name = "memory.semantic.resolve_goal_anchor",
        skip_all,
        fields(goal_len = goal_text.len()),
        err
    )]
    pub async fn resolve_goal_anchor(
        &self,
        goal_text: &str,
    ) -> Result<Option<EntityId>, MemoryError> {
        let Some(store) = &self.graph_store else {
            return Ok(None);
        };
        let seeds = find_seed_entities(
            store,
            self.qdrant.as_deref(),
            &self.provider,
            goal_text,
            ANCHOR_SEED_LIMIT,
            0.0,
            1,
            self.embed_timeout,
        )
        .await?;
        Ok(seeds
            .into_iter()
            .max_by(|(id_a, score_a), (id_b, score_b)| {
                score_a.total_cmp(score_b).then_with(|| id_b.cmp(id_a))
            })
            .map(|(id, _)| id))
    }
}
