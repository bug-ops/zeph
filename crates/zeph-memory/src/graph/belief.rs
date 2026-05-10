// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Pre-commitment probabilistic edge layer for the APEX-MEM knowledge graph.
//!
//! [`BeliefStore`] implements a staging area for candidate facts that lack sufficient
//! confidence for immediate commitment to the committed `graph_edges` store.
//! Evidence events for the same `(source, canonical_relation, target, edge_type)` tuple
//! are accumulated via the Noisy-OR rule. When the cumulative probability crosses
//! [`BeliefMemConfig::promote_threshold`], the caller should promote the belief to a
//! committed edge via `GraphStore::insert_or_supersede`.
//!
//! # Relationship to APEX-MEM
//!
//! - APEX-MEM conflict resolution operates **post-commitment** (multiple committed heads).
//! - `BeliefStore` operates **pre-commitment** (accumulates evidence before the first commit).
//! - Promotion from `BeliefStore` → APEX-MEM uses the standard `insert_or_supersede` path.
//!
//! # Key invariants
//!
//! - `prob` is monotonically non-decreasing for an active (non-promoted) belief.
//! - Promotion is one-way: once `promoted_at` is set, the belief never re-enters pending.
//! - Retrieval from `pending_beliefs` is a fallback: only used when no committed edge exists.
//! - Noisy-OR guarantees `prob ∈ (0, 1)` given inputs in `(0, 1)`.

use tracing::instrument;
use zeph_db::{DbPool, sql};

use crate::error::MemoryError;
use crate::graph::types::EdgeType;

// ── Pure functions ────────────────────────────────────────────────────────────

/// Combine two independent evidence probabilities via the Noisy-OR rule.
///
/// Noisy-OR models independent failure modes: `P(A ∨ B) = 1 − (1 − p_a)(1 − p_b)`.
/// The result is always strictly greater than either input and strictly less than 1.
///
/// Both arguments must be in the open interval `(0.0, 1.0)`.
///
/// # Examples
///
/// ```
/// use zeph_memory::graph::belief::noisy_or;
///
/// let combined = noisy_or(0.4, 0.5);
/// assert!((combined - 0.7).abs() < 1e-6);
/// ```
#[inline]
#[must_use]
pub fn noisy_or(p_existing: f32, p_new: f32) -> f32 {
    debug_assert!(
        p_existing > 0.0 && p_existing < 1.0,
        "p_existing out of range: {p_existing}"
    );
    debug_assert!(p_new > 0.0 && p_new < 1.0, "p_new out of range: {p_new}");
    1.0 - (1.0 - p_existing) * (1.0 - p_new)
}

/// Apply exponential temporal decay to a probability.
///
/// Used before applying a new Noisy-OR update to discount stale evidence:
/// `p_decayed = p * exp(-λ * days)`.
///
/// - `prob`: current probability in `(0, 1)`.
/// - `days_since_update`: elapsed time in fractional days (may be 0.0).
/// - `decay_rate`: λ (0.01 by default in [`BeliefMemConfig`]).
///
/// Returns a value clamped to `(0.0, 1.0)`.
///
/// # Examples
///
/// ```
/// use zeph_memory::graph::belief::time_decayed_prob;
///
/// // 30 days at λ=0.01 → multiplier ≈ 0.74
/// let decayed = time_decayed_prob(0.8, 30.0, 0.01);
/// assert!(decayed < 0.8);
/// assert!(decayed > 0.0);
/// ```
#[inline]
#[must_use]
pub fn time_decayed_prob(prob: f32, days_since_update: f64, decay_rate: f32) -> f32 {
    #[allow(clippy::cast_possible_truncation)]
    let multiplier = (-f64::from(decay_rate) * days_since_update).exp() as f32;
    (prob * multiplier).clamp(f32::MIN_POSITIVE, 1.0 - f32::EPSILON)
}

// ── Types ─────────────────────────────────────────────────────────────────────

/// A candidate edge that has not yet crossed the promotion threshold.
///
/// Stored in `pending_beliefs`. Evidence events accumulate via Noisy-OR until
/// `prob >= BeliefMemConfig::promote_threshold`, at which point the caller promotes
/// the belief to a committed `graph_edges` row.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingBelief {
    /// Unique row identifier.
    pub id: i64,
    /// Source entity (`graph_entities.id`).
    pub source_entity_id: i64,
    /// Target entity (`graph_entities.id`).
    pub target_entity_id: i64,
    /// Original relation verb as extracted from the message.
    pub relation: String,
    /// Normalised relation used for deduplication and indexing.
    pub canonical_relation: String,
    /// Human-readable sentence summarising the relationship.
    pub fact: String,
    /// MAGMA edge type.
    pub edge_type: EdgeType,
    /// Current cumulative probability in `(0.0, 1.0)`.
    pub prob: f32,
    /// Episode the most recent evidence came from.
    pub episode_id: Option<String>,
    /// Unix timestamp (seconds) of the first evidence event.
    pub created_at: i64,
    /// Unix timestamp (seconds) of the most recent Noisy-OR update.
    pub updated_at: i64,
}

