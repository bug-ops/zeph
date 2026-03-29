// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! SYNAPSE spreading activation retrieval over the entity graph.
//!
//! Implements the spreading activation algorithm from arXiv 2601.02744, adapted for
//! the zeph-memory graph schema. Seeds are matched via fuzzy entity search; activation
//! propagates hop-by-hop with:
//! - Exponential decay per hop (`decay_lambda`)
//! - Edge confidence weighting
//! - Temporal recency weighting (reuses `GraphConfig.temporal_decay_rate`)
//! - Lateral inhibition (nodes above `inhibition_threshold` stop receiving activation)
//! - Per-hop pruning to enforce `max_activated_nodes` bound (SA-INV-04)
//! - MAGMA edge type filtering via `edge_types` parameter

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};
#[allow(unused_imports)]
use zeph_db::sql;

use crate::error::MemoryError;
use crate::graph::store::GraphStore;
use crate::graph::types::{Edge, EdgeType, edge_type_weight, evolved_weight};

/// A graph node that was activated during spreading activation.
#[derive(Debug, Clone)]
pub struct ActivatedNode {
    /// Database ID of the activated entity.
    pub entity_id: i64,
    /// Final activation score in `[0.0, 1.0]`.
    pub activation: f32,
    /// Hop at which the maximum activation was received (`0` = seed).
    pub depth: u32,
}

/// A graph edge traversed during spreading activation, with its activation score.
#[derive(Debug, Clone)]
pub struct ActivatedFact {
    /// The traversed edge.
    pub edge: Edge,
    /// Activation score of the source or target entity at time of traversal.
    pub activation_score: f32,
}

/// Parameters for spreading activation. Mirrors `SpreadingActivationConfig` but lives
/// in `zeph-memory` so the crate does not depend on `zeph-config`.
#[derive(Debug, Clone)]
pub struct SpreadingActivationParams {
    pub decay_lambda: f32,
    pub max_hops: u32,
    pub activation_threshold: f32,
    pub inhibition_threshold: f32,
    pub max_activated_nodes: usize,
    pub temporal_decay_rate: f64,
    /// Weight of structural score in hybrid seed ranking. Range: [0.0, 1.0]. Default: 0.4.
    pub seed_structural_weight: f32,
    /// Maximum seeds per community ID. 0 = unlimited. Default: 3.
    pub seed_community_cap: usize,
}

/// Spreading activation engine parameterized from [`SpreadingActivationParams`].
pub struct SpreadingActivation {
    params: SpreadingActivationParams,
}

impl SpreadingActivation {
    /// Create a new spreading activation engine from explicit parameters.
    ///
    /// `params.temporal_decay_rate` is taken from `GraphConfig.temporal_decay_rate` so that
    /// recency weighting reuses the same parameter as BFS recall (SA-INV-05).
    #[must_use]
    pub fn new(params: SpreadingActivationParams) -> Self {
        Self { params }
    }

    /// Run spreading activation from `seeds` over the graph.
    ///
    /// Returns activated nodes sorted by activation score descending, along with
    /// edges collected during propagation.
    ///
    /// # Parameters
    ///
    /// - `store`: graph database accessor
    /// - `seeds`: `HashMap<entity_id, initial_activation>` — nodes to start from
    /// - `edge_types`: MAGMA subgraph filter; when non-empty, only edges of these types
    ///   are traversed (mirrors `bfs_typed` behaviour; SA-INV-08)
    ///
    /// # Errors
    ///
    /// Returns an error if any database query fails.
    #[allow(clippy::too_many_lines)]
    pub async fn spread(
        &self,
        store: &GraphStore,
        seeds: HashMap<i64, f32>,
        edge_types: &[EdgeType],
    ) -> Result<(Vec<ActivatedNode>, Vec<ActivatedFact>), MemoryError> {
        if seeds.is_empty() {
            return Ok((Vec::new(), Vec::new()));
        }

        // Compute `now_secs` once for consistent temporal recency weighting
        // across all edges (matches the pattern in retrieval.rs:83-86).
        let now_secs: i64 = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs().cast_signed())
            .unwrap_or(0);

