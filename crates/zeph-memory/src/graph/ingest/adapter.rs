// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Sealed [`IngestSourceAdapter`] trait and its MVP implementations (spec-067 §2.3).
//!
//! Adapters are pure (no I/O) — they receive raw text (or pre-parsed JSONL) and produce
//! [`super::IngestDocument`] values. All I/O (disk reads, network) happens at the binary
//! layer before the adapter is invoked.
//!
//! External-agent adapters ([`ClaudeCodeJsonl`], [`CodexJsonl`]) are **strict and
//! version-pinned**: any record whose structure deviates from the pinned schema causes an
//! `Err` return rather than silent best-effort parsing. This is spec-067 Phase 3 requirement
//! D1 — unknown schema versions must fail loud.

use serde::Deserialize;
use zeph_llm::provider::Message;

use crate::MemoryError;
use crate::graph::ingest::document::IngestSourceKind;
use crate::graph::types::{GraphOrigin, GraphProvenance};

use super::document::IngestDocument;
use super::report::ImportBatchId;

mod sealed {
    pub trait Sealed {}
}

/// Build one [`IngestDocument`] per non-empty `texts[i]`, pairing it with `ids[i]` and the
/// `context_window` preceding texts.
///
/// Shared by every [`IngestSourceAdapter`] impl in this module — they differ only in how
/// `source_uri` is formatted and which [`GraphOrigin`] tags the provenance.
fn build_ingest_documents(
    texts: &[String],
    ids: &[String],
    context_window: usize,
    batch_id: &ImportBatchId,
    origin: GraphOrigin,
    source_uri: impl Fn(&str) -> String,
) -> Vec<IngestDocument> {
    let mut docs = Vec::with_capacity(texts.len());
    for (i, (content, id)) in texts.iter().zip(ids.iter()).enumerate() {
        if content.trim().is_empty() {
            continue;
        }
        let start = i.saturating_sub(context_window);
        let ctx: Vec<String> = texts[start..i].to_vec();
        let provenance = GraphProvenance {
            origin,
            import_batch_id: batch_id.as_str().to_owned(),
            source_uri: Some(source_uri(id)),
        };
        docs.push(IngestDocument::new(content.clone(), ctx, provenance));
    }
    docs
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
        let ids: Vec<String> = entries.iter().map(|e| e.seq.to_string()).collect();

        Ok(build_ingest_documents(
            &texts,
            &ids,
            self.context_window,
            batch_id,
            IngestSourceKind::SubagentTranscript.graph_origin(),
            |id| format!("subagent:{}#{id}", self.task_id),
        ))
    }
}

// ── Claude Code JSONL adapter ────────────────────────────────────────────────

/// Strict raw-record type for one line in a Claude Code `.jsonl` session file.
///
/// Only `type == "user" | "assistant"` lines are accepted; all other types
/// (mode, permission-mode, file-history-snapshot, …) are silently skipped.
/// A user/assistant line that lacks `message.role` is a schema-version error.
#[derive(Debug, Deserialize)]
struct ClaudeCodeRecord {
    #[serde(rename = "type")]
    record_type: String,
    #[serde(default)]
    uuid: Option<String>,
    message: Option<ClaudeCodeMessage>,
}

/// `message` field inside a Claude Code user/assistant record.
#[derive(Debug, Deserialize)]
struct ClaudeCodeMessage {
    role: String,
    content: ClaudeCodeContent,
}

/// Content in a Claude Code message: either a plain string or an array of blocks.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ClaudeCodeContent {
    Text(String),
    Blocks(Vec<ClaudeCodeBlock>),
}

/// One block inside a Claude Code content array.
#[derive(Debug, Deserialize)]
struct ClaudeCodeBlock {
    #[serde(rename = "type")]
    block_type: String,
    /// Present in `text` and `thinking` blocks.
    #[serde(default)]
    text: Option<String>,
    /// Present in `tool_use` blocks.
    #[serde(default)]
    name: Option<String>,
}

