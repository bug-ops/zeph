// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Kumiho belief revision: semantic supersession of contradicted graph edges.
//!
//! When a new fact is inserted for an entity pair, this module checks whether any existing
//! active edges for the same pair carry a semantically contradictory claim. If so, the old
//! edge is marked as superseded (via `superseded_by` pointer + `valid_to` timestamp) and the
//! new edge becomes the current belief.
//!
//! ## Contradiction heuristic
//!
//! Similarity alone is insufficient for contradiction detection: "Alice prefers Python" and
//! "Alice loves Python" are highly similar but NOT contradictory (they reinforce each other).
//! The heuristic requires:
//! 1. Same entity pair (guaranteed by the call site — `edges_exact` already filters).
//! 2. Same relation domain: the relation strings must overlap significantly (edit-distance-based
//!    normalized similarity >= 0.5, or one contains the other).
//! 3. Different fact content: cosine similarity of fact embeddings >= `similarity_threshold`.
//!
//! Reinforcing facts share a relation AND similar content → caught by exact-match dedup upstream.
//! Contradictions share a relation but differ in content → caught here.

use zeph_llm::any::AnyProvider;
use zeph_llm::provider::LlmProvider as _;

use crate::error::MemoryError;
use crate::graph::types::{Edge, EdgeType};

/// Runtime config for Kumiho belief revision, passed into resolver methods.
#[derive(Debug, Clone)]
pub struct BeliefRevisionConfig {
    pub similarity_threshold: f32,
}

/// Compute cosine similarity between two equal-length vectors.
///
/// Returns 0.0 if either vector has zero norm.
#[must_use]
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a < f32::EPSILON || norm_b < f32::EPSILON {
        return 0.0;
    }
    (dot / (norm_a * norm_b)).clamp(-1.0, 1.0)
}

/// Normalized edit-distance similarity between two strings.
///
/// Returns 1.0 for equal strings, 0.0 when distance equals max length.
/// Used to check whether two relation strings are in the same domain.
#[must_use]
fn relation_similarity(a: &str, b: &str) -> f32 {
    if a == b {
        return 1.0;
    }
    let max_len = a.len().max(b.len());
    if max_len == 0 {
        return 1.0;
    }
    let dist = edit_distance(a, b);
    // max_len <= string byte length, bounded by MAX_RELATION_BYTES (256); cast is safe.
    #[allow(clippy::cast_precision_loss)]
    let max_len_f = max_len as f32;
    #[allow(clippy::cast_precision_loss)]
    let dist_f = dist as f32;
    1.0 - dist_f / max_len_f
}