/// A single Noisy-OR update event recorded in `belief_evidence`.
///
/// Provides a complete audit trail of how each belief's probability evolved.
#[derive(Debug, Clone)]
pub struct BeliefEvidence {
    /// Unique row identifier.
    pub id: i64,
    /// The belief this event belongs to.
    pub belief_id: i64,
    /// Probability before this update (after temporal decay if configured).
    pub prior_prob: f32,
    /// Probability of the new evidence signal (from the extractor's `confidence` field).
    pub evidence_prob: f32,
    /// Probability after applying Noisy-OR: `1 - (1 - prior)(1 - evidence)`.
    pub posterior_prob: f32,
    /// Episode the evidence came from.
    pub episode_id: Option<String>,
    /// Unix timestamp (seconds) when this evidence was recorded.
    pub created_at: i64,
}

/// Configuration for the probabilistic belief layer.
///
/// Embed in `[memory.graph.belief_mem]` in `config.toml`. All thresholds are
/// dimensionless probabilities in `[0.0, 1.0]`.
#[derive(Debug, Clone)]
pub struct BeliefMemConfig {
    /// Whether the feature is enabled. Default: `false`.
    pub enabled: bool,
    /// Minimum probability for a new fact to enter `pending_beliefs`.
    /// Evidence below this is discarded. Default: `0.3`.
    pub min_entry_prob: f32,
    /// Promotion threshold: when `prob >= promote_threshold`, the belief is
    /// returned from [`BeliefStore::record_evidence`] for the caller to commit.
    /// Default: `0.85`.
    pub promote_threshold: f32,
    /// Eviction cap: maximum `pending_beliefs` rows per `(source, canonical_relation)`
    /// group. Oldest low-probability beliefs are evicted when exceeded. Default: `10`.
    pub max_candidates_per_group: usize,
    /// Number of candidates returned by [`BeliefStore::retrieve_candidates`].
    /// Default: `3`.
    pub retrieval_top_k: usize,
    /// Exponential decay rate λ applied to existing probability before each Noisy-OR
    /// update. Set to `0.0` to disable temporal decay. Default: `0.01`.
    pub belief_decay_rate: f32,
}

impl Default for BeliefMemConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            min_entry_prob: 0.3,
            promote_threshold: 0.85,
            max_candidates_per_group: 10,
            retrieval_top_k: 3,
            belief_decay_rate: 0.01,
        }
    }
}

// ── BeliefStore ───────────────────────────────────────────────────────────────

/// Persistence layer for the pre-commitment probabilistic edge layer.
///
/// All mutations go through this type: creating new beliefs, applying Noisy-OR
/// evidence updates, marking beliefs as promoted, and evicting stale candidates.
///
/// Obtain an instance via [`BeliefStore::new`] after running the `zeph-db` migrations.
pub struct BeliefStore {
    pool: DbPool,
    config: BeliefMemConfig,
}

impl BeliefStore {
    /// Create a new `BeliefStore` wrapping `pool` with the given configuration.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use zeph_memory::graph::belief::{BeliefStore, BeliefMemConfig};
    /// use zeph_db::DbPool;
    ///
    /// async fn example(pool: DbPool) {
    ///     let store = BeliefStore::new(pool, BeliefMemConfig::default());
    /// }
    /// ```
    #[must_use]
    pub fn new(pool: DbPool, config: BeliefMemConfig) -> Self {
        Self { pool, config }
    }