        // activation map: entity_id -> (score, depth_at_max)
        let mut activation: HashMap<i64, (f32, u32)> = HashMap::new();

        // Phase 1: seed initialization — seeds bypass activation_threshold (they are
        // query anchors per SYNAPSE semantics). Filter below-threshold seeds with a debug log.
        let mut seed_count = 0usize;
        for (entity_id, match_score) in &seeds {
            if *match_score < self.params.activation_threshold {
                tracing::debug!(
                    entity_id,
                    score = match_score,
                    threshold = self.params.activation_threshold,
                    "spreading activation: seed below threshold, skipping"
                );
                continue;
            }
            activation.insert(*entity_id, (*match_score, 0));
            seed_count += 1;
        }

        tracing::debug!(
            seeds = seed_count,
            "spreading activation: initialized seeds"
        );

        // Collected activated facts (edges traversed with their activation scores).
        let mut activated_facts: Vec<ActivatedFact> = Vec::new();

        // Phase 2: iterative propagation
        for hop in 0..self.params.max_hops {
            // Collect nodes eligible for propagation this hop.
            let active_nodes: Vec<(i64, f32)> = activation
                .iter()
                .filter(|(_, (score, _))| *score >= self.params.activation_threshold)
                .map(|(&id, &(score, _))| (id, score))
                .collect();

            if active_nodes.is_empty() {
                break;
            }

            let node_ids: Vec<i64> = active_nodes.iter().map(|(id, _)| *id).collect();

            // Fetch edges for all active nodes in one batched query.
            let edges = store.edges_for_entities(&node_ids, edge_types).await?;
            let edge_count = edges.len();

            let mut next_activation: HashMap<i64, (f32, u32)> = HashMap::new();

            for edge in &edges {
                // Determine which endpoint is the "source" (currently active) and
                // which is the "neighbor" to receive activation.
                for &(active_id, node_score) in &active_nodes {
                    let neighbor = if edge.source_entity_id == active_id {
                        edge.target_entity_id
                    } else if edge.target_entity_id == active_id {
                        edge.source_entity_id
                    } else {
                        continue;
                    };

                    // Lateral inhibition: skip neighbor if it already has high activation
                    // in either the current map OR this hop's next_activation (CRIT-02 fix:
                    // checks both maps to match SYNAPSE paper semantics and prevent runaway
                    // activation when multiple paths converge in the same hop).
                    let current_score = activation.get(&neighbor).map_or(0.0_f32, |&(s, _)| s);
                    let next_score = next_activation.get(&neighbor).map_or(0.0_f32, |&(s, _)| s);
                    if current_score >= self.params.inhibition_threshold
                        || next_score >= self.params.inhibition_threshold
                    {
                        continue;
                    }

                    let recency = self.recency_weight(&edge.valid_from, now_secs);
                    let edge_weight = evolved_weight(edge.retrieval_count, edge.confidence);
                    let type_w = edge_type_weight(edge.edge_type);
                    let spread_value =
                        node_score * self.params.decay_lambda * edge_weight * recency * type_w;

                    if spread_value < self.params.activation_threshold {
                        continue;
                    }

                    // Use clamped sum (min(1.0, existing + spread_value)) to preserve the
                    // multi-path convergence signal: nodes reachable via multiple paths
                    // receive proportionally higher activation (see MAJOR-01 in critic review).
                    let depth_at_max = hop + 1;
                    let entry = next_activation
                        .entry(neighbor)
                        .or_insert((0.0, depth_at_max));
                    let new_score = (entry.0 + spread_value).min(1.0);
                    if new_score > entry.0 {
                        entry.0 = new_score;
                        entry.1 = depth_at_max;
                    }
                }
            }

            // Merge next_activation into activation (keep max depth-at-max for ties).
            for (node_id, (new_score, new_depth)) in next_activation {
                let entry = activation.entry(node_id).or_insert((0.0, new_depth));
                if new_score > entry.0 {
                    entry.0 = new_score;
                    entry.1 = new_depth;
                }
            }

            // Per-hop pruning: enforce max_activated_nodes (SA-INV-04).
            // After merging, if |activation| > max_activated_nodes, keep only top-N by score.
            let pruned_count = if activation.len() > self.params.max_activated_nodes {
                let before = activation.len();
                let mut entries: Vec<(i64, (f32, u32))> = activation.drain().collect();
                entries.sort_by(|(_, (a, _)), (_, (b, _))| b.total_cmp(a));
                entries.truncate(self.params.max_activated_nodes);
                activation = entries.into_iter().collect();
                before - self.params.max_activated_nodes
            } else {
                0
            };

            tracing::debug!(
                hop,
                active_nodes = active_nodes.len(),
                edges_fetched = edge_count,
                after_merge = activation.len(),
                pruned = pruned_count,
                "spreading activation: hop complete"
            );

            // Collect edges from this hop as activated facts.
            for edge in edges {
                // Include only edges connecting two activated nodes.
                let src_score = activation
                    .get(&edge.source_entity_id)
                    .map_or(0.0, |&(s, _)| s);
                let tgt_score = activation
                    .get(&edge.target_entity_id)
                    .map_or(0.0, |&(s, _)| s);
                if src_score >= self.params.activation_threshold
                    && tgt_score >= self.params.activation_threshold
                {
                    let activation_score = src_score.max(tgt_score);
                    activated_facts.push(ActivatedFact {
                        edge,
                        activation_score,
                    });
                }
            }
        }

