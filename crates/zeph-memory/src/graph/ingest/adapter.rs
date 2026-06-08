// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Sealed [`IngestSourceAdapter`] trait and its MVP implementations (spec-067 §2.3).
//!
//! Adapters are pure (no I/O) — they receive raw text (or pre-parsed JSONL) and produce
//! [`super::IngestDocument`] values. All I/O (disk reads, network) happens at the binary
//! layer before the adapter is invoked.

use serde::Deserialize;
use zeph_llm::provider::Message;

use crate::MemoryError;
use crate::graph::ingest::document::IngestSourceKind;
use crate::graph::types::GraphProvenance;

use super::document::IngestDocument;
use super::report::ImportBatchId;

mod sealed {
    pub trait Sealed {}
}

/// Adapter that converts raw source material into [`IngestDocument`] values.
///
/// The trait is sealed — only implementations in `zeph-memory` are permitted.
/// This preserves the valid-by-construction invariants of [`IngestDocument`]:
/// non-empty content, non-empty source URI, and a hash computed from content.
///
/// # Design
///
/// Adapters are pure: no async, no I/O, no external crate dependencies beyond
/// `zeph-llm` (which `zeph-memory` already depends on). The binary layer reads
/// raw bytes off disk and passes the string in via [`IngestSourceAdapter::parse`].
pub trait IngestSourceAdapter: sealed::Sealed + Send + Sync {
    /// Parse `raw` source material into a list of [`IngestDocument`] values.
    ///
    /// `batch_id` is embedded in every document's provenance so that all documents
    /// produced in one `ingest_documents` call share the same rollback key.
    ///
    /// # Contract
    ///
    /// Implementations SHOULD skip malformed individual records (e.g. a single
    /// unparseable JSONL line) and continue parsing the rest of the input — partial
    /// parse errors are not fatal.  `Err` is returned only for an irrecoverable
    /// format error where no documents could be produced from `raw` at all.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError::Ingest`] only for irrecoverable format errors where
    /// no documents could be produced from `raw`. Malformed individual records are
    /// skipped with a warning and do not cause an `Err` return.
    fn parse(
        &self,
        raw: &str,
        batch_id: &ImportBatchId,
    ) -> Result<Vec<IngestDocument>, MemoryError>;
}

/// A single entry in a subagent JSONL transcript.
///
/// The binary (`zeph-subagent`) writes transcripts as one JSON object per line.
/// Each entry wraps a [`Message`] produced by the LLM or the agent.
///
/// Only `message` is used by the adapter — `seq` and `timestamp` are available
/// for diagnostic purposes but are not stored in the graph.
#[derive(Debug, Deserialize)]
pub struct TranscriptEntry {
    /// Zero-based sequence number within the session.
    pub seq: u64,
    /// ISO-8601 timestamp (opaque string, not parsed).
    pub timestamp: Option<String>,
    /// The LLM or agent message for this turn.
    pub message: Message,
}

/// Adapter for subagent JSONL transcripts.
///
/// Each line in the transcript is a [`TranscriptEntry`]. The adapter produces one
/// [`IngestDocument`] per entry whose flat text content is non-empty. Context for
/// each entry is the plain text of the `context_window` preceding entries.
///
/// # Examples
///
/// ```no_run
/// use zeph_memory::graph::ingest::{SubagentJsonl, IngestSourceAdapter, ImportBatchId};
///
/// let raw = r#"{"seq":0,"timestamp":"2026-01-01T00:00:00Z","message":{"role":"user","content":"hello","parts":[]}}"#;
/// let adapter = SubagentJsonl::new("task-42");
/// let batch = ImportBatchId::new();
/// let docs = adapter.parse(raw, &batch).unwrap();
/// assert!(!docs.is_empty());
/// ```
pub struct SubagentJsonl {
    task_id: String,
    /// Number of preceding messages used as extraction context.
    context_window: usize,
}

impl SubagentJsonl {
    /// Creates a new `SubagentJsonl` adapter for the given task ID.
    ///
    /// `task_id` is embedded in every document's `source_uri` as
    /// `"subagent:<task_id>#<seq>"`.
    ///
    /// The `context_window` defaults to 3 preceding messages.
    #[must_use]
    pub fn new(task_id: impl Into<String>) -> Self {
        Self {
            task_id: task_id.into(),
            context_window: 3,
        }
    }