    /// Record new evidence for a candidate edge and apply Noisy-OR accumulation.
    ///
    /// If a matching `pending_belief` exists for the same `(source_entity_id,
    /// target_entity_id, canonical_relation, edge_type)` tuple, this method:
    /// 1. Applies optional temporal decay to the existing probability.
    /// 2. Combines the decayed probability with `evidence_prob` via Noisy-OR.
    /// 3. Persists the update and appends a row to `belief_evidence`.
    ///
    /// If no matching belief exists and `evidence_prob >= min_entry_prob`, a new
    /// belief row is created.
    ///
    /// Returns `Some(PendingBelief)` when the updated probability crosses
    /// `promote_threshold`. The **caller** is responsible for calling
    /// `GraphStore::insert_or_supersede` to commit the promoted belief, then
    /// calling [`BeliefStore::mark_promoted`] to record the committed edge ID.
    ///
    /// Returns `None` when the belief exists but has not yet crossed the threshold,
    /// or when `evidence_prob < min_entry_prob` and no prior belief existed.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError`] for database failures.
    #[allow(clippy::too_many_arguments)]
    #[instrument(
        name = "memory.graph.belief.record_evidence",
        skip(self, fact, episode_id),
        fields(source_entity_id, target_entity_id, canonical_relation, evidence_prob)
    )]
    pub async fn record_evidence(
        &self,
        source_entity_id: i64,
        target_entity_id: i64,
        relation: &str,
        canonical_relation: &str,
        fact: &str,
        edge_type: EdgeType,
        evidence_prob: f32,
        episode_id: Option<&str>,
    ) -> Result<Option<PendingBelief>, MemoryError> {
        if !self.config.enabled {
            return Ok(None);
        }
        if evidence_prob < self.config.min_entry_prob
            || evidence_prob <= 0.0
            || evidence_prob >= 1.0
        {
            return Ok(None);
        }

        let edge_type_str = edge_type.as_str();

        // Check for an existing belief row.
        let existing = self
            .find_existing(
                source_entity_id,
                target_entity_id,
                canonical_relation,
                edge_type_str,
            )
            .await?;

        let belief = match existing {
            Some(row) => {
                self.apply_evidence_update(row, evidence_prob, episode_id)
                    .await?
            }
            None => {
                self.insert_new_belief(
                    source_entity_id,
                    target_entity_id,
                    relation,
                    canonical_relation,
                    fact,
                    edge_type_str,
                    evidence_prob,
                    episode_id,
                )
                .await?
            }
        };

        // Evict stale candidates to stay within the per-group cap.
        self.evict_stale(source_entity_id, canonical_relation)
            .await?;

        if belief.prob >= self.config.promote_threshold {
            Ok(Some(belief))
        } else {
            Ok(None)
        }
    }

    /// Retrieve the top-K unpromoted beliefs for a `(source, canonical_relation)` pair,
    /// ordered by probability descending.
    ///
    /// This is a fallback for graph recall: called only when no committed edge exists.
    /// Results are annotated by the caller as uncertain (`is_uncertain: true`).
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError`] for database failures.
    #[instrument(
        name = "memory.graph.belief.retrieve_candidates",
        skip(self),
        fields(source_entity_id, canonical_relation)
    )]
    pub async fn retrieve_candidates(
        &self,
        source_entity_id: i64,
        canonical_relation: &str,
        top_k: Option<usize>,
    ) -> Result<Vec<PendingBelief>, MemoryError> {
        #[allow(clippy::cast_possible_wrap)]
        let limit = top_k.unwrap_or(self.config.retrieval_top_k) as i64;

        let rows: Vec<BeliefRow> = zeph_db::query_as(sql!(
            "SELECT id, source_entity_id, target_entity_id, relation, canonical_relation,
                    fact, edge_type, prob, episode_id, created_at, updated_at
             FROM pending_beliefs
             WHERE source_entity_id = ?
               AND canonical_relation = ?
               AND promoted_at IS NULL
             ORDER BY prob DESC
             LIMIT ?"
        ))
        .bind(source_entity_id)
        .bind(canonical_relation)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter().map(belief_from_row).collect()
    }

    /// Mark a belief as promoted and record the committed edge ID.
    ///
    /// Sets `promoted_at` to the current Unix timestamp and stores `committed_edge_id`
    /// so the belief audit trail links to the committed graph edge.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError`] for database failures.
    #[instrument(
        name = "memory.graph.belief.mark_promoted",
        skip(self),
        fields(belief_id, committed_edge_id)
    )]
    pub async fn mark_promoted(
        &self,
        belief_id: i64,
        committed_edge_id: i64,
    ) -> Result<(), MemoryError> {
        zeph_db::query(sql!(
            "UPDATE pending_beliefs
             SET promoted_at = unixepoch(), promoted_edge_id = ?
             WHERE id = ?"
        ))
        .bind(committed_edge_id)
        .bind(belief_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Evict old low-probability beliefs for a `(source, canonical_relation)` group
    /// that exceed [`BeliefMemConfig::max_candidates_per_group`].
    ///
    /// The `max_candidates_per_group` highest-probability beliefs are retained;
    /// the rest are deleted. Returns the number of rows deleted.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError`] for database failures.
    pub async fn evict_stale(
        &self,
        source_entity_id: i64,
        canonical_relation: &str,
    ) -> Result<usize, MemoryError> {
        #[allow(clippy::cast_possible_wrap)]
        let cap = self.config.max_candidates_per_group as i64;

        // NOT IN (subquery) is safe here because `cap` is bounded by
        // `max_candidates_per_group` (default 10), so the subquery result set is small.
        // SQLite's query planner uses the covering index idx_pending_beliefs_retrieval for
        // the inner SELECT, making this O(cap) rather than a full-table scan.
        let deleted = zeph_db::query(sql!(
            "DELETE FROM pending_beliefs
             WHERE source_entity_id = ?
               AND canonical_relation = ?
               AND promoted_at IS NULL
               AND id NOT IN (
                   SELECT id FROM pending_beliefs
                   WHERE source_entity_id = ?
                     AND canonical_relation = ?
                     AND promoted_at IS NULL
                   ORDER BY prob DESC
                   LIMIT ?
               )"
        ))
        .bind(source_entity_id)
        .bind(canonical_relation)
        .bind(source_entity_id)
        .bind(canonical_relation)
        .bind(cap)
        .execute(&self.pool)
        .await?
        .rows_affected();

        #[allow(clippy::cast_possible_truncation)]
        Ok(deleted as usize)
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    async fn find_existing(
        &self,
        source_entity_id: i64,
        target_entity_id: i64,
        canonical_relation: &str,
        edge_type_str: &str,
    ) -> Result<Option<BeliefRow>, MemoryError> {
        let row: Option<BeliefRow> = zeph_db::query_as(sql!(
            "SELECT id, source_entity_id, target_entity_id, relation, canonical_relation,
                    fact, edge_type, prob, episode_id, created_at, updated_at
             FROM pending_beliefs
             WHERE source_entity_id = ?
               AND target_entity_id = ?
               AND canonical_relation = ?
               AND edge_type = ?
               AND promoted_at IS NULL
             LIMIT 1"
        ))
        .bind(source_entity_id)
        .bind(target_entity_id)
        .bind(canonical_relation)
        .bind(edge_type_str)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    async fn apply_evidence_update(
        &self,
        row: BeliefRow,
        evidence_prob: f32,
        episode_id: Option<&str>,
    ) -> Result<PendingBelief, MemoryError> {
        let prior_prob = if self.config.belief_decay_rate > 0.0 {
            let now_secs = now_unix();
            #[allow(clippy::cast_precision_loss)]
            let days_elapsed = (now_secs - row.updated_at) as f64 / 86_400.0;
            time_decayed_prob(
                row.prob,
                days_elapsed.max(0.0),
                self.config.belief_decay_rate,
            )
        } else {
            row.prob
        };

        let posterior = noisy_or(prior_prob, evidence_prob);

        zeph_db::query(sql!(
            "UPDATE pending_beliefs
             SET prob = ?, updated_at = unixepoch(), episode_id = ?
             WHERE id = ?"
        ))
        .bind(posterior)
        .bind(episode_id)
        .bind(row.id)
        .execute(&self.pool)
        .await?;

        zeph_db::query(sql!(
            "INSERT INTO belief_evidence
                (belief_id, prior_prob, evidence_prob, posterior_prob, episode_id)
             VALUES (?, ?, ?, ?, ?)"
        ))
        .bind(row.id)
        .bind(prior_prob)
        .bind(evidence_prob)
        .bind(posterior)
        .bind(episode_id)
        .execute(&self.pool)
        .await?;

        belief_from_row(BeliefRow {
            prob: posterior,
            updated_at: now_unix(),
            episode_id: episode_id.map(ToOwned::to_owned),
            ..row
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn insert_new_belief(
        &self,
        source_entity_id: i64,
        target_entity_id: i64,
        relation: &str,
        canonical_relation: &str,
        fact: &str,
        edge_type_str: &str,
        evidence_prob: f32,
        episode_id: Option<&str>,
    ) -> Result<PendingBelief, MemoryError> {
        let id: i64 = zeph_db::query_scalar(sql!(
            "INSERT INTO pending_beliefs
                (source_entity_id, target_entity_id, relation, canonical_relation,
                 fact, edge_type, prob, episode_id)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)
             RETURNING id"
        ))
        .bind(source_entity_id)
        .bind(target_entity_id)
        .bind(relation)
        .bind(canonical_relation)
        .bind(fact)
        .bind(edge_type_str)
        .bind(evidence_prob)
        .bind(episode_id)
        .fetch_one(&self.pool)
        .await?;

        let now = now_unix();
        zeph_db::query(sql!(
            "INSERT INTO belief_evidence
                (belief_id, prior_prob, evidence_prob, posterior_prob, episode_id)
             VALUES (?, ?, ?, ?, ?)"
        ))
        .bind(id)
        .bind(0.0_f32)
        .bind(evidence_prob)
        .bind(evidence_prob)
        .bind(episode_id)
        .execute(&self.pool)
        .await?;

        Ok(PendingBelief {
            id,
            source_entity_id,
            target_entity_id,
            relation: relation.to_owned(),
            canonical_relation: canonical_relation.to_owned(),
            fact: fact.to_owned(),
            edge_type: edge_type_str.parse::<EdgeType>().unwrap_or_default(),
            prob: evidence_prob,
            episode_id: episode_id.map(ToOwned::to_owned),
            created_at: now,
            updated_at: now,
        })
    }
}

// ── Database row mapping ──────────────────────────────────────────────────────

#[derive(sqlx::FromRow)]
struct BeliefRow {
    id: i64,
    source_entity_id: i64,
    target_entity_id: i64,
    relation: String,
    canonical_relation: String,
    fact: String,
    edge_type: String,
    prob: f32,
    episode_id: Option<String>,
    created_at: i64,
    updated_at: i64,
}

fn belief_from_row(row: BeliefRow) -> Result<PendingBelief, MemoryError> {
    let edge_type = row.edge_type.parse::<EdgeType>().map_err(|e| {
        MemoryError::GraphStore(format!("invalid edge_type '{}': {e}", row.edge_type))
    })?;
    Ok(PendingBelief {
        id: row.id,
        source_entity_id: row.source_entity_id,
        target_entity_id: row.target_entity_id,
        relation: row.relation,
        canonical_relation: row.canonical_relation,
        fact: row.fact,
        edge_type,
        prob: row.prob,
        episode_id: row.episode_id,
        created_at: row.created_at,
        updated_at: row.updated_at,
    })
}

fn now_unix() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    #[allow(clippy::cast_possible_wrap)]
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noisy_or_combines_correctly() {
        // 1 - (1 - 0.4)(1 - 0.5) = 1 - 0.6 * 0.5 = 0.7
        let result = noisy_or(0.4, 0.5);
        assert!((result - 0.7).abs() < 1e-6, "got {result}");
    }

    #[test]
    fn noisy_or_is_bounded() {
        let result = noisy_or(0.9, 0.9);
        assert!(result < 1.0);
        assert!(result > 0.9);
    }

    #[test]
    fn noisy_or_accumulates_above_threshold() {
        // Six evidence events at 0.3 each should exceed 0.85 (from critic M4 scenario)
        let mut p = 0.3_f32;
        for _ in 1..6 {
            p = noisy_or(p, 0.3);
        }
        assert!(p >= 0.85, "accumulated prob {p} did not reach 0.85");
    }

    #[test]
    fn time_decayed_prob_reduces_value() {
        let original = 0.8_f32;
        let decayed = time_decayed_prob(original, 30.0, 0.01);
        assert!(decayed < original);
        assert!(decayed > 0.0);
    }

    #[test]
    fn time_decayed_prob_zero_days_unchanged() {
        let original = 0.7_f32;
        let decayed = time_decayed_prob(original, 0.0, 0.01);
        assert!((decayed - original).abs() < 1e-5);
    }

    #[test]
    fn time_decayed_prob_zero_rate_unchanged() {
        let original = 0.6_f32;
        let decayed = time_decayed_prob(original, 100.0, 0.0);
        assert!((decayed - original).abs() < 1e-5);
    }

    #[test]
    fn time_decayed_prob_stays_in_bounds() {
        let decayed = time_decayed_prob(0.99, 10_000.0, 1.0);
        assert!(decayed > 0.0);
        assert!(decayed < 1.0);
    }

    #[test]
    fn belief_mem_config_defaults() {
        let cfg = BeliefMemConfig::default();
        assert!(!cfg.enabled);
        assert!((cfg.min_entry_prob - 0.3).abs() < 1e-6);
        assert!((cfg.promote_threshold - 0.85).abs() < 1e-6);
        assert_eq!(cfg.max_candidates_per_group, 10);
        assert_eq!(cfg.retrieval_top_k, 3);
        assert!((cfg.belief_decay_rate - 0.01).abs() < 1e-6);
    }
}
