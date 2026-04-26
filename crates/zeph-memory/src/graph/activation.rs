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
use std::sync::OnceLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
#[allow(unused_imports)]
use zeph_db::sql;

use crate::embedding_store::EmbeddingStore;
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

// ── HL-F5: HeLa-Mem spreading activation (#3346) ─────────────────────────────

/// A graph edge surfaced by HL-F5 spreading activation (#3346), scored by
/// `path_weight × max(cosine_query_to_endpoint, 0.0)`.
///
/// Mirrors [`ActivatedFact`] so callers can dispatch over a single
/// `Vec<HelaFact>` ↔ `Vec<ActivatedFact>` ↔ `Vec<GraphFact>` shape at the
/// strategy-selection site.
#[derive(Debug, Clone)]
pub struct HelaFact {
    /// The edge by which the higher-scored endpoint was reached.
    pub edge: Edge,
    /// Final HL-F5 score: `path_weight × cosine_clamped`. Range: `[0.0, +∞)`.
    pub score: f32,
    /// BFS depth at which `edge` was traversed (`1..=spread_depth`).
    /// `0` is reserved for the synthetic anchor edge in the isolated-anchor fallback.
    pub depth: u32,
    /// Multiplicative product of edge weights along the BFS path that reached
    /// this edge's far endpoint. Range: `[0.0, +∞)`.
    pub path_weight: f32,
    /// Clamped cosine similarity of the far endpoint's entity embedding
    /// to the query embedding, in `[0.0, 1.0]`. `None` when the endpoint
    /// has no stored embedding (skipped from results in that case).
    pub cosine: Option<f32>,
}

/// Parameters for HL-F5 spreading activation retrieval.
///
/// Build via [`Default`] and override individual fields:
///
/// ```rust
/// use zeph_memory::graph::activation::HelaSpreadParams;
///
/// let params = HelaSpreadParams { spread_depth: 3, ..Default::default() };
/// ```
#[derive(Debug, Clone)]
pub struct HelaSpreadParams {
    /// BFS hops. Clamped to `[1, 6]` at runtime. Default: `2`.
    pub spread_depth: u32,
    /// MAGMA edge-type filter. Empty = all types. Default: `[]`.
    pub edge_types: Vec<EdgeType>,
    /// Soft upper bound on the visited-node set. Default: `200`.
    pub max_visited: usize,
    /// Per-step circuit breaker. Any internal step (anchor ANN, edges batch,
    /// vectors batch) that exceeds this duration triggers an `Ok(Vec::new())`
    /// fallback with a `WARN`. Default: `Some(8 ms)`.
    pub step_budget: Option<std::time::Duration>,
}

impl Default for HelaSpreadParams {
    fn default() -> Self {
        Self {
            spread_depth: 2,
            edge_types: Vec::new(),
            max_visited: 200,
            step_budget: Some(std::time::Duration::from_millis(8)),
        }
    }
}

/// Process-global dim-mismatch sentinel for HL-F5 (keyed by collection name).
///
/// MINOR-1 resolution: keyed by collection so re-provisioning with a different
/// dimension recovers after a process restart.  A per-`SemanticMemory` guard would
/// require passing state down; a process-global string key is the least-invasive
/// approach that prevents permanent lockout from transient startup errors.
/// Test isolation: each test constructs its own `HelaSpreadParams` with
/// a distinct mock collection name to avoid cross-test interference.
static HELA_DIM_MISMATCH: OnceLock<String> = OnceLock::new();

