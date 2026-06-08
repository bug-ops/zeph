// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Ingest-document types for `zeph knowledge ingest` (spec-067 §2.3).

use crate::graph::ingest::ledger::IngestLedger;
use crate::graph::types::{GraphOrigin, GraphProvenance};

/// Classifies the origin of documents fed to [`super::IngestDocument`].
///
/// Used to select the appropriate LLM extraction prompt and determine the
/// [`GraphOrigin`] value stamped on extracted entities and edges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum IngestSourceKind {
    /// A versioned repository artifact such as a spec, design doc, or README.
    ///
    /// Uses the tech-doc extraction prompt (precise, structured).
    StaticArtifact,

    /// A transcript produced by a Zeph subagent task session.
    ///
    /// Uses the tech-doc extraction prompt (entity-focused, not conversational).
    SubagentTranscript,

    /// Content sourced from a foreign agent system (A2A / ACP / external API).
    ///
    /// Uses the tech-doc extraction prompt.
    ///
    /// # Note
    ///
    /// Trust classification for external agents is resolved at the binary layer
    /// (spec-067 §3 D1); `zeph-memory` treats `ExternalAgent` the same as
    /// `StaticArtifact` for prompt selection.
    ExternalAgent,
    // TODO(D1): add per-source trust classification once the trust-level spec is finalised.
}

impl IngestSourceKind {
    /// Returns the LLM system prompt appropriate for this source kind.
    ///
    /// All current variants use the tech-doc prompt; the conversational prompt
    /// (used by the live-conversation path) is intentionally NOT selectable here.
    ///
    /// # Note
    ///
    /// The return type is `&'static str` — prompts must be compile-time constants.
    /// This is a known MVP limitation (see C7 in critic handoff); runtime-configurable
    /// prompts will require a breaking change to this signature.
    #[must_use]
    pub fn system_prompt(self) -> &'static str {
        match self {
            Self::StaticArtifact | Self::SubagentTranscript | Self::ExternalAgent => {
                super::prompt::TECH_DOC_SYSTEM_PROMPT
            }
        }
    }

    /// Returns the [`GraphOrigin`] value stamped on entities extracted from this kind.
    #[must_use]
    pub fn graph_origin(self) -> GraphOrigin {
        match self {
            Self::SubagentTranscript => GraphOrigin::Subagent,
            Self::StaticArtifact | Self::ExternalAgent => GraphOrigin::Ingest,
        }
    }
}

/// A validated, hash-stamped document ready for graph extraction.
///
/// Created exclusively through [`super::IngestSourceAdapter::parse`] implementations,
/// which ensure:
/// - `content` is non-empty.
/// - `source_uri` is non-empty.
/// - `content_hash` is the canonical BLAKE3 hex digest of `content`.
/// - `provenance` is fully populated with `GraphOrigin::Ingest`.
///
/// # Examples
///
/// ```no_run
/// use zeph_memory::graph::ingest::{IngestDocument, IngestSourceKind};
///
/// // Documents are created by adapters. Direct construction is crate-private.
/// // This example demonstrates what the adapter produces.
/// # let doc: IngestDocument = unimplemented!("use SubagentJsonl adapter");
/// let uri = doc.source_uri();
/// let hash = doc.content_hash();
/// let content = doc.content();
/// ```
#[derive(Debug, Clone)]
pub struct IngestDocument {
    content: String,
    context: Vec<String>,
    provenance: GraphProvenance,
    content_hash: String,
}

impl IngestDocument {
    /// Constructs a new `IngestDocument`.
    ///
    /// This constructor is `pub(super)` — only adapters in the `graph::ingest` module
    /// may build documents, ensuring the valid-by-construction invariants hold.
    pub(super) fn new(
        content: String,
        prior_messages: Vec<String>,
        provenance: GraphProvenance,
    ) -> Self {
        let content_hash = IngestLedger::content_hash(content.as_bytes());
        Self {
            content,
            context: prior_messages,
            provenance,
            content_hash,
        }
    }

    /// The raw text content to be extracted.
    #[must_use]
    pub fn content(&self) -> &str {
        &self.content
    }

    /// Recent context messages that precede this document (used as extraction context).
    #[must_use]
    pub fn context(&self) -> &[String] {
        &self.context
    }

    /// Provenance metadata (origin, batch ID, source URI).
    #[must_use]
    pub fn provenance(&self) -> &GraphProvenance {
        &self.provenance
    }

    /// The canonical BLAKE3 hex digest of [`Self::content`].
    ///
    /// Computed once at construction time and cached.
    #[must_use]
    pub fn content_hash(&self) -> &str {
        &self.content_hash
    }

    /// Repository-relative source locator, e.g. `"subagent:<task_id>#42"`.
    ///
    /// Convenience accessor for `provenance().source_uri` — guaranteed non-`None`
    /// for all documents created by adapters.
    #[must_use]
    pub fn source_uri(&self) -> &str {
        self.provenance.source_uri.as_deref().unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subagent_transcript_maps_to_subagent_origin() {
        assert_eq!(
            IngestSourceKind::SubagentTranscript.graph_origin(),
            GraphOrigin::Subagent
        );
    }

    #[test]
    fn static_artifact_maps_to_ingest_origin() {
        assert_eq!(
            IngestSourceKind::StaticArtifact.graph_origin(),
            GraphOrigin::Ingest
        );
    }
}
