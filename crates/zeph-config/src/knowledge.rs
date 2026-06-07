// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Knowledge-ingest configuration (`[knowledge]`).
//!
//! This module defines [`KnowledgeConfig`], the optional TOML section that controls
//! the `zeph knowledge ingest` command (spec-067, Phase 1). All fields carry sane
//! defaults so an existing config that omits `[knowledge]` entirely still works.
//!
//! # Configuration example
//!
//! ```toml
//! [knowledge]
//! ingest_provider = "fast"   # from [[llm.providers]]; Phase 2 graph extraction
//! concurrency = 3
//! max_documents = 0          # 0 = unlimited
//! recall_include_imported = true
//! transcript_scope = "current-project"
//! ```

use serde::{Deserialize, Serialize};

/// Configuration for the `zeph knowledge` subsystem (spec-067 Phase 1).
///
/// Deserialised from the optional `[knowledge]` table in `config.toml`.
/// Missing fields fall back to [`Default`] values — no migration required for
/// existing configs that omit the section entirely.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct KnowledgeConfig {
    /// Provider name from `[[llm.providers]]` used for graph extraction (Phase 2).
    ///
    /// Empty string → fall back to `[memory.graph].extract_provider` → primary provider.
    /// The notes-sink (Phase 1) does not perform LLM calls; this field is reserved for
    /// Phase 2 graph extraction and ignored until then.
    pub ingest_provider: String,

    /// Maximum number of concurrent document-processing tasks during batch extraction (Phase 2).
    ///
    /// Has no effect in Phase 1 (notes sink processes files sequentially via
    /// `IngestionPipeline`). Stored now for config stability.
    pub concurrency: usize,

    /// Maximum number of documents processed per `ingest` run; `0` = unlimited.
    ///
    /// The CLI `--max-documents` flag overrides this value when non-zero.
    /// Bounds cost per run (spec-067 NFR-002).
    pub max_documents: usize,

    /// Whether semantic-recall results include rows imported via `zeph knowledge ingest`.
    ///
    /// When `false`, only conversation-derived memory rows are surfaced.
    /// Phase 2 recall integration honours this flag; Phase 1 notes-sink writes use it
    /// only to document intent.
    pub recall_include_imported: bool,

    /// Scope of transcript sources eligible for ingest.
    ///
    /// Only `"current-project"` is honoured in Phase 1 (INV-6: project-root-anchored
    /// sources only). Other values are reserved for future cross-project modes.
    pub transcript_scope: String,
}

impl Default for KnowledgeConfig {
    fn default() -> Self {
        Self {
            ingest_provider: String::new(),
            concurrency: 3,
            max_documents: 0,
            recall_include_imported: true,
            transcript_scope: "current-project".to_owned(),
        }
    }
}