        // Phase 3: collect nodes above threshold, sorted by activation score descending.
        let mut result: Vec<ActivatedNode> = activation
            .into_iter()
            .filter(|(_, (score, _))| *score >= self.params.activation_threshold)
            .map(|(entity_id, (activation, depth))| ActivatedNode {
                entity_id,
                activation,
                depth,
            })
            .collect();
        result.sort_by(|a, b| b.activation.total_cmp(&a.activation));

        tracing::info!(
            activated = result.len(),
            facts = activated_facts.len(),
            "spreading activation: complete"
        );

        Ok((result, activated_facts))
    }

    /// Compute temporal recency weight for an edge.
    ///
    /// Formula: `1.0 / (1.0 + age_days * temporal_decay_rate)`.
    /// Returns `1.0` when `temporal_decay_rate = 0.0` (no temporal adjustment).
    /// Reuses the same formula as `GraphFact::score_with_decay` (SA-INV-05).
    #[allow(clippy::cast_precision_loss)]
    fn recency_weight(&self, valid_from: &str, now_secs: i64) -> f32 {
        if self.params.temporal_decay_rate <= 0.0 {
            return 1.0;
        }
        let Some(valid_from_secs) = parse_sqlite_datetime_to_unix(valid_from) else {
            return 1.0;
        };
        let age_secs = (now_secs - valid_from_secs).max(0);
        let age_days = age_secs as f64 / 86_400.0;
        let weight = 1.0_f64 / (1.0 + age_days * self.params.temporal_decay_rate);
        // cast f64 -> f32: safe, weight is in [0.0, 1.0]
        #[allow(clippy::cast_possible_truncation)]
        let w = weight as f32;
        w
    }
}

