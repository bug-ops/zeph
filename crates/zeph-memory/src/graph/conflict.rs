// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! APEX-MEM conflict resolution for cardinality-1 predicates.
//!
//! When multiple head edges share `(subject, canonical_relation)` and the predicate has
//! `cardinality = 1`, this module selects one authoritative edge per the configured
//! [`ConflictStrategy`]:
//!
//! - `Recency`: picks the edge with the greatest `valid_from`.
//! - `Confidence`: picks the edge with the highest `confidence`.
//! - `Llm`: delegates to an LLM provider with a 500 ms hard timeout, falling back to `Recency`.
//!
//! # Invariants
//!
//! - The resolver is only invoked for cardinality-1 predicates; cardinality-n predicates
//!   pass all head edges through unchanged.
//! - The LLM strategy respects a 500 ms mandatory timeout and a per-turn budget cap; both
//!   exhaustion paths fall back to `Recency`.
//! - Losing edges are optionally retained in `alternatives` (disabled by default).
//!
//! # Unique index vs conflict resolver
//!
//! The partial unique index `uq_graph_edges_active_head` prevents same-target duplicates
//! for a cardinality-1 predicate (i.e., two rows for the exact same target entity cannot
//! both be active). The conflict resolver handles the orthogonal case: two head edges with
//! *different* targets for the same cardinality-1 predicate (e.g., `works_at Acme` vs
//! `works_at Globex`).

use std::time::Duration;

use zeph_llm::any::AnyProvider;
use zeph_llm::provider::{LlmProvider as _, Message, Role};

use crate::error::MemoryError;
use crate::graph::types::Edge;

/// Conflict resolution strategy for cardinality-1 predicates.
///
/// Mirrors `zeph_config::ConflictStrategy` but lives in `zeph-memory` to avoid
/// a circular crate dependency (`zeph-memory` → `zeph-config` → `zeph-mcp` → `zeph-memory`).
/// `zeph-config` re-exports its own copy; callers convert between the two.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConflictStrategy {
    /// Pick the edge with the most recent `valid_from` timestamp.
    Recency,
    /// Pick the edge with the highest `confidence` value.
    Confidence,
    /// Delegate to the configured LLM provider (500 ms timeout, falls back to `Recency`).
    Llm,
}

/// Maximum allowed depth when walking a `supersedes` chain for cycle detection.
/// Defined here as a named constant per critic nit #7.
pub const SUPERSEDE_DEPTH_CAP: usize = 64;

/// Output of conflict resolution for a single `(subject, canonical_relation)` group.
pub struct ConflictResult {
    /// The authoritative edge chosen by the resolver.
    pub winner: Edge,
    /// Edges that were not selected. Populated only when `retain_alternatives = true`.
    pub alternatives: Vec<Edge>,
}

/// Conflict resolver for cardinality-1 predicate groups.
pub struct ConflictResolver {
    strategy: ConflictStrategy,
    timeout: Duration,
    /// Remaining LLM calls allowed this turn (decremented on each LLM invocation).
    llm_budget: std::sync::atomic::AtomicI32,
    retain_alternatives: bool,
    /// LLM provider used when `strategy = Llm`. `None` falls back to `Recency`.
    llm_provider: Option<AnyProvider>,
}

impl ConflictResolver {
    /// Create a new resolver.
    ///
    /// - `strategy`: resolution strategy
    /// - `timeout_ms`: LLM resolver hard timeout in milliseconds (mandatory 500 ms per spec)
    /// - `llm_budget_per_turn`: max LLM calls per agent turn before falling back to recency
    /// - `retain_alternatives`: when `true`, losing edges are returned in `ConflictResult::alternatives`
    #[must_use]
    pub fn new(
        strategy: ConflictStrategy,
        timeout_ms: u64,
        llm_budget_per_turn: usize,
        retain_alternatives: bool,
    ) -> Self {
        let budget = i32::try_from(llm_budget_per_turn).unwrap_or(i32::MAX);
        Self {
            strategy,
            timeout: Duration::from_millis(timeout_ms),
            llm_budget: std::sync::atomic::AtomicI32::new(budget),
            retain_alternatives,
            llm_provider: None,
        }
    }

    /// Attach an LLM provider for `strategy = Llm` conflict resolution.
    #[must_use]
    pub fn with_llm_provider(mut self, provider: AnyProvider) -> Self {
        self.llm_provider = Some(provider);
        self
    }

    /// Reset the per-turn LLM budget. Call at the start of each agent turn.
    pub fn reset_turn_budget(&self, budget: usize) {
        let budget_i32 = i32::try_from(budget).unwrap_or(i32::MAX);
        self.llm_budget
            .store(budget_i32, std::sync::atomic::Ordering::Relaxed);
    }