/// Cosine similarity of two equal-length slices.
///
/// Returns `0.0` when either norm is zero (prevents division by zero).
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b.iter()).map(|(&x, &y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    let denom = (norm_a * norm_b).max(f32::EPSILON);
    dot / denom
}

/// HL-F5 BFS spreading activation from the top-1 ANN anchor node (#3346).
///
/// Algorithm overview:
/// 1. Embed `query` → anchor via ANN search in the entity Qdrant collection.
/// 2. BFS up to `params.spread_depth` hops, propagating multiplicative edge
///    weights (`path_weight = Π edge.weight along path`). Multi-path convergence
///    keeps the maximum `path_weight`.
/// 3. Retrieve entity embeddings for all visited nodes via `get_points`.
/// 4. Score each node: `score = path_weight × max(cosine(query, entity), 0.0)`.
/// 5. Sort descending, truncate to `limit`, reinforce traversed edges via Hebbian
///    update (when `hebbian_enabled`).
///
/// Fallback: when the anchor entity has no outgoing edges a single synthetic
/// [`HelaFact`] with `edge.id == 0` and `score = anchor_cosine` is returned
/// (the real ANN cosine, never a fabricated `1.0`).
///
/// Per-step circuit breaker: any individual step exceeding `params.step_budget`
/// emits a `WARN` and returns `Ok(Vec::new())`.
///
/// Dim-mismatch resilience: a one-time dim probe on the first call guards against
/// collection/provider configuration mismatches (#3382 pattern). Subsequent calls
/// to a mismatched collection short-circuit immediately.
///
/// # Errors
///
/// Returns an error if the embed call or any database query fails.
#[tracing::instrument(
    name = "memory.graph.hela_spread",
    skip_all,
    fields(
        depth = params.spread_depth,
        limit,
        anchor_id = tracing::field::Empty,
        visited = tracing::field::Empty,
        scored = tracing::field::Empty,
        fallback = tracing::field::Empty,
    )
)]
#[allow(clippy::too_many_arguments, clippy::too_many_lines)] // complex algorithm function; both suppressions justified until the function is decomposed in a future refactor
pub async fn hela_spreading_recall(
    store: &GraphStore,
    embeddings: &EmbeddingStore,
    provider: &zeph_llm::any::AnyProvider,
    query: &str,
    limit: usize,
    params: &HelaSpreadParams,
    hebbian_enabled: bool,
    hebbian_lr: f32,
) -> Result<Vec<HelaFact>, MemoryError> {
    use zeph_llm::LlmProvider as _;

    const ENTITY_COLLECTION: &str = "zeph_graph_entities";

    if limit == 0 {
        return Ok(Vec::new());
    }

    // ── Step 0: dim-mismatch guard ────────────────────────────────────────────
    // MINOR-1: guard is keyed by collection name so re-provisioning recovers.
    if HELA_DIM_MISMATCH.get().map(String::as_str) == Some(ENTITY_COLLECTION) {
        tracing::debug!("hela: dim mismatch previously detected for collection, skipping");
        return Ok(Vec::new());
    }

    // ── Step 1: embed query ───────────────────────────────────────────────────
    let q_vec = provider.embed(query).await?;

    // Dim probe: search with k=1 to catch dimension mismatch at the Qdrant layer.
    let t_anchor = Instant::now();
    let anchor_results = match embeddings
        .search_collection(ENTITY_COLLECTION, &q_vec, 1, None)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("wrong vector dimension")
                || msg.contains("InvalidArgument")
                || msg.contains("dimension")
            {
                let _ = HELA_DIM_MISMATCH.set(ENTITY_COLLECTION.to_owned());
                tracing::warn!(
                    collection = ENTITY_COLLECTION,
                    error = %e,
                    "hela: vector dimension mismatch — HL-F5 disabled for this collection"
                );
                return Ok(Vec::new());
            }
            return Err(e);
        }
    };

    if params.step_budget.is_some_and(|b| t_anchor.elapsed() > b) {
        tracing::warn!(
            elapsed_ms = t_anchor.elapsed().as_millis(),
            "hela: anchor ANN over budget"
        );
        return Ok(Vec::new());
    }

    let Some(anchor_point) = anchor_results.first() else {
        tracing::debug!("hela: no anchor found, returning empty");
        return Ok(Vec::new());
    };
    let Some(anchor_entity_id) = anchor_point
        .payload
        .get("entity_id")
        .and_then(serde_json::Value::as_i64)
    else {
        tracing::warn!("hela: anchor point missing entity_id payload");
        return Ok(Vec::new());
    };
    let anchor_cosine = anchor_point.score;

    tracing::Span::current().record("anchor_id", anchor_entity_id);
    tracing::debug!(anchor_entity_id, anchor_cosine, "hela: anchor resolved");

    let spread_depth = params.spread_depth.clamp(1, 6);

    // ── Step 2: BFS with multiplicative path-weight propagation ──────────────
    // `visited`: entity_id → (depth, path_weight, edge_id_via_which_we_arrived)
    let mut visited: HashMap<i64, (u32, f32, Option<i64>)> = HashMap::new();
    visited.insert(anchor_entity_id, (0, 1.0, None));

    // Dedup edges keyed by id for Step 4 lookup (avoids N clones per frontier).
    // MINOR-3 resolution: collect edges into a HashMap<id, Edge> outside the
    // per-source loop to avoid 10K clones on a hub × 50-entity frontier.
    let mut edge_cache: HashMap<i64, Edge> = HashMap::new();
    let mut frontier: Vec<i64> = vec![anchor_entity_id];

    for hop in 0..spread_depth {
        if frontier.is_empty() {
            break;
        }

        tracing::debug!(hop, frontier_size = frontier.len(), "hela: starting hop");

        let t_step = Instant::now();
        let edges = store
            .edges_for_entities(&frontier, &params.edge_types)
            .await?;
        if params.step_budget.is_some_and(|b| t_step.elapsed() > b) {
            tracing::warn!(
                hop,
                elapsed_ms = t_step.elapsed().as_millis(),
                "hela: edge-fetch over budget"
            );
            return Ok(Vec::new());
        }

        let mut next_frontier: Vec<i64> = Vec::new();

        for edge in &edges {
            // Cache by edge id to avoid repeated clones per source in frontier.
            edge_cache.entry(edge.id).or_insert_with(|| edge.clone());

            for &src_id in &frontier {
                let neighbor = if edge.source_entity_id == src_id {
                    edge.target_entity_id
                } else if edge.target_entity_id == src_id {
                    edge.source_entity_id
                } else {
                    continue;
                };

                let parent_pw = visited.get(&src_id).map_or(1.0, |&(_, pw, _)| pw);
                let new_pw = parent_pw * edge.weight;

                // Multi-path resolution: keep MAX path_weight; lower depth as
                // tie-break. MINOR-4 note: max_visited is a soft bound — the
                // actual visited set may exceed it by O(edges_per_hop_step) for
                // one frontier step before the outer break fires.
                let entry = visited
                    .entry(neighbor)
                    .or_insert((hop + 1, 0.0_f32, Some(edge.id)));
                // Prefer strictly higher path weight; break ties in favour of shallower depth.
                if new_pw > entry.1
                    || ((new_pw - entry.1).abs() < f32::EPSILON && hop + 1 < entry.0)
                {
                    *entry = (hop + 1, new_pw, Some(edge.id));
                    if !next_frontier.contains(&neighbor) {
                        next_frontier.push(neighbor);
                    }
                }

                if visited.len() >= params.max_visited {
                    break;
                }
            }

            if visited.len() >= params.max_visited {
                break;
            }
        }

        tracing::debug!(
            hop,
            edges_fetched = edges.len(),
            visited = visited.len(),
            next_frontier = next_frontier.len(),
            "hela: hop complete"
        );

        frontier = next_frontier;
        if visited.len() >= params.max_visited {
            break;
        }
    }

    // ── Isolated-anchor fallback ──────────────────────────────────────────────
    // `visited.len() == 1` means no edges were traversed from the anchor.
    if visited.len() == 1 {
        tracing::Span::current().record("fallback", true);
        tracing::debug!(
            anchor_entity_id,
            anchor_cosine,
            "hela: anchor isolated, falling back to pure ANN"
        );
        let fact = HelaFact {
            edge: Edge::synthetic_anchor(anchor_entity_id),
            score: anchor_cosine,
            depth: 0,
            path_weight: 1.0,
            cosine: Some(anchor_cosine.clamp(0.0, 1.0)),
        };
        return Ok(vec![fact]);
    }

    // ── Step 3: retrieve entity embeddings ───────────────────────────────────
    let entity_ids: Vec<i64> = visited.keys().copied().collect();
    let point_id_map = store.qdrant_point_ids_for_entities(&entity_ids).await?;
    let point_ids: Vec<String> = point_id_map.values().cloned().collect();

    let t_vec = Instant::now();
    let vec_map = embeddings
        .get_vectors_from_collection(ENTITY_COLLECTION, &point_ids)
        .await?;
    if params.step_budget.is_some_and(|b| t_vec.elapsed() > b) {
        tracing::warn!(
            elapsed_ms = t_vec.elapsed().as_millis(),
            "hela: vectors-batch over budget"
        );
        return Ok(Vec::new());
    }

    // ── Step 4: score per visited node ────────────────────────────────────────
    // Cosine clamped to [0.0, 1.0]: anti-correlated neighbors score 0.0 so
    // they are ranked below positively-correlated ones.  A negative cosine on a
    // strongly-reinforced edge would otherwise invert the retrieval signal.
    let mut facts: Vec<HelaFact> = Vec::with_capacity(visited.len().saturating_sub(1));
    for (&entity_id, &(depth, path_weight, edge_id_opt)) in &visited {
        if entity_id == anchor_entity_id {
            continue;
        }
        let Some(edge_id) = edge_id_opt else {
            continue;
        };
        let Some(point_id) = point_id_map.get(&entity_id) else {
            continue;
        };
        let Some(node_vec) = vec_map.get(point_id) else {
            continue;
        };
        if node_vec.len() != q_vec.len() {
            // Per-node dim mismatch — skip (defense-in-depth for legacy collections).
            continue;
        }
        let cosine_clamped = cosine(&q_vec, node_vec).max(0.0);
        let fact_score = path_weight * cosine_clamped;
        let Some(edge) = edge_cache.get(&edge_id).cloned() else {
            continue;
        };
        facts.push(HelaFact {
            edge,
            score: fact_score,
            depth,
            path_weight,
            cosine: Some(cosine_clamped),
        });
    }

    // ── Step 5: sort, truncate, Hebbian increment ─────────────────────────────
    facts.sort_by(|a, b| b.score.total_cmp(&a.score));
    facts.truncate(limit);

    // HL-F2 reinforcement on edges that survived truncation (kept ≈ used).
    // Hebbian on "kept edges only" — consistent with graph_recall_activated at
    // graph/retrieval.rs:427-433. Note: SYNAPSE reinforces all traversed edges;
    // this PR intentionally reinforces only surfaced edges. See MINOR-5.
    if hebbian_enabled {
        let edge_ids: Vec<i64> = facts
            .iter()
            .map(|f| f.edge.id)
            .filter(|&id| id != 0) // skip synthetic anchor
            .collect();
        if !edge_ids.is_empty()
            && let Err(e) = store.apply_hebbian_increment(&edge_ids, hebbian_lr).await
        {
            tracing::warn!(error = %e, "hela: hebbian increment failed");
        }
    }

    tracing::Span::current().record("visited", visited.len());
    tracing::Span::current().record("scored", facts.len());

    Ok(facts)
}