/// Parse a `SQLite` `datetime('now')` string to Unix seconds.
///
/// Accepts `"YYYY-MM-DD HH:MM:SS"` (and variants with fractional seconds or timezone suffix).
/// Returns `None` if the string cannot be parsed.
#[must_use]
fn parse_sqlite_datetime_to_unix(s: &str) -> Option<i64> {
    if s.len() < 19 {
        return None;
    }
    let year: i64 = s[0..4].parse().ok()?;
    let month: i64 = s[5..7].parse().ok()?;
    let day: i64 = s[8..10].parse().ok()?;
    let hour: i64 = s[11..13].parse().ok()?;
    let min: i64 = s[14..16].parse().ok()?;
    let sec: i64 = s[17..19].parse().ok()?;

    // Days since Unix epoch via civil calendar algorithm.
    // Reference: https://howardhinnant.github.io/date_algorithms.html#days_from_civil
    let (y, m) = if month <= 2 {
        (year - 1, month + 9)
    } else {
        (year, month - 3)
    };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * m + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;

    Some(days * 86_400 + hour * 3_600 + min * 60 + sec)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::GraphStore;
    use crate::graph::types::EntityType;
    use crate::store::SqliteStore;

    async fn setup_store() -> GraphStore {
        let store = SqliteStore::new(":memory:").await.unwrap();
        GraphStore::new(store.pool().clone())
    }

    fn default_params() -> SpreadingActivationParams {
        SpreadingActivationParams {
            decay_lambda: 0.85,
            max_hops: 3,
            activation_threshold: 0.1,
            inhibition_threshold: 0.8,
            max_activated_nodes: 50,
            temporal_decay_rate: 0.0,
            seed_structural_weight: 0.4,
            seed_community_cap: 3,
        }
    }

    // Test 1: empty graph (no edges) — seed entity is still returned as activated node,
    // but no facts (edges) are found. Spread does not validate entity existence in DB.
    #[tokio::test]
    async fn spread_empty_graph_no_edges_no_facts() {
        let store = setup_store().await;
        let sa = SpreadingActivation::new(default_params());
        let seeds = HashMap::from([(1_i64, 1.0_f32)]);
        let (nodes, facts) = sa.spread(&store, seeds, &[]).await.unwrap();
        // Seed node is returned as activated (activation=1.0, depth=0).
        assert_eq!(nodes.len(), 1, "seed must be in activated nodes");
        assert_eq!(nodes[0].entity_id, 1);
        assert!((nodes[0].activation - 1.0).abs() < 1e-6);
        // No edges in empty graph, so no ActivatedFacts.
        assert!(
            facts.is_empty(),
            "expected no activated facts on empty graph"
        );
    }

    // Test 2: empty seeds returns empty
    #[tokio::test]
    async fn spread_empty_seeds_returns_empty() {
        let store = setup_store().await;
        let sa = SpreadingActivation::new(default_params());
        let (nodes, facts) = sa.spread(&store, HashMap::new(), &[]).await.unwrap();
        assert!(nodes.is_empty());
        assert!(facts.is_empty());
    }

    // Test 3: single seed with no edges returns only the seed
    #[tokio::test]
    async fn spread_single_seed_no_edges_returns_seed() {
        let store = setup_store().await;
        let alice = store
            .upsert_entity("Alice", "Alice", EntityType::Person, None)
            .await
            .unwrap();

        let sa = SpreadingActivation::new(default_params());
        let seeds = HashMap::from([(alice, 1.0_f32)]);
        let (nodes, _) = sa.spread(&store, seeds, &[]).await.unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].entity_id, alice);
        assert_eq!(nodes[0].depth, 0);
        assert!((nodes[0].activation - 1.0).abs() < 1e-6);
    }

    // Test 4: linear chain A->B->C with max_hops=3 — all activated, scores decay
    #[tokio::test]
    async fn spread_linear_chain_all_activated_with_decay() {
        let store = setup_store().await;
        let a = store
            .upsert_entity("A", "A", EntityType::Person, None)
            .await
            .unwrap();
        let b = store
            .upsert_entity("B", "B", EntityType::Person, None)
            .await
            .unwrap();
        let c = store
            .upsert_entity("C", "C", EntityType::Person, None)
            .await
            .unwrap();
        store
            .insert_edge(a, b, "knows", "A knows B", 1.0, None)
            .await
            .unwrap();
        store
            .insert_edge(b, c, "knows", "B knows C", 1.0, None)
            .await
            .unwrap();

        let mut cfg = default_params();
        cfg.max_hops = 3;
        cfg.decay_lambda = 0.9;
        let sa = SpreadingActivation::new(cfg);
        let seeds = HashMap::from([(a, 1.0_f32)]);
        let (nodes, _) = sa.spread(&store, seeds, &[]).await.unwrap();

        let ids: Vec<i64> = nodes.iter().map(|n| n.entity_id).collect();
        assert!(ids.contains(&a), "A (seed) must be activated");
        assert!(ids.contains(&b), "B (hop 1) must be activated");
        assert!(ids.contains(&c), "C (hop 2) must be activated");

        // Scores must decay: score(A) > score(B) > score(C)
        let score_a = nodes.iter().find(|n| n.entity_id == a).unwrap().activation;
        let score_b = nodes.iter().find(|n| n.entity_id == b).unwrap().activation;
        let score_c = nodes.iter().find(|n| n.entity_id == c).unwrap().activation;
        assert!(
            score_a > score_b,
            "seed A should have higher activation than hop-1 B"
        );
        assert!(
            score_b > score_c,
            "hop-1 B should have higher activation than hop-2 C"
        );
    }

    // Test 5: linear chain with max_hops=1 — C not activated
    #[tokio::test]
    async fn spread_linear_chain_max_hops_limits_reach() {
        let store = setup_store().await;
        let a = store
            .upsert_entity("A", "A", EntityType::Person, None)
            .await
            .unwrap();
        let b = store
            .upsert_entity("B", "B", EntityType::Person, None)
            .await
            .unwrap();
        let c = store
            .upsert_entity("C", "C", EntityType::Person, None)
            .await
            .unwrap();
        store
            .insert_edge(a, b, "knows", "A knows B", 1.0, None)
            .await
            .unwrap();
        store
            .insert_edge(b, c, "knows", "B knows C", 1.0, None)
            .await
            .unwrap();

        let mut cfg = default_params();
        cfg.max_hops = 1;
        let sa = SpreadingActivation::new(cfg);
        let seeds = HashMap::from([(a, 1.0_f32)]);
        let (nodes, _) = sa.spread(&store, seeds, &[]).await.unwrap();

        let ids: Vec<i64> = nodes.iter().map(|n| n.entity_id).collect();
        assert!(ids.contains(&a), "A must be activated (seed)");
        assert!(ids.contains(&b), "B must be activated (hop 1)");
        assert!(!ids.contains(&c), "C must NOT be activated with max_hops=1");
    }

    // Test 6: diamond graph — D receives convergent activation from two paths
    // Graph: A -> B, A -> C, B -> D, C -> D
    // With clamped sum, D gets activation from both paths (convergence signal preserved).
    #[tokio::test]
    async fn spread_diamond_graph_convergence() {
        let store = setup_store().await;
        let a = store
            .upsert_entity("A", "A", EntityType::Person, None)
            .await
            .unwrap();
        let b = store
            .upsert_entity("B", "B", EntityType::Person, None)
            .await
            .unwrap();
        let c = store
            .upsert_entity("C", "C", EntityType::Person, None)
            .await
            .unwrap();
        let d = store
            .upsert_entity("D", "D", EntityType::Person, None)
            .await
            .unwrap();
        store
            .insert_edge(a, b, "rel", "A-B", 1.0, None)
            .await
            .unwrap();
        store
            .insert_edge(a, c, "rel", "A-C", 1.0, None)
            .await
            .unwrap();
        store
            .insert_edge(b, d, "rel", "B-D", 1.0, None)
            .await
            .unwrap();
        store
            .insert_edge(c, d, "rel", "C-D", 1.0, None)
            .await
            .unwrap();

        let mut cfg = default_params();
        cfg.max_hops = 3;
        cfg.decay_lambda = 0.9;
        cfg.inhibition_threshold = 0.95; // raise inhibition to allow convergence
        let sa = SpreadingActivation::new(cfg);
        let seeds = HashMap::from([(a, 1.0_f32)]);
        let (nodes, _) = sa.spread(&store, seeds, &[]).await.unwrap();

        let ids: Vec<i64> = nodes.iter().map(|n| n.entity_id).collect();
        assert!(ids.contains(&d), "D must be activated via diamond paths");

        // D should be activated at depth 2
        let node_d = nodes.iter().find(|n| n.entity_id == d).unwrap();
        assert_eq!(node_d.depth, 2, "D should be at depth 2");
    }

    // Test 7: inhibition threshold prevents runaway activation in dense cluster
    #[tokio::test]
    async fn spread_inhibition_prevents_runaway() {
        let store = setup_store().await;
        // Create a hub node connected to many leaves
        let hub = store
            .upsert_entity("Hub", "Hub", EntityType::Concept, None)
            .await
            .unwrap();

        for i in 0..5 {
            let leaf = store
                .upsert_entity(
                    &format!("Leaf{i}"),
                    &format!("Leaf{i}"),
                    EntityType::Concept,
                    None,
                )
                .await
                .unwrap();
            store
                .insert_edge(hub, leaf, "has", &format!("Hub has Leaf{i}"), 1.0, None)
                .await
                .unwrap();
            // Connect all leaves back to hub to create a dense cluster
            store
                .insert_edge(
                    leaf,
                    hub,
                    "part_of",
                    &format!("Leaf{i} part_of Hub"),
                    1.0,
                    None,
                )
                .await
                .unwrap();
        }

        // Seed hub with full activation — it should be inhibited after hop 1
        let mut cfg = default_params();
        cfg.inhibition_threshold = 0.8;
        cfg.max_hops = 3;
        let sa = SpreadingActivation::new(cfg);
        let seeds = HashMap::from([(hub, 1.0_f32)]);
        let (nodes, _) = sa.spread(&store, seeds, &[]).await.unwrap();

        // Hub should remain at initial activation (1.0), not grow unbounded
        let hub_node = nodes.iter().find(|n| n.entity_id == hub);
        assert!(hub_node.is_some(), "hub must be in results");
        assert!(
            hub_node.unwrap().activation <= 1.0,
            "activation must not exceed 1.0"
        );
    }

    // Test 8: max_activated_nodes cap — lowest activations pruned
    #[tokio::test]
    async fn spread_max_activated_nodes_cap_enforced() {
        let store = setup_store().await;
        let root = store
            .upsert_entity("Root", "Root", EntityType::Person, None)
            .await
            .unwrap();

        // Create 20 leaf nodes connected to root
        for i in 0..20 {
            let leaf = store
                .upsert_entity(
                    &format!("Node{i}"),
                    &format!("Node{i}"),
                    EntityType::Concept,
                    None,
                )
                .await
                .unwrap();
            store
                .insert_edge(root, leaf, "has", &format!("Root has Node{i}"), 0.9, None)
                .await
                .unwrap();
        }

        let max_nodes = 5;
        let cfg = SpreadingActivationParams {
            max_activated_nodes: max_nodes,
            max_hops: 2,
            ..default_params()
        };
        let sa = SpreadingActivation::new(cfg);
        let seeds = HashMap::from([(root, 1.0_f32)]);
        let (nodes, _) = sa.spread(&store, seeds, &[]).await.unwrap();

        assert!(
            nodes.len() <= max_nodes,
            "activation must be capped at {max_nodes} nodes, got {}",
            nodes.len()
        );
    }

    // Test 9: temporal decay — recent edges produce higher activation
    #[tokio::test]
    async fn spread_temporal_decay_recency_effect() {
        let store = setup_store().await;
        let src = store
            .upsert_entity("Src", "Src", EntityType::Person, None)
            .await
            .unwrap();
        let recent = store
            .upsert_entity("Recent", "Recent", EntityType::Tool, None)
            .await
            .unwrap();
        let old = store
            .upsert_entity("Old", "Old", EntityType::Tool, None)
            .await
            .unwrap();

        // Insert recent edge (default valid_from = now)
        store
            .insert_edge(src, recent, "uses", "Src uses Recent", 1.0, None)
            .await
            .unwrap();

        // Insert old edge manually with a 1970 timestamp
        zeph_db::query(
            sql!("INSERT INTO graph_edges (source_entity_id, target_entity_id, relation, fact, confidence, valid_from)
             VALUES (?1, ?2, 'uses', 'Src uses Old', 1.0, '1970-01-01 00:00:00')"),
        )
        .bind(src)
        .bind(old)
        .execute(store.pool())
        .await
        .unwrap();

        let mut cfg = default_params();
        cfg.max_hops = 2;
        // Use significant temporal decay rate to distinguish recent vs old
        let sa = SpreadingActivation::new(SpreadingActivationParams {
            temporal_decay_rate: 0.5,
            ..cfg
        });
        let seeds = HashMap::from([(src, 1.0_f32)]);
        let (nodes, _) = sa.spread(&store, seeds, &[]).await.unwrap();

        let score_recent = nodes
            .iter()
            .find(|n| n.entity_id == recent)
            .map_or(0.0, |n| n.activation);
        let score_old = nodes
            .iter()
            .find(|n| n.entity_id == old)
            .map_or(0.0, |n| n.activation);

        assert!(
            score_recent > score_old,
            "recent edge ({score_recent}) must produce higher activation than old edge ({score_old})"
        );
    }

    // Test 10: edge_type filtering — only edges of specified type are traversed
    #[tokio::test]
    async fn spread_edge_type_filter_excludes_other_types() {
        let store = setup_store().await;
        let a = store
            .upsert_entity("A", "A", EntityType::Person, None)
            .await
            .unwrap();
        let b_semantic = store
            .upsert_entity("BSemantic", "BSemantic", EntityType::Tool, None)
            .await
            .unwrap();
        let c_causal = store
            .upsert_entity("CCausal", "CCausal", EntityType::Concept, None)
            .await
            .unwrap();

        // Semantic edge from A
        store
            .insert_edge(a, b_semantic, "uses", "A uses BSemantic", 1.0, None)
            .await
            .unwrap();

        // Causal edge from A (inserted with explicit edge_type)
        zeph_db::query(
            sql!("INSERT INTO graph_edges (source_entity_id, target_entity_id, relation, fact, confidence, valid_from, edge_type)
             VALUES (?1, ?2, 'caused', 'A caused CCausal', 1.0, datetime('now'), 'causal')"),
        )
        .bind(a)
        .bind(c_causal)
        .execute(store.pool())
        .await
        .unwrap();

        let cfg = default_params();
        let sa = SpreadingActivation::new(cfg);

        // Spread with only semantic edges
        let seeds = HashMap::from([(a, 1.0_f32)]);
        let (nodes, _) = sa
            .spread(&store, seeds, &[EdgeType::Semantic])
            .await
            .unwrap();

        let ids: Vec<i64> = nodes.iter().map(|n| n.entity_id).collect();
        assert!(
            ids.contains(&b_semantic),
            "BSemantic must be activated via semantic edge"
        );
        assert!(
            !ids.contains(&c_causal),
            "CCausal must NOT be activated when filtering to semantic only"
        );
    }

    // Test 11: large seed list (stress test for batch query)
    #[tokio::test]
    async fn spread_large_seed_list() {
        let store = setup_store().await;
        let mut seeds = HashMap::new();

        // Create 100 seed entities — tests that edges_for_entities handles chunking correctly
        for i in 0..100i64 {
            let id = store
                .upsert_entity(
                    &format!("Entity{i}"),
                    &format!("entity{i}"),
                    EntityType::Concept,
                    None,
                )
                .await
                .unwrap();
            seeds.insert(id, 1.0_f32);
        }

        let cfg = default_params();
        let sa = SpreadingActivation::new(cfg);
        // Should complete without error even with 100 seeds (chunking handles SQLite limit)
        let result = sa.spread(&store, seeds, &[]).await;
        assert!(
            result.is_ok(),
            "large seed list must not error: {:?}",
            result.err()
        );
    }
}