    /// Resolve a group of head edges that share the same cardinality-1 predicate.
    ///
    /// `candidates` must be non-empty and all share `(source_entity_id, canonical_relation)`.
    ///
    /// # Errors
    ///
    /// Returns an error only on internal logic failures (empty candidate list).
    pub async fn resolve(
        &self,
        mut candidates: Vec<Edge>,
        metrics: &ApexMetrics,
    ) -> Result<ConflictResult, MemoryError> {
        tracing::debug!(target: "memory.graph.apex.conflict_resolve", candidates = candidates.len());

        if candidates.is_empty() {
            return Err(MemoryError::InvalidInput(
                "conflict resolver called with empty candidate list".into(),
            ));
        }
        if candidates.len() == 1 {
            return Ok(ConflictResult {
                winner: candidates.remove(0),
                alternatives: Vec::new(),
            });
        }

        let effective_strategy = self.effective_strategy();
        let winner_idx = match effective_strategy {
            ConflictStrategy::Recency => recency_winner(&candidates),
            ConflictStrategy::Confidence => confidence_winner(&candidates),
            ConflictStrategy::Llm => self.llm_winner(&candidates, metrics).await,
        };

        metrics
            .conflicts_total
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        let winner = candidates.remove(winner_idx);
        let alternatives = if self.retain_alternatives {
            candidates
        } else {
            Vec::new()
        };
        Ok(ConflictResult {
            winner,
            alternatives,
        })
    }

    /// Return the active strategy, falling back to `Recency` if LLM budget is exhausted.
    fn effective_strategy(&self) -> ConflictStrategy {
        if self.strategy == ConflictStrategy::Llm {
            let remaining = self.llm_budget.load(std::sync::atomic::Ordering::Relaxed);
            if remaining <= 0 {
                return ConflictStrategy::Recency;
            }
        }
        self.strategy.clone()
    }

    async fn llm_winner(&self, candidates: &[Edge], metrics: &ApexMetrics) -> usize {
        tracing::debug!(target: "memory.graph.apex.conflict_llm", candidates = candidates.len());
        self.llm_budget
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);

        let Some(provider) = &self.llm_provider else {
            // No provider configured — fall back to recency without consuming timeout.
            return recency_winner(candidates);
        };

        let prompt = build_conflict_prompt(candidates);
        let messages = [
            Message::from_legacy(
                Role::System,
                "You are a knowledge graph conflict resolver. Given a list of conflicting \
                 edge facts indexed from 0, respond with only the index of the most \
                 authoritative or recent fact. Output a single integer and nothing else.",
            ),
            Message::from_legacy(Role::User, prompt),
        ];

        let timeout = self.timeout;
        match tokio::time::timeout(timeout, provider.chat(&messages)).await {
            Ok(Ok(response)) => {
                let trimmed = response.trim();
                if let Ok(idx) = trimmed.parse::<usize>()
                    && idx < candidates.len()
                {
                    return idx;
                }
                tracing::warn!(
                    raw = %trimmed,
                    "apex_mem: LLM conflict resolver returned unparseable index, falling back to recency"
                );
                recency_winner(candidates)
            }
            Ok(Err(e)) => {
                tracing::warn!(error = %e,
                    "apex_mem: LLM conflict resolver call failed, falling back to recency");
                recency_winner(candidates)
            }
            Err(_) => {
                metrics
                    .llm_timeouts_total
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                tracing::warn!(
                    "apex_mem: LLM conflict resolver timed out after {}ms, falling back to recency",
                    timeout.as_millis()
                );
                recency_winner(candidates)
            }
        }
    }
}

fn build_conflict_prompt(candidates: &[Edge]) -> String {
    let mut lines = String::from("Conflicting facts for the same predicate:\n");
    for (i, edge) in candidates.iter().enumerate() {
        use std::fmt::Write as _;
        let _ = writeln!(lines, "{i}: [{}] {}", edge.valid_from, edge.fact);
    }
    lines.push_str(
        "\nWhich index (0-based) is the most authoritative? Respond with only the integer.",
    );
    lines
}

fn recency_winner(candidates: &[Edge]) -> usize {
    candidates
        .iter()
        .enumerate()
        .max_by(|(_, a), (_, b)| a.valid_from.cmp(&b.valid_from))
        .map_or(0, |(i, _)| i)
}

