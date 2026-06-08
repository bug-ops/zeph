// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Knowledge-ingest subsystem for `zeph-memory` (spec-067).
//!
//! Provides:
//! - The idempotency ledger ([`IngestLedger`]) used by `zeph knowledge ingest` to skip
//!   unchanged inputs (Phase 1 / FR-012, INV-5).
//! - Document and provenance types ([`IngestDocument`], [`IngestSourceKind`]) used by
//!   the graph-sink batch API (Phase 2 / FR-020..028).
//! - Source adapters ([`IngestSourceAdapter`], [`SubagentJsonl`], [`ClaudeCodeJsonl`],
//!   [`CodexJsonl`]) that convert raw source material into validated [`IngestDocument`]
//!   values.
//! - Progress and report types ([`IngestProgress`], [`IngestReport`], [`ImportBatchId`])
//!   for the `SemanticMemory::ingest_documents` call site.
//! - The tech-doc extraction prompt ([`prompt::TECH_DOC_SYSTEM_PROMPT`]) selected by
//!   [`IngestSourceKind::system_prompt`].

pub mod adapter;
pub mod document;
pub mod ledger;
pub mod prompt;
pub mod report;

pub use adapter::{
    ClaudeCodeJsonl, CodexJsonl, IngestSourceAdapter, SubagentJsonl, TranscriptEntry,
};
pub use document::{IngestDocument, IngestSourceKind};
pub use ledger::{IngestLedger, LedgerEntry};
pub use report::{HubDegree, ImportBatchId, IngestFailure, IngestProgress, IngestReport};