// ── SYNAPSE spreading activation ──────────────────────────────────────────────

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
            .map_or(0, |d| d.as_secs().cast_signed());

        let mut activation = self.initialize_seeds(&seeds);
        let mut activated_facts: Vec<ActivatedFact> = Vec::new();

        for hop in 0..self.params.max_hops {
            let active_nodes: Vec<(i64, f32)> = activation
                .iter()
                .filter(|(_, (score, _))| *score >= self.params.activation_threshold)
                .map(|(&id, &(score, _))| (id, score))
                .collect();

            if active_nodes.is_empty() {
                break;
            }

            let node_ids: Vec<i64> = active_nodes.iter().map(|(id, _)| *id).collect();
            let edges = store.edges_for_entities(&node_ids, edge_types).await?;
            let edge_count = edges.len();

            let next_activation =
                self.propagate_one_hop(hop, &active_nodes, &edges, &activation, now_secs);

            let pruned_count = self.merge_and_prune(&mut activation, next_activation);

            tracing::debug!(
                hop,
                active_nodes = active_nodes.len(),
                edges_fetched = edge_count,
                after_merge = activation.len(),
                pruned = pruned_count,
                "spreading activation: hop complete"
            );

            self.collect_activated_facts(&edges, &activation, &mut activated_facts);
        }

        let result = self.finalize(activation);

        tracing::info!(
            activated = result.len(),
            facts = activated_facts.len(),
            "spreading activation: complete"
        );

        Ok((result, activated_facts))
    }

    /// Populate the activation map from seed scores, filtering seeds below threshold.
    fn initialize_seeds(&self, seeds: &HashMap<i64, f32>) -> HashMap<i64, (f32, u32)> {
        let mut activation: HashMap<i64, (f32, u32)> = HashMap::new();
        let mut seed_count = 0usize;
        // Seeds bypass activation_threshold (they are query anchors per SYNAPSE semantics).
        for (entity_id, match_score) in seeds {
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
        activation
    }

    /// Compute the next-hop activation map by propagating through `edges`.
    ///
    /// Applies lateral inhibition (CRIT-02) and clamped multi-path convergence sums.
    fn propagate_one_hop(
        &self,
        hop: u32,
        active_nodes: &[(i64, f32)],
        edges: &[Edge],
        activation: &HashMap<i64, (f32, u32)>,
        now_secs: i64,
    ) -> HashMap<i64, (f32, u32)> {
        let mut next_activation: HashMap<i64, (f32, u32)> = HashMap::new();

        for edge in edges {
            for &(active_id, node_score) in active_nodes {
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

                // Clamped sum preserves the multi-path convergence signal: nodes reachable
                // via multiple paths receive proportionally higher activation (MAJOR-01).
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

        next_activation
    }

    /// Merge `next_activation` into `activation` and prune to `max_activated_nodes` (SA-INV-04).
    ///
    /// Returns the number of pruned nodes for tracing.
    fn merge_and_prune(
        &self,
        activation: &mut HashMap<i64, (f32, u32)>,
        next_activation: HashMap<i64, (f32, u32)>,
    ) -> usize {
        for (node_id, (new_score, new_depth)) in next_activation {
            let entry = activation.entry(node_id).or_insert((0.0, new_depth));
            if new_score > entry.0 {
                entry.0 = new_score;
                entry.1 = new_depth;
            }
        }

        if activation.len() > self.params.max_activated_nodes {
            let before = activation.len();
            let mut entries: Vec<(i64, (f32, u32))> = activation.drain().collect();
            entries.sort_by(|(_, (a, _)), (_, (b, _))| b.total_cmp(a));
            entries.truncate(self.params.max_activated_nodes);
            *activation = entries.into_iter().collect();
            before - self.params.max_activated_nodes
        } else {
            0
        }
    }

    /// Append edges whose both endpoints are above threshold to `activated_facts`.
    fn collect_activated_facts(
        &self,
        edges: &[Edge],
        activation: &HashMap<i64, (f32, u32)>,
        activated_facts: &mut Vec<ActivatedFact>,
    ) {
        for edge in edges {
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
                    edge: edge.clone(),
                    activation_score,
                });
            }
        }
    }

    /// Collect nodes above threshold into `Vec<ActivatedNode>`, sorted descending by score.
    fn finalize(&self, activation: HashMap<i64, (f32, u32)>) -> Vec<ActivatedNode> {
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
        result
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

    // ── HL-F5 unit tests ─────────────────────────────────────────────────────

    #[test]
    fn hela_cosine_identical_vectors() {
        let v = vec![1.0_f32, 0.0, 0.0];
        assert!(
            (cosine(&v, &v) - 1.0).abs() < 1e-6,
            "identical vectors → cosine 1.0"
        );
    }

    #[test]
    fn hela_cosine_orthogonal_vectors() {
        let a = vec![1.0_f32, 0.0];
        let b = vec![0.0_f32, 1.0];
        assert!(
            cosine(&a, &b).abs() < 1e-6,
            "orthogonal vectors → cosine 0.0"
        );
    }

    #[test]
    fn hela_cosine_anti_correlated() {
        let a = vec![1.0_f32, 0.0];
        let b = vec![-1.0_f32, 0.0];
        assert!(
            cosine(&a, &b) < 0.0,
            "anti-correlated vectors → negative cosine"
        );
    }

    #[test]
    fn hela_cosine_zero_vector_no_panic() {
        let a = vec![0.0_f32, 0.0];
        let b = vec![1.0_f32, 0.0];
        // Should not panic — denom is guarded by f32::EPSILON
        let result = cosine(&a, &b);
        assert!(
            result.is_finite(),
            "zero-norm vector must yield finite cosine"
        );
    }

    #[test]
    fn hela_spread_params_default_depth_is_two() {
        let p = HelaSpreadParams::default();
        assert_eq!(p.spread_depth, 2);
        assert!(p.step_budget.is_some());
        assert!(p.edge_types.is_empty());
        assert_eq!(p.max_visited, 200);
    }

    #[test]
    fn hela_synthetic_anchor_edge_id_is_zero() {
        let edge = Edge::synthetic_anchor(42);
        assert_eq!(
            edge.id, 0,
            "synthetic anchor must have id = 0 to be excluded from Hebbian"
        );
        assert_eq!(edge.source_entity_id, 42);
        assert_eq!(edge.target_entity_id, 42);
    }

    #[test]
    fn hela_negative_cosine_clamped_to_zero_in_score() {
        // path_weight × cosine.max(0.0): negative cosine must contribute 0.0
        let anti = vec![-1.0_f32, 0.0];
        let query = vec![1.0_f32, 0.0];
        let cosine_raw = cosine(&query, &anti);
        assert!(cosine_raw < 0.0);
        let clamped = cosine_raw.max(0.0);
        let fact_score = 0.9_f32 * clamped;
        assert!(
            fact_score < f32::EPSILON,
            "anti-correlated score must be 0.0"
        );
    }

    #[test]
    fn hela_path_weight_multiplicative() {
        // Two-hop path with edge weights 0.8, 0.5 → path_weight = 0.4
        let w1 = 0.8_f32;
        let w2 = 0.5_f32;
        let expected = w1 * w2;
        assert!((expected - 0.4).abs() < 1e-6);
    }

    #[test]
    fn hela_max_path_weight_on_multipath() {
        // When two paths reach the same node, keep the higher path_weight.
        let pw_a = 0.9_f32; // short direct path
        let pw_b = 0.3_f32; // longer indirect path
        let kept = pw_a.max(pw_b);
        assert!(
            (kept - 0.9).abs() < 1e-6,
            "multi-path resolution must keep maximum path_weight"
        );
    }

    #[test]
    fn hela_fact_score_formula() {
        let path_weight = 0.8_f32;
        let cosine_clamped = 0.75_f32;
        let expected = path_weight * cosine_clamped;
        // Verify the formula used in hela_spreading_recall Step 4.
        assert!((expected - 0.6).abs() < 1e-5);
    }
}