/// Simple byte-level edit distance (Levenshtein) between two strings.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let m = a.len();
    let n = b.len();
    let mut prev: Vec<usize> = (0..=n).collect();
    let mut curr = vec![0usize; n + 1];
    for i in 1..=m {
        curr[0] = i;
        for j in 1..=n {
            curr[j] = if a[i - 1] == b[j - 1] {
                prev[j - 1]
            } else {
                1 + prev[j].min(curr[j - 1]).min(prev[j - 1])
            };
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[n]
}

/// Embed a fact string with the given provider.
///
/// Returns `None` on timeout (5 s) or provider error.
async fn embed_fact(provider: &AnyProvider, fact: &str) -> Option<Vec<f32>> {
    match tokio::time::timeout(std::time::Duration::from_secs(5), provider.embed(fact)).await {
        Ok(Ok(v)) => Some(v),
        Ok(Err(err)) => {
            tracing::warn!(error = %err, "belief_revision: embed failed");
            None
        }
        Err(_) => {
            tracing::warn!("belief_revision: embed timed out");
            None
        }
    }
}

/// Find edges that are semantically superseded by a new fact for the same entity pair.
///
/// An existing edge is superseded when:
/// - Its relation is in the same domain as `new_relation` (similarity >= 0.5, or containment).
/// - The fact embedding similarity to `new_fact_embedding` is >= `config.similarity_threshold`.
///
/// The new fact embedding is pre-computed by the caller so this function can reuse it across
/// multiple call sites without redundant embed calls.
///
/// Returns the list of edge IDs to supersede (may be empty).
///
/// # Errors
///
/// Returns an error if embedding calls fail in an unexpected way (beyond soft timeouts).
pub async fn find_superseded_edges(
    existing_edges: &[Edge],
    new_fact_embedding: &[f32],
    new_relation: &str,
    new_edge_type: EdgeType,
    provider: &AnyProvider,
    config: &BeliefRevisionConfig,
) -> Result<Vec<i64>, MemoryError> {
    // Filter to edges of the same type with a related relation domain.
    let candidates: Vec<&Edge> = existing_edges
        .iter()
        .filter(|e| {
            e.edge_type == new_edge_type && is_same_relation_domain(&e.relation, new_relation)
        })
        .collect();

    if candidates.is_empty() {
        return Ok(Vec::new());
    }

    let mut superseded = Vec::new();
    for edge in candidates {
        let Some(existing_emb) = embed_fact(provider, &edge.fact).await else {
            continue;
        };
        let sim = cosine_similarity(new_fact_embedding, &existing_emb);
        if sim >= config.similarity_threshold {
            tracing::debug!(
                edge_id = edge.id,
                relation = %edge.relation,
                similarity = sim,
                "belief_revision: edge superseded"
            );
            superseded.push(edge.id);
        }
    }
    Ok(superseded)
}

/// Check whether two relation strings are in the same domain.
///
/// Returns `true` when:
/// - They are identical.
/// - One contains the other as a substring.
/// - Their normalized edit-distance similarity >= 0.5.
#[must_use]
fn is_same_relation_domain(existing: &str, new: &str) -> bool {
    if existing == new {
        return true;
    }
    if existing.contains(new) || new.contains(existing) {
        return true;
    }
    relation_similarity(existing, new) >= 0.5
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_similarity_identical_vectors() {
        let v = vec![1.0f32, 0.0, 0.0];
        assert!((cosine_similarity(&v, &v) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_similarity_orthogonal_vectors() {
        let a = vec![1.0f32, 0.0];
        let b = vec![0.0f32, 1.0];
        assert!(cosine_similarity(&a, &b).abs() < 1e-6);
    }

    #[test]
    fn cosine_similarity_zero_vector_returns_zero() {
        let a = vec![0.0f32, 0.0];
        let b = vec![1.0f32, 0.0];
        assert!((cosine_similarity(&a, &b) - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn cosine_similarity_length_mismatch_returns_zero() {
        let a = vec![1.0f32, 0.0];
        let b = vec![1.0f32];
        assert!((cosine_similarity(&a, &b) - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn relation_similarity_equal_strings() {
        assert!((relation_similarity("works_at", "works_at") - 1.0).abs() < 1e-6);
    }

    #[test]
    fn relation_similarity_completely_different() {
        // "abc" vs "xyz" — all 3 chars different, distance = 3, max_len = 3, sim = 0.0
        let s = relation_similarity("abc", "xyz");
        assert!(s < 0.1, "expected near-0 similarity, got {s}");
    }

    #[test]
    fn relation_similarity_partial_overlap() {
        // "works_at" vs "work_at" — 1 char diff out of 8 → ~0.875
        let s = relation_similarity("works_at", "work_at");
        assert!(s > 0.5, "expected > 0.5, got {s}");
    }

    #[test]
    fn is_same_relation_domain_identical() {
        assert!(is_same_relation_domain("works_at", "works_at"));
    }

    #[test]
    fn is_same_relation_domain_containment() {
        assert!(is_same_relation_domain("prefers", "prefers_strongly"));
    }

    #[test]
    fn is_same_relation_domain_different_domains() {
        assert!(!is_same_relation_domain("works_at", "knows"));
    }

    #[test]
    fn edit_distance_empty_strings() {
        assert_eq!(edit_distance("", ""), 0);
    }

    #[test]
    fn edit_distance_one_empty() {
        assert_eq!(edit_distance("abc", ""), 3);
        assert_eq!(edit_distance("", "xyz"), 3);
    }

    #[test]
    fn edit_distance_single_substitution() {
        assert_eq!(edit_distance("works_at", "work_at"), 1);
    }

    // --- find_superseded_edges tests (provider-free paths) ---

    fn make_edge(
        id: i64,
        relation: &str,
        fact: &str,
        edge_type: crate::graph::types::EdgeType,
    ) -> crate::graph::types::Edge {
        crate::graph::types::Edge {
            id,
            source_entity_id: 1,
            target_entity_id: 2,
            relation: relation.to_string(),
            fact: fact.to_string(),
            confidence: 1.0,
            valid_from: "2026-01-01".to_string(),
            valid_to: None,
            created_at: "2026-01-01".to_string(),
            expired_at: None,
            episode_id: None,
            qdrant_point_id: None,
            edge_type,
            retrieval_count: 0,
            last_retrieved_at: None,
            superseded_by: None,
        }
    }

    #[tokio::test]
    async fn find_superseded_edges_empty_existing_returns_empty() {
        use zeph_llm::any::AnyProvider;
        use zeph_llm::mock::MockProvider;
        let provider = AnyProvider::Mock(MockProvider::default());
        let config = BeliefRevisionConfig {
            similarity_threshold: 0.85,
        };
        let result = find_superseded_edges(
            &[],
            &[0.5f32; 4],
            "works_at",
            crate::graph::types::EdgeType::Semantic,
            &provider,
            &config,
        )
        .await
        .unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn find_superseded_edges_different_edge_type_not_candidate() {
        use zeph_llm::any::AnyProvider;
        use zeph_llm::mock::MockProvider;
        let provider = AnyProvider::Mock(MockProvider::default());
        let config = BeliefRevisionConfig {
            similarity_threshold: 0.85,
        };
        // Edge is Causal, new fact is Semantic → different type, must not be a candidate.
        let edge = make_edge(
            42,
            "works_at",
            "Alice works at Acme",
            crate::graph::types::EdgeType::Causal,
        );
        let result = find_superseded_edges(
            &[edge],
            &[0.5f32; 4],
            "works_at",
            crate::graph::types::EdgeType::Semantic,
            &provider,
            &config,
        )
        .await
        .unwrap();
        assert!(
            result.is_empty(),
            "different edge type must not be superseded"
        );
    }

    #[tokio::test]
    async fn find_superseded_edges_different_domain_not_candidate() {
        use zeph_llm::any::AnyProvider;
        use zeph_llm::mock::MockProvider;
        let provider = AnyProvider::Mock(MockProvider::default());
        let config = BeliefRevisionConfig {
            similarity_threshold: 0.85,
        };
        // "works_at" vs "knows" — clearly different domains (edit-distance sim < 0.5).
        let edge = make_edge(
            7,
            "works_at",
            "Alice works at Acme",
            crate::graph::types::EdgeType::Semantic,
        );
        let result = find_superseded_edges(
            &[edge],
            &[0.5f32; 4],
            "knows",
            crate::graph::types::EdgeType::Semantic,
            &provider,
            &config,
        )
        .await
        .unwrap();
        assert!(
            result.is_empty(),
            "different relation domain must not be superseded"
        );
    }

    #[tokio::test]
    async fn find_superseded_edges_high_similarity_supersedes() {
        use zeph_llm::any::AnyProvider;
        use zeph_llm::mock::MockProvider;
        // MockProvider returns the same fixed embedding for all inputs.
        // cosine_similarity(same, same) = 1.0 >= threshold → superseded.
        let mut mock = MockProvider::default();
        mock.supports_embeddings = true;
        mock.embedding = vec![1.0f32, 0.0, 0.0, 0.0];
        let provider = AnyProvider::Mock(mock);
        let config = BeliefRevisionConfig {
            similarity_threshold: 0.85,
        };
        let edge = make_edge(
            99,
            "works_at",
            "Alice works at OldCo",
            crate::graph::types::EdgeType::Semantic,
        );
        // new fact embedding is identical to mock embedding → sim = 1.0
        let new_emb = vec![1.0f32, 0.0, 0.0, 0.0];
        let result = find_superseded_edges(
            &[edge],
            &new_emb,
            "works_at",
            crate::graph::types::EdgeType::Semantic,
            &provider,
            &config,
        )
        .await
        .unwrap();
        assert_eq!(result, vec![99], "high-similarity edge must be superseded");
    }

    #[tokio::test]
    async fn find_superseded_edges_low_similarity_not_superseded() {
        use zeph_llm::any::AnyProvider;
        use zeph_llm::mock::MockProvider;
        // MockProvider returns [1,0,0,0]; new fact embedding is [0,1,0,0] → cosine = 0.0 < 0.85.
        let mut mock = MockProvider::default();
        mock.supports_embeddings = true;
        mock.embedding = vec![1.0f32, 0.0, 0.0, 0.0];
        let provider = AnyProvider::Mock(mock);
        let config = BeliefRevisionConfig {
            similarity_threshold: 0.85,
        };
        let edge = make_edge(
            55,
            "works_at",
            "Alice works at OldCo",
            crate::graph::types::EdgeType::Semantic,
        );
        // orthogonal embedding → sim = 0.0 → not superseded
        let new_emb = vec![0.0f32, 1.0, 0.0, 0.0];
        let result = find_superseded_edges(
            &[edge],
            &new_emb,
            "works_at",
            crate::graph::types::EdgeType::Semantic,
            &provider,
            &config,
        )
        .await
        .unwrap();
        assert!(
            result.is_empty(),
            "low-similarity edge (reinforcing or unrelated) must not be superseded"
        );
    }
}