fn confidence_winner(candidates: &[Edge]) -> usize {
    candidates
        .iter()
        .enumerate()
        .max_by(|(_, a), (_, b)| {
            a.confidence
                .partial_cmp(&b.confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map_or(0, |(i, _)| i)
}

// ── Metrics counters ─────────────────────────────────────────────────────────

/// Atomic counters for APEX-MEM Prometheus metrics.
///
/// Shared across the store and conflict resolver via `Arc`.
#[derive(Debug, Default)]
pub struct ApexMetrics {
    /// Number of append-only supersede operations performed.
    pub supersedes_total: std::sync::atomic::AtomicU64,
    /// Number of conflict resolution operations performed.
    pub conflicts_total: std::sync::atomic::AtomicU64,
    /// Number of LLM conflict resolver timeout fallbacks.
    pub llm_timeouts_total: std::sync::atomic::AtomicU64,
    /// Number of predicates with no ontology entry (unmapped).
    pub unmapped_predicates_total: std::sync::atomic::AtomicU64,
}

impl ApexMetrics {
    /// Collect current counter snapshots as `(name, value)` pairs.
    #[must_use]
    pub fn snapshot(&self) -> Vec<(&'static str, u64)> {
        vec![
            (
                "apex_mem_supersedes_total",
                self.supersedes_total
                    .load(std::sync::atomic::Ordering::Relaxed),
            ),
            (
                "apex_mem_conflicts_total",
                self.conflicts_total
                    .load(std::sync::atomic::Ordering::Relaxed),
            ),
            (
                "apex_mem_llm_timeouts_total",
                self.llm_timeouts_total
                    .load(std::sync::atomic::Ordering::Relaxed),
            ),
            (
                "apex_mem_unmapped_predicates_total",
                self.unmapped_predicates_total
                    .load(std::sync::atomic::Ordering::Relaxed),
            ),
        ]
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_edge(id: i64, valid_from: &str, confidence: f32) -> Edge {
        Edge {
            id,
            source_entity_id: 1,
            target_entity_id: 2,
            relation: "works_at".into(),
            canonical_relation: "works_at".into(),
            fact: "fact".into(),
            confidence,
            valid_from: valid_from.to_string(),
            valid_to: None,
            created_at: valid_from.to_string(),
            expired_at: None,
            source_message_id: None,
            qdrant_point_id: None,
            edge_type: crate::graph::types::EdgeType::Semantic,
            retrieval_count: 0,
            last_retrieved_at: None,
            superseded_by: None,
            supersedes: None,
        }
    }

    #[tokio::test]
    async fn recency_strategy_picks_newest() {
        let metrics = ApexMetrics::default();
        let resolver = ConflictResolver::new(ConflictStrategy::Recency, 500, 3, false);
        let candidates = vec![
            make_edge(1, "2026-01-01 00:00:00", 0.9),
            make_edge(2, "2026-06-01 00:00:00", 0.5),
            make_edge(3, "2026-03-01 00:00:00", 0.7),
        ];
        let result = resolver.resolve(candidates, &metrics).await.unwrap();
        assert_eq!(result.winner.id, 2, "newest valid_from wins");
    }

    #[tokio::test]
    async fn confidence_strategy_picks_highest() {
        let metrics = ApexMetrics::default();
        let resolver = ConflictResolver::new(ConflictStrategy::Confidence, 500, 3, false);
        let candidates = vec![
            make_edge(1, "2026-01-01 00:00:00", 0.9),
            make_edge(2, "2026-06-01 00:00:00", 0.5),
            make_edge(3, "2026-03-01 00:00:00", 0.7),
        ];
        let result = resolver.resolve(candidates, &metrics).await.unwrap();
        assert_eq!(result.winner.id, 1);
    }

    #[tokio::test]
    async fn single_candidate_passes_through() {
        let metrics = ApexMetrics::default();
        let resolver = ConflictResolver::new(ConflictStrategy::Recency, 500, 3, false);
        let candidates = vec![make_edge(42, "2026-01-01 00:00:00", 0.8)];
        let result = resolver.resolve(candidates, &metrics).await.unwrap();
        assert_eq!(result.winner.id, 42);
        assert!(result.alternatives.is_empty());
    }

    #[tokio::test]
    async fn retain_alternatives_when_enabled() {
        let metrics = ApexMetrics::default();
        let resolver = ConflictResolver::new(ConflictStrategy::Recency, 500, 3, true);
        let candidates = vec![
            make_edge(1, "2026-01-01 00:00:00", 0.9),
            make_edge(2, "2026-06-01 00:00:00", 0.5),
        ];
        let result = resolver.resolve(candidates, &metrics).await.unwrap();
        assert_eq!(result.winner.id, 2);
        assert_eq!(result.alternatives.len(), 1);
        assert_eq!(result.alternatives[0].id, 1);
    }

    #[tokio::test]
    async fn budget_exhaustion_falls_back_to_recency() {
        let metrics = ApexMetrics::default();
        let resolver = ConflictResolver::new(ConflictStrategy::Llm, 500, 0, false);
        // Budget = 0 → effective strategy is Recency immediately.
        let candidates = vec![
            make_edge(1, "2026-01-01 00:00:00", 0.9),
            make_edge(2, "2026-06-01 00:00:00", 0.5),
        ];
        let result = resolver.resolve(candidates, &metrics).await.unwrap();
        assert_eq!(result.winner.id, 2);
    }

    #[test]
    fn metrics_snapshot_has_four_entries() {
        let m = ApexMetrics::default();
        assert_eq!(m.snapshot().len(), 4);
    }
}
