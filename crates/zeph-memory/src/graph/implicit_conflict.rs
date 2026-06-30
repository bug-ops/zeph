// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Implicit conflict detection for SYNAPSE recall (spec 004-17, STALE/CUPMem).
//!
//! [`ImplicitConflictDetector`] runs at write time to detect predicate pairs
//! that are semantically similar but not identical, staging them in
//! `implicit_conflict_candidates` for later resolution or annotation.
//!
//! SYNAPSE recall uses [`annotate_conflicts`] to mark retrieved [`ActivatedFact`]s
//! that have pending conflict candidates.

// SQLite limits bind parameters to 999. Each ID is bound twice (two IN clauses),
// so process in chunks of at most 499.
const MAX_IDS_PER_QUERY: usize = 499;

use std::time::{SystemTime, UNIX_EPOCH};

use zeph_config::ImplicitConflictConfig;
use zeph_db::DbTransaction;

use crate::error::MemoryError;
use crate::graph::activation::ActivatedFact;

/// A candidate conflict pair detected at write time.
#[derive(Debug, Clone)]
pub struct ConflictCandidate {
    /// ID of the newly inserted edge.
    pub edge_a_id: i64,
    /// ID of the existing edge with a similar predicate.
    pub edge_b_id: i64,
    /// Similarity score in `[0.0, 1.0]`.
    pub similarity: f64,
    /// The similarity method that produced this candidate.
    pub method: String,
}

/// Write-time implicit conflict detector.
///
/// Compares a new edge's predicate against existing active edges on the same
/// source entity using the configured similarity method and threshold.
///
/// # Examples
///
/// ```rust,no_run
/// use zeph_config::ImplicitConflictConfig;
/// use zeph_memory::graph::implicit_conflict::ImplicitConflictDetector;
///
/// let config = ImplicitConflictConfig { enabled: true, ..Default::default() };
/// let detector = ImplicitConflictDetector::new(config);
/// let candidates = detector.detect_candidates(42, "employ", &[(1, "employs")], false);
/// assert!(!candidates.is_empty());
/// ```
pub struct ImplicitConflictDetector {
    config: ImplicitConflictConfig,
}

impl ImplicitConflictDetector {
    /// Create a new detector with the given configuration.
    #[must_use]
    pub fn new(config: ImplicitConflictConfig) -> Self {
        Self { config }
    }

    /// Detect implicit conflict candidates for a new predicate against existing ones.
    ///
    /// Returns pairs where normalized Levenshtein similarity is
    /// `>= conflict_similarity_threshold` **and** the predicates differ (identical
    /// predicates are already handled by APEX-MEM explicit supersession).
    ///
    /// Returns an empty vec when `enabled = false` or when the cardinality flag
    /// `is_cardinality_n` is set (FR-011).
    ///
    /// # Arguments
    ///
    /// * `new_edge_id` — database ID of the newly inserted edge
    /// * `new_predicate` — canonical relation of the new edge
    /// * `existing` — slice of `(edge_id, canonical_relation)` for all other active
    ///   edges on the same source entity
    /// * `is_cardinality_n` — set to `true` for multi-valued predicates; skips detection
    #[must_use]
    pub fn detect_candidates(
        &self,
        new_edge_id: i64,
        new_predicate: &str,
        existing: &[(i64, &str)],
        is_cardinality_n: bool,
    ) -> Vec<ConflictCandidate> {
        let _span = tracing::info_span!(
            "memory.graph.implicit_conflict.detect",
            predicate = new_predicate,
        )
        .entered();

        if !self.config.enabled || is_cardinality_n || existing.is_empty() {
            return Vec::new();
        }

        let threshold = self.config.conflict_similarity_threshold;
        let mut candidates = Vec::new();

        for &(edge_id, predicate) in existing {
            if predicate == new_predicate {
                // Identical predicate: handled by APEX-MEM explicit supersession.
                continue;
            }
            let sim = Self::normalized_levenshtein(new_predicate, predicate);
            if sim >= threshold {
                candidates.push(ConflictCandidate {
                    edge_a_id: new_edge_id,
                    edge_b_id: edge_id,
                    similarity: sim,
                    method: "levenshtein".to_owned(),
                });
            }
        }

        candidates
    }

