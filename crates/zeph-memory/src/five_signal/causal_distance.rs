// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use zeph_common::memory::EdgeType;

use crate::graph::GraphStore;
use crate::types::{EntityId, MessageId};

/// Causal distance computer backed by MAGMA graph BFS.
///
/// Computes the shortest causal-edge hop count between the current goal entity and each
/// candidate entity. BFS is bounded by `max_depth` to satisfy NFR-003. Results are cached
/// per goal entity id to avoid re-traversal within the same turn.
///
/// The cache is guarded by a `std::sync::Mutex` (held only for synchronous reads/writes),
/// so `compute` takes `&self` and the BFS query runs without any lock held, preventing
/// serialization of concurrent recall operations (#5107).
pub struct CausalDistanceComputer {
    graph_store: Arc<GraphStore>,
    max_depth: u32,
    neutral_distance: u32,
    /// Last BFS result: `(goal_entity_id, depth_map)`.
    cache: Mutex<Option<(EntityId, HashMap<EntityId, u32>)>>,
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
            cache: Mutex::new(None),
        }
    }

    /// Compute causal distances from `goal_entity_id` to each entity in `entity_ids`.
    ///
    /// Returns a map of `entity_id → causal distance` where unreachable or missing entities
    /// receive `neutral_distance`. When `goal_entity_id` is `None`, returns an empty map
    /// (callers treat absent entries as neutral, contributing zero to the signal per FR-006).
    ///
    /// BFS result is cached per `goal_entity_id`; the cache is invalidated only when
    /// the goal entity changes. The cache lock is held only during synchronous operations;
    /// the BFS I/O runs lock-free to avoid serializing concurrent recall calls (#5107).
    ///
    /// # Panics
    ///
    /// Panics if the internal cache mutex is poisoned (another thread panicked while
    /// holding the lock — unrecoverable state).
    ///
    /// # Errors
    ///
    /// Returns an error if the graph BFS query fails.
    #[tracing::instrument(
        name = "memory.five_signal.causal_distance.compute",
        skip(self, entity_ids),
        fields(goal_entity_id, candidate_count = entity_ids.len())
    )]
    pub async fn compute(
        &self,
        goal_entity_id: Option<EntityId>,
        entity_ids: &[EntityId],
    ) -> Result<HashMap<EntityId, u32>, crate::error::MemoryError> {
        tracing::debug!("five_signal: computing causal distances");

        let Some(goal_id) = goal_entity_id else {
            return Ok(HashMap::new());
        };

        let neutral = self.neutral_distance;

        // Phase 1: check cache (sync, lock acquired and released immediately).
        let cached = {
            let guard = self.cache.lock().expect("causal cache lock poisoned");
            guard.as_ref().and_then(|(id, map)| {
                if *id == goal_id {
                    Some(
                        entity_ids
                            .iter()
                            .map(|&eid| (eid, map.get(&eid).copied().unwrap_or(neutral)))
                            .collect::<HashMap<EntityId, u32>>(),
                    )
                } else {
                    None
                }
            })
        };

        if let Some(result) = cached {
            return Ok(result);
        }

        // Phase 2: BFS runs without holding any lock.
        let (_, _, depth_map) = self
            .graph_store
            .bfs_typed(goal_id, self.max_depth, &[EdgeType::Causal])
            .await?;

        // Phase 3: store result and build output (sync, lock acquired and released immediately).
        let result = {
            let mut guard = self.cache.lock().expect("causal cache lock poisoned");
            let result = entity_ids
                .iter()
                .map(|&eid| (eid, depth_map.get(&eid).copied().unwrap_or(neutral)))
                .collect();
            *guard = Some((goal_id, depth_map));
            result
        };

        Ok(result)
    }

    /// Compute causal distances from `goal` to each message through the entities its edges touch.
    ///
    /// Message ids and entity ids are distinct id spaces, so each message is first resolved to
    /// the entities of its active edges; its distance is the minimum over those entities.
    /// Messages without any resolved entity are absent from the result (callers treat absence
    /// as neutral). When `goal` is `None` this returns an empty map before touching the store.
    ///
    /// # Errors
    ///
    /// Returns an error if the entity resolution or the BFS query fails.
    pub async fn causal_distances(
        &self,
        goal: Option<EntityId>,
        message_ids: &[MessageId],
    ) -> Result<HashMap<MessageId, u32>, crate::error::MemoryError> {
        let Some(goal) = goal else {
            return Ok(HashMap::new());
        };
        let by_message = self
            .graph_store
            .entity_ids_for_messages(message_ids)
            .await?;
        let entities: Vec<EntityId> = by_message.values().flatten().copied().collect();
        if entities.is_empty() {
            return Ok(HashMap::new());
        }
        let distances = self.compute(Some(goal), &entities).await?;
        Ok(by_message
            .into_iter()
            .filter_map(|(message, entities)| {
                entities
                    .iter()
                    .filter_map(|entity| distances.get(entity).copied())
                    .min()
                    .map(|distance| (message, distance))
            })
            .collect())
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
    ///
    /// # Panics
    ///
    /// Panics if the internal cache mutex is poisoned.
    pub fn invalidate_cache(&self) {
        let mut guard = self.cache.lock().expect("causal cache lock poisoned");
        *guard = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "sqlite")]
    use crate::graph::types::EdgeType;

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

    // Regression test for #4405: goal_entity_id=None returns empty map without touching the DB.
    // Hardcodes `sqlx::SqlitePool` and is not portable to PostgreSQL; skipped entirely
    // when `postgres` is the active backend (see issue #5364).
    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn compute_none_goal_returns_empty_map() {
        use std::sync::Arc;

        // Build a minimal in-memory graph store so the constructor is satisfied.
        let pool = sqlx::SqlitePool::connect(":memory:").await.unwrap();
        let graph_store = Arc::new(crate::graph::GraphStore::new(pool));
        let computer = CausalDistanceComputer::new(graph_store, 10, 5);

        let result = computer
            .compute(
                None,
                &[
                    EntityId::for_test(1),
                    EntityId::for_test(2),
                    EntityId::for_test(3),
                ],
            )
            .await
            .expect("None goal must not fail");
        assert!(
            result.is_empty(),
            "goal_entity_id=None must return empty map, got: {result:?}"
        );
    }

    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn causal_distances_resolve_messages_through_entities() {
        use crate::graph::types::EntityType;

        let store = crate::store::SqliteStore::new(":memory:").await.unwrap();
        let gs = Arc::new(GraphStore::new(store.pool().clone()));
        let conversation = store.create_conversation().await.unwrap();
        let linked = store
            .save_message(conversation, "user", "m1")
            .await
            .unwrap();
        let unrelated = store
            .save_message(conversation, "user", "m2")
            .await
            .unwrap();
        let mut ids = Vec::new();
        for name in ["goal", "a", "d", "b", "c"] {
            ids.push(
                gs.upsert_entity(name, name, EntityType::Concept, None, None)
                    .await
                    .unwrap(),
            );
        }
        let [goal, a, d, b, c] = ids[..] else {
            unreachable!("five entities were inserted")
        };
        let edge = |from, to, kind, episode| {
            let gs = Arc::clone(&gs);
            async move {
                gs.insert_edge_typed(from, to, "rel", "fact", 0.9, episode, kind, None, None)
                    .await
                    .unwrap();
            }
        };
        edge(goal, a, EdgeType::Causal, None).await;
        edge(a, d, EdgeType::Causal, Some(linked)).await;
        edge(b, c, EdgeType::Semantic, Some(unrelated)).await;

        assert_eq!(
            unrelated.0,
            a.get(),
            "premise: the unrelated message id must collide with entity a"
        );
        let computer = CausalDistanceComputer::new(gs, 10, 5);
        let distances = computer
            .causal_distances(Some(goal), &[linked, unrelated])
            .await
            .unwrap();

        assert_eq!(distances.get(&linked), Some(&1));
        assert_eq!(
            distances.get(&unrelated),
            Some(&5),
            "message {unrelated} shares its numeric id with entity {a}; it must not be read as that entity"
        );
    }

    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn causal_distances_touch_store_only_with_goal() {
        let pool = sqlx::SqlitePool::connect(":memory:").await.unwrap();
        pool.close().await;
        let computer = CausalDistanceComputer::new(Arc::new(GraphStore::new(pool)), 10, 5);
        let messages = [MessageId(1)];

        let none = computer.causal_distances(None, &messages).await;
        assert!(none.expect("None goal must not query the store").is_empty());

        let some = computer
            .causal_distances(Some(EntityId::for_test(1)), &messages)
            .await;
        assert!(some.is_err(), "Some goal must query the closed pool");
    }
}
