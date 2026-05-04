// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! View-aware recall types for `MemCoT` graph retrieval (issues #3574 / #3575).
//!
//! [`RecallView`] selects the enrichment level applied to the raw graph recall results.
//! [`RecalledFact`] is the unified fact type returned by [`crate::semantic::SemanticMemory::recall_graph_view`].

use crate::graph::types::GraphFact;
use crate::types::MessageId;

/// Enrichment level for view-aware graph recall.
///
/// Controls whether and how graph facts are enriched after the base retrieval step
/// (BFS or spreading activation). For `Head`, the function is byte-identical to
/// the legacy `recall_graph` / `recall_graph_activated` paths.
///
/// TODO(F3): add a per-call override so callers can request a different view than
/// the one configured in `MemCotConfig::recall_view`.
///
/// # Examples
///
/// ```
/// use zeph_memory::RecallView;
///
/// assert_eq!(RecallView::default(), RecallView::Head);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RecallView {
    /// Standard retrieval — no enrichment beyond what the base method provides.
    ///
    /// When `sa_params = None` AND `view = Head`, the output is byte-identical to
    /// calling `recall_graph` directly.
    #[default]
    Head,
    /// Retrieval + source-message provenance.
    ///
    /// Each returned fact is enriched with the `MessageId` and a ≤200-char snippet
    /// from the message that originally created the edge.
    ZoomIn,
    /// Retrieval + 1-hop neighbor expansion.
    ///
    /// For each returned fact, up to `neighbor_cap` additional 1-hop neighbor facts
    /// are appended. Neighbors are deduped against the head set using the canonical
    /// `(source_name, relation, target_name, edge_type)` tuple.
    ZoomOut,
}

/// A graph fact returned by the view-aware recall path.
///
/// Wraps a base [`GraphFact`] and carries optional enrichment fields populated
/// depending on the [`RecallView`] in use.
///
/// # Examples
///
/// ```
/// use zeph_memory::{RecalledFact, RecallView};
/// use zeph_memory::graph::types::GraphFact;
/// use zeph_memory::graph::EdgeType;
///
/// let base = GraphFact {
///     entity_name: "Rust".to_string(),
///     relation: "uses".to_string(),
///     target_name: "LLVM".to_string(),
///     fact: "Rust uses LLVM for code generation".to_string(),
///     entity_match_score: 0.9,
///     hop_distance: 0,
///     confidence: 0.95,
///     valid_from: None,
///     edge_type: EdgeType::Semantic,
///     retrieval_count: 1,
///     edge_id: None,
/// };
/// let recalled = RecalledFact::from_graph_fact(base);
/// assert!(recalled.activation_score.is_none());
/// assert!(recalled.provenance_message_id.is_none());
/// assert!(recalled.neighbors.is_empty());
/// ```
#[derive(Debug, Clone)]
pub struct RecalledFact {
    /// The base graph fact (text, relation, confidence, `edge_type`, `hop_distance`).
    pub fact: GraphFact,
    /// Spreading-activation score. `Some` when the SA path was used; `None` for plain BFS.
    ///
    /// Used by the assembler to render `(activation: X)` suffix and preserve
    /// current output bytes when `view = Head`.
    pub activation_score: Option<f32>,
    /// Source-message ID for the edge (Zoom-In only).
    pub provenance_message_id: Option<MessageId>,
    /// ≤200-char snippet from the source message, with `\n\r<>` scrubbed (Zoom-In only).
    pub provenance_snippet: Option<String>,
    /// 1-hop neighbor facts deduped against the head set (Zoom-Out only).
    pub neighbors: Vec<GraphFact>,
}

impl RecalledFact {
    /// Wrap a plain `GraphFact` with no enrichment fields set.
    #[must_use]
    pub fn from_graph_fact(fact: GraphFact) -> Self {
        Self {
            fact,
            activation_score: None,
            provenance_message_id: None,
            provenance_snippet: None,
            neighbors: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::types::{EdgeType, GraphFact};
    use crate::types::MessageId;

    fn make_fact() -> GraphFact {
        GraphFact {
            entity_name: "Rust".to_string(),
            relation: "uses".to_string(),
            target_name: "LLVM".to_string(),
            fact: "Rust uses LLVM".to_string(),
            entity_match_score: 0.9,
            hop_distance: 0,
            confidence: 0.95,
            valid_from: None,
            edge_type: EdgeType::Semantic,
            retrieval_count: 0,
            edge_id: None,
        }
    }

    #[test]
    fn from_graph_fact_no_enrichment() {
        let rf = RecalledFact::from_graph_fact(make_fact());
        assert!(rf.activation_score.is_none());
        assert!(rf.provenance_message_id.is_none());
        assert!(rf.provenance_snippet.is_none());
        assert!(rf.neighbors.is_empty());
    }

    #[test]
    fn recall_view_default_is_head() {
        assert_eq!(RecallView::default(), RecallView::Head);
    }

    // ── Snapshot tests: Head / ZoomIn / ZoomOut output shape ─────────────────

    fn head_fact() -> RecalledFact {
        RecalledFact::from_graph_fact(GraphFact {
            entity_name: "Rust".to_string(),
            relation: "uses".to_string(),
            target_name: "LLVM".to_string(),
            fact: "Rust uses LLVM for code generation".to_string(),
            entity_match_score: 0.9,
            hop_distance: 0,
            confidence: 0.95,
            valid_from: Some("2026-01-01".to_string()),
            edge_type: EdgeType::Semantic,
            retrieval_count: 1,
            edge_id: Some(10),
        })
    }

    #[test]
    fn snapshot_head_no_enrichment() {
        let rf = head_fact();
        insta::assert_debug_snapshot!("head_view", rf);
    }

    #[test]
    fn snapshot_zoom_in_with_provenance() {
        let mut rf = head_fact();
        rf.provenance_message_id = Some(MessageId(42));
        rf.provenance_snippet = Some("The Rust compiler uses LLVM as its backend".to_string());
        insta::assert_debug_snapshot!("zoom_in_view", rf);
    }

    #[test]
    fn snapshot_zoom_out_with_neighbors() {
        let mut rf = head_fact();
        rf.neighbors.push(GraphFact {
            entity_name: "LLVM".to_string(),
            relation: "supports".to_string(),
            target_name: "WebAssembly".to_string(),
            fact: "LLVM supports WebAssembly output".to_string(),
            entity_match_score: 0.5,
            hop_distance: 1,
            confidence: 0.8,
            valid_from: None,
            edge_type: EdgeType::Semantic,
            retrieval_count: 0,
            edge_id: Some(11),
        });
        insta::assert_debug_snapshot!("zoom_out_view", rf);
    }

    #[test]
    fn snapshot_sa_fact_with_activation_score() {
        // Simulate an SA-path fact: activation_score is Some, entity names are empty.
        let rf = RecalledFact {
            fact: GraphFact {
                entity_name: String::new(),
                relation: "uses".to_string(),
                target_name: String::new(),
                fact: "Rust uses LLVM for compilation".to_string(),
                entity_match_score: 0.82,
                hop_distance: 0,
                confidence: 0.9,
                valid_from: Some("2026-01-01".to_string()),
                edge_type: EdgeType::Semantic,
                retrieval_count: 0,
                edge_id: Some(55),
            },
            activation_score: Some(0.82),
            provenance_message_id: None,
            provenance_snippet: None,
            neighbors: Vec::new(),
        };
        insta::assert_debug_snapshot!("sa_head_view", rf);
    }
}
