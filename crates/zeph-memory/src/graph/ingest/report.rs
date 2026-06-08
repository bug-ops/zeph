// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Result and progress types for `SemanticMemory::ingest_documents` (spec-067 §2.3).

use std::fmt;

/// Opaque identifier for a single ingest run.
///
/// Used as a rollback key: all entities and edges written during one `ingest_documents`
/// call share the same `ImportBatchId`, so the entire batch can be removed atomically
/// via `zeph knowledge rollback --batch-id <id>`.
///
/// The underlying format is a `UUIDv4` string, consistent with the `import_batch_id` column
/// documented in [`super::ledger::IngestLedger`] and with the rollback CLI (which treats
/// the column as an opaque `TEXT` key). ULID literals appear in ledger doctests as
/// illustrative placeholders only — the authoritative format in `knowledge.rs` is
/// `uuid::Uuid::new_v4().to_string()`.
///
/// # Examples
///
/// ```no_run
/// use zeph_memory::graph::ingest::ImportBatchId;
///
/// let id = ImportBatchId::new();
/// println!("batch: {id}");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ImportBatchId(String);

impl ImportBatchId {
    /// Generates a new random `ImportBatchId` (`UUIDv4`).
    #[must_use]
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    /// Returns the underlying string value.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for ImportBatchId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for ImportBatchId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Incremental progress event emitted by [`crate::semantic::SemanticMemory::ingest_documents`].
///
/// Sent on the `mpsc::Sender<IngestProgress>` channel passed to `ingest_documents`.
/// The channel is advisory — a full or dropped receiver never fails the batch.
///
/// # Wire contract
///
/// Events arrive in this order per batch:
/// `Started` → (`DocumentSkipped` | `DocumentDone` | `DocumentFailed` | `DocumentRejected`)* → `Finished`
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum IngestProgress {
    /// Emitted once before any document is processed.
    Started {
        /// Total number of documents in the batch (after intra-batch dedup).
        total: usize,
    },
    /// A document was skipped because its `(source_uri, content_hash)` pair is already
    /// in the ingest ledger, or it was a duplicate within the same batch.
    DocumentSkipped {
        /// Source locator of the skipped document.
        uri: String,
    },
    /// A document was successfully extracted and stored.
    DocumentDone {
        /// Source locator of the processed document.
        uri: String,
        /// Number of graph entities upserted.
        entities: usize,
        /// Number of graph edges inserted.
        edges: usize,
    },
    /// A document failed to extract or store; the batch continues.
    DocumentFailed {
        /// Source locator of the failed document.
        uri: String,
        /// Human-readable failure reason.
        reason: String,
    },
    /// A document was rejected by the post-extract validator (sanitizer).
    ///
    /// Rejected documents write nothing to the graph or ledger. They are counted
    /// separately from failures in [`IngestReport`] so operators can distinguish
    /// sanitizer drops from extraction errors.
    DocumentRejected {
        /// Source locator of the rejected document.
        uri: String,
        /// Rejection reason from the validator.
        reason: String,
    },
    /// Emitted once after all documents have been processed.
    Finished,
}

/// Per-document failure record included in [`IngestReport`].
#[derive(Debug, Clone)]
pub struct IngestFailure {
    /// Source locator of the failed document.
    pub uri: String,
    /// Human-readable failure reason.
    pub reason: String,
}

/// Entity degree entry for dry-run hub-degree projection (FR-026).
#[derive(Debug, Clone)]
pub struct HubDegree {
    /// Entity name.
    pub entity: String,
    /// Projected edge degree (number of edges connecting to this entity in the dry run).
    pub degree: usize,
}

/// Aggregate result returned by [`crate::semantic::SemanticMemory::ingest_documents`].
///
/// In dry-run mode (`dry_run = true`), `entities_total` and `edges_total` are projected
/// post-sanitization counts and `hub_degree` contains the top-N entities by degree.
/// No writes occur.
///
/// In live mode, `entities_total` and `edges_total` reflect actual upserts/inserts.
///
/// # Dry-run accuracy
///
/// Dry-run counts are post-sanitization but pre-quality-gate upper bounds. The live write
/// path applies self-loop rejection and admission control in `insert_edges` /
/// `upsert_entities` that may reduce the final entity and edge counts relative to the
/// dry-run projection. Rejected documents do NOT contribute to `entities_total` or
/// `edges_total` in either mode.
#[derive(Debug, Clone, Default)]
pub struct IngestReport {
    /// Batch identifier shared by all documents processed in this call.
    pub batch_id: String,
    /// Total number of documents in the input (after intra-batch dedup).
    pub documents_total: usize,
    /// Number of documents skipped (already ingested or intra-batch duplicate).
    pub skipped: usize,
    /// Number of documents successfully processed.
    pub succeeded: usize,
    /// Number of documents rejected by the post-extract validator (sanitizer).
    ///
    /// Rejected documents write nothing to the graph or ledger and contribute zero
    /// to `entities_total` / `edges_total`. They are distinct from `failed` documents,
    /// which represent extraction or storage errors.
    pub rejected: usize,
    /// Per-document failure records.
    pub failed: Vec<IngestFailure>,
    /// Total entities upserted (live) or projected (dry-run).
    ///
    /// Excludes rejected documents. In dry-run mode this is a post-sanitization
    /// upper bound; the live run may produce fewer entities after admission control.
    pub entities_total: usize,
    /// Total edges inserted (live) or projected (dry-run).
    ///
    /// Excludes rejected documents. In dry-run mode this is a post-sanitization
    /// upper bound; the live run may produce fewer edges after self-loop rejection
    /// and admission control.
    pub edges_total: usize,
    /// `true` when `ingest_documents` was called with `dry_run = true`.
    pub dry_run: bool,
    /// Top-N entities by projected edge degree. Populated only in dry-run mode (FR-026).
    ///
    /// Degrees are computed from post-sanitization extractor output (rejected documents
    /// do not contribute). They represent pre-quality-gate upper bounds — not the final
    /// graph topology.
    pub hub_degree: Vec<HubDegree>,
}

#[cfg(test)]
mod tests {
    use super::{ImportBatchId, IngestFailure, IngestReport};

    #[test]
    fn import_batch_id_is_unique() {
        let a = ImportBatchId::new();
        let b = ImportBatchId::new();
        assert_ne!(a, b);
        // UUIDv4: 36 chars with 4 hyphens
        assert_eq!(a.as_str().len(), 36);
        assert_eq!(a.as_str().chars().filter(|&c| c == '-').count(), 4);
    }

    #[test]
    fn ingest_report_rejected_arithmetic_invariant() {
        let mut report = IngestReport {
            documents_total: 4,
            ..IngestReport::default()
        };
        report.skipped += 1;
        report.succeeded += 1;
        report.rejected += 1;
        report.failed.push(IngestFailure {
            uri: "u".into(),
            reason: "r".into(),
        });
        assert_eq!(
            report.skipped + report.succeeded + report.rejected + report.failed.len(),
            report.documents_total
        );
    }
}