impl ClaudeCodeContent {
    /// Flatten all extractable parts into a single string.
    ///
    /// Extracts `text` and `thinking` blocks verbatim.
    /// For `tool_use` blocks, appends a short `<tool: name>` marker so that
    /// tool-heavy assistant turns are not silently dropped.
    fn to_text(&self) -> String {
        match self {
            Self::Text(s) => s.clone(),
            Self::Blocks(blocks) => blocks
                .iter()
                .filter_map(|b| match b.block_type.as_str() {
                    "text" | "thinking" => b.text.as_deref().map(str::to_owned),
                    "tool_use" => Some(format!(
                        "<tool: {}>",
                        b.name.as_deref().unwrap_or("unknown")
                    )),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }
}

/// Adapter for Claude Code session `.jsonl` files (spec-067 §9 Phase 3).
///
/// **Strict and version-pinned**: only `"type": "user"` and `"type": "assistant"` records
/// are processed. All other record types are silently skipped. A user/assistant record
/// that lacks the expected `message` structure causes an `Err` — no silent mis-parsing.
///
/// Files are expected to be scoped to the current project by the calling binary layer
/// (path enforcement via `~/.claude/projects/<project-slug>/`). This adapter performs
/// no I/O and no path checks; it receives raw text from the binary layer.
///
/// # Examples
///
/// ```no_run
/// use zeph_memory::graph::ingest::{ClaudeCodeJsonl, IngestSourceAdapter, ImportBatchId};
///
/// let raw = r#"{"type":"user","uuid":"abc","message":{"role":"user","content":"hello"}}"#;
/// let adapter = ClaudeCodeJsonl::new("session-42");
/// let batch = ImportBatchId::new();
/// let docs = adapter.parse(raw, &batch).unwrap();
/// assert!(!docs.is_empty());
/// ```
pub struct ClaudeCodeJsonl {
    session_id: String,
    context_window: usize,
}

impl ClaudeCodeJsonl {
    /// Creates a new `ClaudeCodeJsonl` adapter for the given session ID.
    ///
    /// `session_id` is embedded in every document's `source_uri` as
    /// `"claude-code:<session_id>#<uuid>"`.
    ///
    /// The `context_window` defaults to 3 preceding messages.
    #[must_use]
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
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

impl sealed::Sealed for ClaudeCodeJsonl {}

impl IngestSourceAdapter for ClaudeCodeJsonl {
    /// Parse a Claude Code `.jsonl` session file.
    ///
    /// Skips non-user/assistant records silently. A `user` or `assistant` record
    /// that lacks the expected `message` structure is a schema error and causes `Err`.
    fn parse(
        &self,
        raw: &str,
        batch_id: &ImportBatchId,
    ) -> Result<Vec<IngestDocument>, MemoryError> {
        let mut texts: Vec<String> = Vec::new();
        let mut uuids: Vec<String> = Vec::new();

        for (lineno, line) in raw.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }

            let record: ClaudeCodeRecord = serde_json::from_str(line).map_err(|e| {
                MemoryError::Ingest(format!(
                    "ClaudeCodeJsonl: line {lineno} JSON parse error: {e}"
                ))
            })?;

            // Skip non-conversation record types silently.
            if record.record_type != "user" && record.record_type != "assistant" {
                continue;
            }

            // A user/assistant record MUST have a valid `message` — fail loud.
            let msg = record.message.ok_or_else(|| {
                MemoryError::Ingest(format!(
                    "ClaudeCodeJsonl: line {lineno} has type={:?} but missing `message` field \
                     — unexpected schema version",
                    record.record_type
                ))
            })?;

            // role must be "user" or "assistant"
            if msg.role != "user" && msg.role != "assistant" {
                return Err(MemoryError::Ingest(format!(
                    "ClaudeCodeJsonl: line {lineno} unexpected message.role={:?}",
                    msg.role
                )));
            }

            texts.push(msg.content.to_text());
            uuids.push(record.uuid.unwrap_or_else(|| format!("line{lineno}")));
        }

        Ok(build_ingest_documents(
            &texts,
            &uuids,
            self.context_window,
            batch_id,
            IngestSourceKind::ExternalAgent.graph_origin(),
            |uuid| format!("claude-code:{}#{uuid}", self.session_id),
        ))
    }
}

// ── Codex JSONL adapter ──────────────────────────────────────────────────────

/// Strict raw-record type for one line in an `OpenAI` Codex CLI `.jsonl` session file.
///
/// Only `type == "response_item"` records with `payload.type == "message"` and
/// `payload.role` in `["user", "assistant"]` are accepted. The `session_meta` record
/// at the start of each file is used by the binary layer for project-scope enforcement
/// but is skipped by this adapter. All other record types are silently skipped.
#[derive(Debug, Deserialize)]
struct CodexRecord {
    #[serde(rename = "type")]
    record_type: String,
    #[serde(default)]
    payload: Option<CodexPayload>,
}

/// `payload` field in a Codex record.
#[derive(Debug, Deserialize)]
struct CodexPayload {
    #[serde(rename = "type")]
    payload_type: Option<String>,
    role: Option<String>,
    content: Option<Vec<CodexContentBlock>>,
    id: Option<String>,
}

/// One content block inside a Codex message.
#[derive(Debug, Deserialize)]
struct CodexContentBlock {
    #[serde(rename = "type")]
    block_type: String,
    text: Option<String>,
}

/// Adapter for `OpenAI` Codex CLI session `.jsonl` files (spec-067 §9 Phase 3).
///
/// **Strict and version-pinned**: only `"type": "response_item"` records with
/// `payload.type == "message"` and `payload.role` in `["user", "assistant"]` are
/// processed. Records with `role == "developer"` (system instructions) are skipped.
/// Unknown structure in a `response_item` causes an `Err`.
///
/// Project-scope enforcement (only files whose `session_meta.payload.cwd` matches the
/// current project root) is performed by the binary layer, not this adapter.
///
/// # Examples
///
/// ```no_run
/// use zeph_memory::graph::ingest::{CodexJsonl, IngestSourceAdapter, ImportBatchId};
///
/// let raw = concat!(
///     r#"{"type":"session_meta","payload":{"id":"s1","cwd":"/project"}}"#, "\n",
///     r#"{"type":"response_item","payload":{"type":"message","role":"user","id":"i1","content":[{"type":"input_text","text":"hello"}]}}"#
/// );
/// let adapter = CodexJsonl::new("session-1");
/// let batch = ImportBatchId::new();
/// let docs = adapter.parse(raw, &batch).unwrap();
/// assert_eq!(docs.len(), 1);
/// ```
pub struct CodexJsonl {
    session_id: String,
    context_window: usize,
}

impl CodexJsonl {
    /// Creates a new `CodexJsonl` adapter for the given session ID.
    ///
    /// `session_id` is embedded in every document's `source_uri` as
    /// `"codex:<session_id>#<item_id>"`.
    #[must_use]
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
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

impl sealed::Sealed for CodexJsonl {}

impl IngestSourceAdapter for CodexJsonl {
    /// Parse a Codex CLI `.jsonl` session file.
    ///
    /// Skips `session_meta`, `developer`-role messages, and all other non-message
    /// record types silently. A `response_item` record whose `payload` is missing or
    /// structurally invalid causes `Err`.
    fn parse(
        &self,
        raw: &str,
        batch_id: &ImportBatchId,
    ) -> Result<Vec<IngestDocument>, MemoryError> {
        let mut texts: Vec<String> = Vec::new();
        let mut item_ids: Vec<String> = Vec::new();

        for (lineno, line) in raw.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }

            let record: CodexRecord = serde_json::from_str(line).map_err(|e| {
                MemoryError::Ingest(format!("CodexJsonl: line {lineno} JSON parse error: {e}"))
            })?;

            // Skip session_meta and all other non-response_item records.
            if record.record_type != "response_item" {
                continue;
            }

            // response_item MUST have a payload — fail loud.
            let payload = record.payload.ok_or_else(|| {
                MemoryError::Ingest(format!(
                    "CodexJsonl: line {lineno} has type=response_item but missing `payload` \
                     — unexpected schema version"
                ))
            })?;

            // Must be a message payload.
            let payload_type = payload.payload_type.as_deref().unwrap_or("");
            if payload_type != "message" {
                continue; // tool_call, tool_output, etc. — skip silently
            }

            let role = payload.role.as_deref().unwrap_or("");
            match role {
                "user" | "assistant" => {}
                "developer" => continue, // system-level instructions — skip
                _ => {
                    return Err(MemoryError::Ingest(format!(
                        "CodexJsonl: line {lineno} unexpected payload.role={role:?}"
                    )));
                }
            }

            // Extract text from input_text (user turns) and output_text (assistant turns).
            let content_text: String = payload
                .content
                .as_deref()
                .unwrap_or_default()
                .iter()
                .filter(|b| b.block_type == "input_text" || b.block_type == "output_text")
                .filter_map(|b| b.text.as_deref())
                .collect::<Vec<_>>()
                .join("\n");

            let item_id = payload.id.unwrap_or_else(|| format!("line{lineno}"));

            texts.push(content_text);
            item_ids.push(item_id);
        }

        Ok(build_ingest_documents(
            &texts,
            &item_ids,
            self.context_window,
            batch_id,
            IngestSourceKind::ExternalAgent.graph_origin(),
            |item_id| format!("codex:{}#{item_id}", self.session_id),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::types::GraphOrigin;

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

    // ── ClaudeCodeJsonl tests ────────────────────────────────────────────────

    fn make_claude_code_user(uuid: &str, text: &str) -> String {
        format!(
            r#"{{"type":"user","uuid":"{uuid}","message":{{"role":"user","content":{text:?}}}}}"#
        )
    }

    fn make_claude_code_assistant(uuid: &str, text: &str) -> String {
        format!(
            r#"{{"type":"assistant","uuid":"{uuid}","message":{{"role":"assistant","content":[{{"type":"text","text":{text:?}}}]}}}}"#
        )
    }

    fn make_claude_code_non_conversation(record_type: &str) -> String {
        format!(r#"{{"type":"{record_type}","sessionId":"s1"}}"#)
    }

    #[test]
    fn claude_code_parses_user_message() {
        let raw = make_claude_code_user("uuid-1", "Hello from user");
        let adapter = ClaudeCodeJsonl::new("sess-1");
        let batch = ImportBatchId::new();
        let docs = adapter.parse(&raw, &batch).unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].content(), "Hello from user");
        assert_eq!(docs[0].source_uri(), "claude-code:sess-1#uuid-1");
        assert_eq!(docs[0].provenance().origin, GraphOrigin::ExternalAgent);
    }

    #[test]
    fn claude_code_parses_assistant_array_content() {
        let raw = make_claude_code_assistant("uuid-2", "Hello from assistant");
        let adapter = ClaudeCodeJsonl::new("sess-2");
        let batch = ImportBatchId::new();
        let docs = adapter.parse(&raw, &batch).unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].content(), "Hello from assistant");
    }

    #[test]
    fn claude_code_skips_non_conversation_records() {
        let raw = [
            make_claude_code_non_conversation("mode"),
            make_claude_code_non_conversation("permission-mode"),
            make_claude_code_non_conversation("file-history-snapshot"),
            make_claude_code_user("uuid-3", "actual content"),
        ]
        .join("\n");
        let adapter = ClaudeCodeJsonl::new("sess-3");
        let batch = ImportBatchId::new();
        let docs = adapter.parse(&raw, &batch).unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].content(), "actual content");
    }

    #[test]
    fn claude_code_errors_on_missing_message_in_user_record() {
        let raw = r#"{"type":"user","uuid":"u1"}"#; // no "message" field
        let adapter = ClaudeCodeJsonl::new("sess-err");
        let batch = ImportBatchId::new();
        assert!(adapter.parse(raw, &batch).is_err());
    }

    #[test]
    fn claude_code_skips_empty_content() {
        let raw = [
            make_claude_code_user("u1", ""),
            make_claude_code_user("u2", "non-empty"),
        ]
        .join("\n");
        let adapter = ClaudeCodeJsonl::new("sess-4");
        let batch = ImportBatchId::new();
        let docs = adapter.parse(&raw, &batch).unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].content(), "non-empty");
    }

    #[test]
    fn claude_code_tags_external_agent_origin() {
        let raw = make_claude_code_user("u1", "some content");
        let adapter = ClaudeCodeJsonl::new("sess-5");
        let batch = ImportBatchId::new();
        let docs = adapter.parse(&raw, &batch).unwrap();
        assert_eq!(docs[0].provenance().origin, GraphOrigin::ExternalAgent);
    }

    #[test]
    fn claude_code_embeds_batch_id_in_provenance() {
        let raw = make_claude_code_user("u1", "content");
        let adapter = ClaudeCodeJsonl::new("sess-6");
        let batch = ImportBatchId::new();
        let docs = adapter.parse(&raw, &batch).unwrap();
        assert_eq!(docs[0].provenance().import_batch_id, batch.as_str());
    }

    #[test]
    fn claude_code_context_window() {
        let raw = [
            make_claude_code_user("u1", "first"),
            make_claude_code_user("u2", "second"),
            make_claude_code_user("u3", "third"),
            make_claude_code_user("u4", "fourth"),
        ]
        .join("\n");
        let adapter = ClaudeCodeJsonl::new("sess-7").with_context_window(2);
        let batch = ImportBatchId::new();
        let docs = adapter.parse(&raw, &batch).unwrap();
        assert_eq!(docs.len(), 4);
        assert!(docs[0].context().is_empty());
        assert_eq!(docs[1].context().len(), 1);
        assert_eq!(docs[2].context().len(), 2);
        assert_eq!(docs[3].context().len(), 2); // capped at window
    }

    #[test]
    fn claude_code_thinking_block_extracted() {
        // Assistant record with only a thinking block — must not be dropped.
        let raw = r#"{"type":"assistant","uuid":"u1","message":{"role":"assistant","content":[{"type":"thinking","text":"inner reasoning"}]}}"#;
        let adapter = ClaudeCodeJsonl::new("sess-think");
        let batch = ImportBatchId::new();
        let docs = adapter.parse(raw, &batch).unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].content(), "inner reasoning");
    }

    #[test]
    fn claude_code_tool_use_block_produces_marker() {
        let raw = r#"{"type":"assistant","uuid":"u1","message":{"role":"assistant","content":[{"type":"tool_use","name":"shell","text":null}]}}"#;
        let adapter = ClaudeCodeJsonl::new("sess-tool");
        let batch = ImportBatchId::new();
        let docs = adapter.parse(raw, &batch).unwrap();
        assert_eq!(docs.len(), 1);
        assert!(docs[0].content().contains("<tool: shell>"));
    }

    #[test]
    fn claude_code_empty_raw() {
        let adapter = ClaudeCodeJsonl::new("sess-empty");
        let batch = ImportBatchId::new();
        let docs = adapter.parse("", &batch).unwrap();
        assert!(docs.is_empty());
    }

    // ── CodexJsonl tests ─────────────────────────────────────────────────────

    fn make_codex_session_meta(cwd: &str) -> String {
        format!(
            r#"{{"type":"session_meta","payload":{{"id":"s1","cwd":"{cwd}","originator":"codex_cli_rs","cli_version":"1.0.0"}}}}"#
        )
    }

    fn make_codex_response_item(role: &str, text: &str, id: &str) -> String {
        format!(
            r#"{{"type":"response_item","payload":{{"type":"message","role":"{role}","id":"{id}","content":[{{"type":"input_text","text":{text:?}}}]}}}}"#
        )
    }

    fn make_codex_assistant(text: &str, id: &str) -> String {
        format!(
            r#"{{"type":"response_item","payload":{{"type":"message","role":"assistant","id":"{id}","content":[{{"type":"output_text","text":{text:?}}}]}}}}"#
        )
    }

    #[test]
    fn codex_parses_user_message() {
        let raw = [
            make_codex_session_meta("/project"),
            make_codex_response_item("user", "Hello", "item-1"),
        ]
        .join("\n");
        let adapter = CodexJsonl::new("sess-c1");
        let batch = ImportBatchId::new();
        let docs = adapter.parse(&raw, &batch).unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].content(), "Hello");
        assert_eq!(docs[0].source_uri(), "codex:sess-c1#item-1");
        assert_eq!(docs[0].provenance().origin, GraphOrigin::ExternalAgent);
    }

    #[test]
    fn codex_skips_developer_role() {
        let raw = [
            make_codex_session_meta("/project"),
            make_codex_response_item("developer", "System instructions", "sys-1"),
            make_codex_response_item("user", "User message", "u-1"),
        ]
        .join("\n");
        let adapter = CodexJsonl::new("sess-c2");
        let batch = ImportBatchId::new();
        let docs = adapter.parse(&raw, &batch).unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].content(), "User message");
    }

    #[test]
    fn codex_errors_on_missing_payload_in_response_item() {
        let raw = r#"{"type":"response_item"}"#; // no payload
        let adapter = CodexJsonl::new("sess-c-err");
        let batch = ImportBatchId::new();
        assert!(adapter.parse(raw, &batch).is_err());
    }

    #[test]
    fn codex_tags_external_agent_origin() {
        let raw = make_codex_response_item("assistant", "answer", "a-1");
        let adapter = CodexJsonl::new("sess-c3");
        let batch = ImportBatchId::new();
        let docs = adapter.parse(&raw, &batch).unwrap();
        assert_eq!(docs[0].provenance().origin, GraphOrigin::ExternalAgent);
    }

    #[test]
    fn codex_extracts_output_text_from_assistant_turns() {
        // Real Codex assistant turns use output_text, not input_text.
        let raw = [
            make_codex_session_meta("/project"),
            make_codex_assistant("assistant answer", "a-1"),
        ]
        .join("\n");
        let adapter = CodexJsonl::new("sess-c4");
        let batch = ImportBatchId::new();
        let docs = adapter.parse(&raw, &batch).unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].content(), "assistant answer");
    }

    #[test]
    fn codex_embeds_batch_id_in_provenance() {
        let raw = make_codex_response_item("user", "hello", "u-1");
        let adapter = CodexJsonl::new("sess-c5");
        let batch = ImportBatchId::new();
        let docs = adapter.parse(&raw, &batch).unwrap();
        assert_eq!(docs[0].provenance().import_batch_id, batch.as_str());
    }

    #[test]
    fn codex_context_window() {
        let raw = [
            make_codex_response_item("user", "msg1", "i1"),
            make_codex_response_item("user", "msg2", "i2"),
            make_codex_response_item("user", "msg3", "i3"),
            make_codex_response_item("user", "msg4", "i4"),
        ]
        .join("\n");
        let adapter = CodexJsonl::new("sess-c6").with_context_window(2);
        let batch = ImportBatchId::new();
        let docs = adapter.parse(&raw, &batch).unwrap();
        assert_eq!(docs.len(), 4);
        assert!(docs[0].context().is_empty());
        assert_eq!(docs[1].context().len(), 1);
        assert_eq!(docs[2].context().len(), 2);
        assert_eq!(docs[3].context().len(), 2);
    }

    #[test]
    fn codex_skips_empty_content() {
        let raw = [
            make_codex_response_item("user", "", "empty-1"),
            make_codex_response_item("user", "non-empty", "u-2"),
        ]
        .join("\n");
        let adapter = CodexJsonl::new("sess-c7");
        let batch = ImportBatchId::new();
        let docs = adapter.parse(&raw, &batch).unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].content(), "non-empty");
    }
}