    /// Persist conflict candidates into `implicit_conflict_candidates`.
    ///
    /// Each candidate is inserted with `status = 'pending'` and an expiry of
    /// `now + ttl_days * 86400` seconds.
    ///
    /// # Errors
    ///
    /// Returns a [`MemoryError`] on database write failure.
    #[tracing::instrument(skip_all, name = "memory.graph.implicit_conflict.stage", fields(count = candidates.len()))]
    pub async fn stage_candidates(
        &self,
        candidates: &[ConflictCandidate],
        tx: &mut DbTransaction<'_>,
        ttl_days: u32,
    ) -> Result<(), MemoryError> {
        if candidates.is_empty() {
            return Ok(());
        }

        #[allow(clippy::cast_possible_wrap)]
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let ttl_secs = i64::from(ttl_days) * 86_400;
        let expires_at = now + ttl_secs;

        for c in candidates {
            sqlx::query(
                "INSERT INTO implicit_conflict_candidates
                 (edge_a_id, edge_b_id, similarity, method, status, created_at, expires_at)
                 VALUES (?, ?, ?, ?, 'pending', ?, ?)",
            )
            .bind(c.edge_a_id)
            .bind(c.edge_b_id)
            .bind(c.similarity)
            .bind(&c.method)
            .bind(now)
            .bind(expires_at)
            .execute(&mut **tx)
            .await
            .map_err(MemoryError::from)?;
        }

        Ok(())
    }

    /// Compute normalized Levenshtein similarity between two strings.
    ///
    /// Returns a value in `[0.0, 1.0]` where `1.0` means identical.
    /// Returns `1.0` if both strings are empty, `0.0` if only one is empty.
    #[must_use]
    pub fn normalized_levenshtein(a: &str, b: &str) -> f64 {
        if a == b {
            return 1.0;
        }
        let len_a = a.chars().count();
        let len_b = b.chars().count();
        if len_a == 0 && len_b == 0 {
            return 1.0;
        }
        if len_a == 0 || len_b == 0 {
            return 0.0;
        }
        let dist = levenshtein_distance(a, b);
        let max_len = len_a.max(len_b);
        #[allow(clippy::cast_precision_loss)]
        let result = 1.0 - (dist as f64 / max_len as f64);
        result
    }

    /// Returns `true` when detection is enabled.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }

    /// Returns the configured TTL for conflict candidates, in days.
    #[must_use]
    pub fn candidate_ttl_days(&self) -> u32 {
        self.config.candidate_ttl_days
    }
}

/// Annotate retrieved [`ActivatedFact`]s with pending implicit conflict metadata.
///
/// Queries `implicit_conflict_candidates` for all edge IDs in `facts` and sets
/// `is_implicit_conflict = true` and `conflict_candidate_id` on matches.
///
/// # Errors
///
/// Returns a [`MemoryError`] on database query failure.
#[tracing::instrument(skip_all, name = "memory.graph.implicit_conflict.annotate", fields(facts = facts.len()))]
pub async fn annotate_conflicts(
    facts: &mut [ActivatedFact],
    tx: &mut DbTransaction<'_>,
) -> Result<(), MemoryError> {
    if facts.is_empty() {
        return Ok(());
    }

    let edge_ids: Vec<i64> = facts.iter().map(|f| f.edge.id).collect();

    let mut edge_to_candidate: std::collections::HashMap<i64, i64> =
        std::collections::HashMap::new();

    for chunk in edge_ids.chunks(MAX_IDS_PER_QUERY) {
        let placeholders: String = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
        let query_str = format!(
            "SELECT id, edge_a_id, edge_b_id
             FROM implicit_conflict_candidates
             WHERE status = 'pending'
               AND (edge_a_id IN ({placeholders}) OR edge_b_id IN ({placeholders}))",
        );

        let mut q = sqlx::query(sqlx::AssertSqlSafe(query_str));
        for id in chunk {
            q = q.bind(id);
        }
        // Bind a second time for the second IN clause.
        for id in chunk {
            q = q.bind(id);
        }

        let rows = q.fetch_all(&mut **tx).await.map_err(MemoryError::from)?;

        for row in rows {
            use sqlx::Row as _;
            let candidate_id: i64 = row.try_get("id").map_err(MemoryError::from)?;
            let ea: i64 = row.try_get("edge_a_id").map_err(MemoryError::from)?;
            let eb: i64 = row.try_get("edge_b_id").map_err(MemoryError::from)?;
            edge_to_candidate.entry(ea).or_insert(candidate_id);
            edge_to_candidate.entry(eb).or_insert(candidate_id);
        }
    }

    for fact in facts.iter_mut() {
        if let Some(&cid) = edge_to_candidate.get(&fact.edge.id) {
            fact.is_implicit_conflict = true;
            fact.conflict_candidate_id = Some(cid);
        }
    }

    Ok(())
}

