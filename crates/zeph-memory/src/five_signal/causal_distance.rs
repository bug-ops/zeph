// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::HashMap;
use std::sync::Arc;

use zeph_common::memory::EdgeType;

use crate::graph::GraphStore;

/// Causal distance computer backed by MAGMA graph BFS.
///
/// Computes the shortest causal-edge hop count between the current goal entity and each
/// candidate entity. BFS is bounded by `max_depth` to satisfy NFR-003. Results are cached
/// per goal entity id to avoid re-traversal within the same turn.
pub struct CausalDistanceComputer {
    graph_store: Arc<GraphStore>,
    max_depth: u32,
    neutral_distance: u32,
    /// Last BFS result: `(goal_entity_id, depth_map)`.
    cache: Option<(i64, HashMap<i64, u32>)>,
}

impl CausalDistanceComputer {
    /// Create a new computer.
    ///
    /// # Parameters
    ///
    /// - `max_depth`: BFS hop limit (default: 10).
    /// - `neutral_distance`: distance assigned to unreachable entities (default: 5).
    #[must_use]
    pub fn new(graph_store: Arc<GraphStore>, max_depth: u32, neutral_distance: u32) -> Self {
        Self {
            graph_store,
            max_depth,
            neutral_distance,
            cache: None,
        }
    }

    /// Compute causal distances from `goal_entity_id` to each entity in `entity_ids`.
    ///
    /// Returns a map of `entity_id → causal distance` where unreachable or missing entities
    /// receive `neutral_distance`. When `goal_entity_id` is `None`, returns an empty map
    /// (callers treat absent entries as neutral, contributing zero to the signal per FR-006).
    ///
    /// BFS result is cached per `goal_entity_id`; the cache is invalidated only when
    /// the goal entity changes.
    ///
    /// # Errors
    ///
    /// Returns an error if the graph BFS query fails.
    pub async fn compute(
        &mut self,
        goal_entity_id: Option<i64>,
        entity_ids: &[i64],
    ) -> Result<HashMap<i64, u32>, crate::error::MemoryError> {
        tracing::debug!("five_signal: computing causal distances");

        let Some(goal_id) = goal_entity_id else {
            return Ok(HashMap::new());
        };

        let neutral = self.neutral_distance;
        let depth_map = self.ensure_cache(goal_id).await?;

        let result = entity_ids
            .iter()
            .map(|&eid| {
                let dist = depth_map.get(&eid).copied().unwrap_or(neutral);
                (eid, dist)
            })
            .collect();

        Ok(result)
    }

    /// Convert a raw causal distance to a score in `[0.0, 1.0]`.
    ///
    /// Distance 1 → 1.0, distance 5 → 0.2, `neutral_distance` → neutral value.
    /// Distance 0 (goal entity itself) → 1.0 (clamped).
    #[must_use]
    #[inline]
    pub fn distance_to_score(distance: u32) -> f64 {
        if distance == 0 {
            1.0
        } else {
            (1.0_f64 / f64::from(distance)).min(1.0)
        }
    }

    /// Invalidate the BFS cache. Call at turn boundaries when the goal entity may change.
    pub fn invalidate_cache(&mut self) {
        self.cache = None;
    }

    async fn ensure_cache(
        &mut self,
        goal_id: i64,
    ) -> Result<&HashMap<i64, u32>, crate::error::MemoryError> {
        if self.cache.as_ref().map(|(id, _)| *id) != Some(goal_id) {
            let (_, _, depth_map) = self
                .graph_store
                .bfs_typed(goal_id, self.max_depth, &[EdgeType::Causal])
                .await?;
            self.cache = Some((goal_id, depth_map));
        }
        Ok(&self.cache.as_ref().expect("just set above").1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distance_to_score_values() {
        assert!((CausalDistanceComputer::distance_to_score(0) - 1.0).abs() < 1e-9);
        assert!((CausalDistanceComputer::distance_to_score(1) - 1.0).abs() < 1e-9);
        assert!((CausalDistanceComputer::distance_to_score(2) - 0.5).abs() < 1e-9);
        assert!((CausalDistanceComputer::distance_to_score(5) - 0.2).abs() < 1e-9);
    }

    #[test]
    fn distance_to_score_beyond_max_depth_clamped_to_min() {
        // Scores decrease as distance grows and never exceed 1.0.
        let score_at_limit = CausalDistanceComputer::distance_to_score(10);
        let score_beyond = CausalDistanceComputer::distance_to_score(20);
        assert!(score_at_limit <= 1.0);
        assert!(score_beyond <= score_at_limit, "deeper nodes score lower");
        assert!((score_at_limit - 0.1).abs() < 1e-9);
        assert!((score_beyond - 0.05).abs() < 1e-9);
    }

    #[test]
    fn neutral_distance_determines_unreachable_score() {
        // Unreachable entities receive neutral_distance (default 5) → score = 1/5 = 0.2.
        let neutral = 5_u32;
        let score = CausalDistanceComputer::distance_to_score(neutral);
        assert!((score - 0.2).abs() < 1e-9);
    }
}