    /// Overrides the context window size.
    #[must_use]
    pub fn with_context_window(mut self, n: usize) -> Self {
        self.context_window = n;
        self
    }
}

impl sealed::Sealed for SubagentJsonl {}

impl IngestSourceAdapter for SubagentJsonl {
    /// Parse a JSONL transcript string.
    ///
    /// Lines that fail to parse are skipped with a warning; entries whose flat
    /// text content is empty are also skipped (nothing to extract).
    fn parse(
        &self,
        raw: &str,
        batch_id: &ImportBatchId,
    ) -> Result<Vec<IngestDocument>, MemoryError> {
        let entries: Vec<TranscriptEntry> = raw
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|line| match serde_json::from_str::<TranscriptEntry>(line) {
                Ok(e) => Some(e),
                Err(err) => {
                    tracing::warn!("SubagentJsonl: skipping malformed line: {err}");
                    None
                }
            })
            .collect();

        let texts: Vec<String> = entries
            .iter()
            .map(|e| e.message.to_llm_content().to_owned())
            .collect();

        let mut docs = Vec::with_capacity(entries.len());
        for (i, entry) in entries.iter().enumerate() {
            let content = texts[i].clone();
            if content.trim().is_empty() {
                continue;
            }
            let start = i.saturating_sub(self.context_window);
            let ctx: Vec<String> = texts[start..i].to_vec();

            let source_uri = format!("subagent:{}#{}", self.task_id, entry.seq);
            let provenance = GraphProvenance {
                origin: IngestSourceKind::SubagentTranscript.graph_origin(),
                import_batch_id: batch_id.as_str().to_owned(),
                source_uri: Some(source_uri),
            };
            docs.push(IngestDocument::new(content, ctx, provenance));
        }

        Ok(docs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_jsonl(entries: &[(u64, &str)]) -> String {
        entries
            .iter()
            .map(|(seq, text)| {
                format!(
                    r#"{{"seq":{seq},"timestamp":null,"message":{{"role":"user","content":{text:?},"parts":[]}}}}"#,
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn parse_single_entry() {
        let raw = make_jsonl(&[(0, "Rust is a systems language")]);
        let adapter = SubagentJsonl::new("task-1");
        let batch = ImportBatchId::new();
        let docs = adapter.parse(&raw, &batch).unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].content(), "Rust is a systems language");
        assert_eq!(docs[0].source_uri(), "subagent:task-1#0");
        assert!(!docs[0].content_hash().is_empty());
    }

    #[test]
    fn parse_skips_empty_content() {
        let raw = make_jsonl(&[(0, ""), (1, "non-empty content")]);
        let adapter = SubagentJsonl::new("task-2");
        let batch = ImportBatchId::new();
        let docs = adapter.parse(&raw, &batch).unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].content(), "non-empty content");
    }

    #[test]
    fn parse_builds_context_window() {
        let raw = make_jsonl(&[
            (0, "first message"),
            (1, "second message"),
            (2, "third message"),
            (3, "fourth message"),
        ]);
        let adapter = SubagentJsonl::new("task-3").with_context_window(2);
        let batch = ImportBatchId::new();
        let docs = adapter.parse(&raw, &batch).unwrap();
        assert_eq!(docs.len(), 4);
        assert!(docs[0].context().is_empty());
        assert_eq!(docs[1].context().len(), 1);
        assert_eq!(docs[2].context().len(), 2);
        assert_eq!(docs[3].context().len(), 2); // capped at window size
    }

    #[test]
    fn parse_skips_malformed_lines() {
        let raw = "not json\n".to_owned() + &make_jsonl(&[(0, "valid message")]);
        let adapter = SubagentJsonl::new("task-4");
        let batch = ImportBatchId::new();
        let docs = adapter.parse(&raw, &batch).unwrap();
        assert_eq!(docs.len(), 1);
    }

    #[test]
    fn parse_embeds_batch_id_in_provenance() {
        let raw = make_jsonl(&[(0, "content")]);
        let adapter = SubagentJsonl::new("task-5");
        let batch = ImportBatchId::new();
        let docs = adapter.parse(&raw, &batch).unwrap();
        assert_eq!(docs[0].provenance().import_batch_id, batch.as_str());
    }

    #[test]
    fn parse_empty_raw_returns_empty_vec() {
        let adapter = SubagentJsonl::new("task-6");
        let batch = ImportBatchId::new();
        let docs = adapter.parse("", &batch).unwrap();
        assert!(docs.is_empty());
    }
}