/// Hand-rolled Levenshtein edit distance (char-level).
fn levenshtein_distance(a: &str, b: &str) -> usize {
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    let m = a_chars.len();
    let n = b_chars.len();

    let mut prev: Vec<usize> = (0..=n).collect();
    let mut curr = vec![0usize; n + 1];

    for i in 1..=m {
        curr[0] = i;
        for j in 1..=n {
            let cost = usize::from(a_chars[i - 1] != b_chars[j - 1]);
            curr[j] = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }

    prev[n]
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeph_config::ImplicitConflictConfig;

    fn detector(enabled: bool) -> ImplicitConflictDetector {
        ImplicitConflictDetector::new(ImplicitConflictConfig {
            enabled,
            conflict_similarity_threshold: 0.80,
            ..Default::default()
        })
    }

    #[test]
    fn normalized_levenshtein_identical() {
        assert!(
            (ImplicitConflictDetector::normalized_levenshtein("uses", "uses") - 1.0).abs()
                < f64::EPSILON
        );
    }

    #[test]
    fn normalized_levenshtein_empty_both() {
        assert!(
            (ImplicitConflictDetector::normalized_levenshtein("", "") - 1.0).abs() < f64::EPSILON
        );
    }

    #[test]
    fn normalized_levenshtein_empty_one() {
        assert!(
            (ImplicitConflictDetector::normalized_levenshtein("", "abc") - 0.0).abs()
                < f64::EPSILON
        );
        assert!(
            (ImplicitConflictDetector::normalized_levenshtein("abc", "") - 0.0).abs()
                < f64::EPSILON
        );
    }

    #[test]
    fn normalized_levenshtein_completely_different() {
        let sim = ImplicitConflictDetector::normalized_levenshtein("uses", "xyz_unrelated_value");
        assert!(sim < 0.5, "expected low similarity, got {sim}");
    }

    #[test]
    fn detect_candidates_above_threshold_returns_candidate() {
        let d = detector(true);
        // "employ" vs "employs": distance = 1, max = 7, sim ≈ 0.857 — above 0.80
        let candidates = d.detect_candidates(42, "employ", &[(7, "employs")], false);
        assert_eq!(candidates.len(), 1, "expected one candidate");
        assert_eq!(candidates[0].edge_a_id, 42);
        assert_eq!(candidates[0].edge_b_id, 7);
        assert!(candidates[0].similarity >= 0.80);
    }

    #[test]
    fn detect_candidates_below_threshold_returns_empty() {
        let d = detector(true);
        let candidates = d.detect_candidates(1, "uses", &[(2, "xyz_unrelated")], false);
        assert!(
            candidates.is_empty(),
            "expected no candidates below threshold"
        );
    }

    #[test]
    fn detect_candidates_identical_predicate_skipped() {
        let d = detector(true);
        // Identical predicates are handled by APEX-MEM; detector must skip them.
        let candidates = d.detect_candidates(1, "uses", &[(2, "uses")], false);
        assert!(
            candidates.is_empty(),
            "identical predicates must not create candidates"
        );
    }

    #[test]
    fn detect_candidates_disabled_returns_empty() {
        let d = detector(false);
        // Even with high-similarity predicates, disabled detector returns nothing.
        let candidates = d.detect_candidates(1, "employ", &[(2, "employs")], false);
        assert!(candidates.is_empty(), "disabled detector must return empty");
    }

    #[test]
    fn detect_candidates_cardinality_n_skipped() {
        let d = detector(true);
        let candidates = d.detect_candidates(1, "employ", &[(2, "employs")], true);
        assert!(
            candidates.is_empty(),
            "cardinality-n predicate must be skipped"
        );
    }

    // ── annotate_conflicts DB tests ───────────────────────────────────────────

    async fn setup_test_db() -> crate::store::SqliteStore {
        crate::store::SqliteStore::new(":memory:").await.unwrap()
    }

    fn stub_fact(edge_id: i64) -> ActivatedFact {
        use crate::graph::types::{Edge, EdgeType};
        ActivatedFact {
            edge: Edge {
                id: edge_id,
                source_entity_id: 1,
                target_entity_id: 2,
                relation: "test".to_owned(),
                canonical_relation: "test".to_owned(),
                fact: "test fact".to_owned(),
                confidence: 1.0,
                valid_from: "2026-01-01".to_owned(),
                valid_to: None,
                created_at: "2026-01-01".to_owned(),
                expired_at: None,
                source_message_id: None,
                qdrant_point_id: None,
                edge_type: EdgeType::Semantic,
                retrieval_count: 0,
                last_retrieved_at: None,
                superseded_by: None,
                supersedes: None,
                weight: 1.0,
                confidence_fast: 1.0,
                confidence_slow: 1.0,
                turn_index: None,
            },
            activation_score: 1.0,
            is_implicit_conflict: false,
            conflict_candidate_id: None,
        }
    }

    #[tokio::test]
    async fn annotate_conflicts_marks_flagged_edges() {
        let db = setup_test_db().await;
        let pool = db.pool();

        // Insert a pending candidate pair (edge_a_id=1, edge_b_id=2).
        // Use raw SQL since these are not real graph_edges (no FK enforcement with PRAGMA).
        sqlx::query(
            "PRAGMA foreign_keys = OFF;
             INSERT INTO implicit_conflict_candidates
             (edge_a_id, edge_b_id, similarity, method, status, created_at, expires_at)
             VALUES (1, 2, 0.90, 'levenshtein', 'pending', 1000000, 9999999)",
        )
        .execute(pool)
        .await
        .unwrap();

        let mut facts = vec![stub_fact(1), stub_fact(3)];

        let mut tx = zeph_db::begin(pool).await.unwrap();
        annotate_conflicts(&mut facts, &mut tx).await.unwrap();
        tx.commit().await.unwrap();

        assert!(facts[0].is_implicit_conflict, "edge 1 must be flagged");
        assert!(facts[0].conflict_candidate_id.is_some());
        assert!(!facts[1].is_implicit_conflict, "edge 3 must not be flagged");
        assert!(facts[1].conflict_candidate_id.is_none());
    }

    #[tokio::test]
    async fn annotate_conflicts_empty_candidates_no_annotation() {
        let db = setup_test_db().await;
        let pool = db.pool();

        let mut facts = vec![stub_fact(10), stub_fact(20)];

        let mut tx = zeph_db::begin(pool).await.unwrap();
        annotate_conflicts(&mut facts, &mut tx).await.unwrap();
        tx.commit().await.unwrap();

        assert!(
            !facts[0].is_implicit_conflict,
            "no candidates → no annotation"
        );
        assert!(facts[1].conflict_candidate_id.is_none());
    }

    #[tokio::test]
    async fn annotate_conflicts_edge_b_side_also_flagged() {
        let db = setup_test_db().await;
        let pool = db.pool();

        // Insert candidate with edge_a=5, edge_b=7. Pass edge 7 in facts.
        sqlx::query(
            "PRAGMA foreign_keys = OFF;
             INSERT INTO implicit_conflict_candidates
             (edge_a_id, edge_b_id, similarity, method, status, created_at, expires_at)
             VALUES (5, 7, 0.85, 'levenshtein', 'pending', 1000000, 9999999)",
        )
        .execute(pool)
        .await
        .unwrap();

        let mut facts = vec![stub_fact(7)];

        let mut tx = zeph_db::begin(pool).await.unwrap();
        annotate_conflicts(&mut facts, &mut tx).await.unwrap();
        tx.commit().await.unwrap();

        assert!(
            facts[0].is_implicit_conflict,
            "edge on edge_b side must also be flagged"
        );
    }
}
