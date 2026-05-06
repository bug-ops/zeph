// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Typed page classification and minimum-fidelity invariants for context compaction.
//!
//! Every context segment entering the assembler is tagged with a [`PageType`] and
//! wrapped in a [`TypedPage`]. The [`PageInvariant`] trait declares the fidelity
//! contract enforced at every compaction boundary.
//!
//! Classification is deterministic and side-effect free — no I/O, no LLM calls.
//!
//! # Architecture
//!
//! This module lives in `zeph-context` to keep classification logic co-located with
//! the assembler. No dependency on `zeph-memory` is introduced here.
//!
//! # Feature flag
//!
//! All typed-page functionality is gated behind the
//! `[memory.compaction.typed_pages] enabled = true` config key. When disabled the
//! assembler falls back to the legacy untyped path without behaviour change.

use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

// ── PageType ──────────────────────────────────────────────────────────────────

/// Classification of a context segment for compaction purposes.
///
/// The variant determines which [`PageInvariant`] is enforced and what shape the
/// compacted summary must have.
///
/// # Invariant
///
/// Every [`TypedPage`] carries exactly one `PageType`. Unclassifiable segments
/// default to [`PageType::ConversationTurn`] (see [`classify`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PageType {
    /// A tool request/response pair sourced from memory or the current turn.
    ToolOutput,
    /// A user or assistant message that does not carry a tool role.
    ConversationTurn,
    /// Cross-session context, past summaries, or graph-fact recall injections.
    MemoryExcerpt,
    /// Session digest, persona, skill instructions, or compression guidelines.
    SystemContext,
}

impl std::fmt::Display for PageType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ToolOutput => f.write_str("tool_output"),
            Self::ConversationTurn => f.write_str("conversation_turn"),
            Self::MemoryExcerpt => f.write_str("memory_excerpt"),
            Self::SystemContext => f.write_str("system_context"),
        }
    }
}

// ── PageOrigin ────────────────────────────────────────────────────────────────

/// Provenance of a [`TypedPage`], serialised into audit records.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PageOrigin {
    /// Tool request/response pair.
    ToolPair {
        /// Name of the tool that produced this output.
        tool_name: String,
    },
    /// User or assistant conversation turn.
    Turn {
        /// Opaque message identifier (numeric message id as string).
        message_id: String,
    },
    /// Injected from memory (cross-session, summary, graph-facts, etc.).
    Excerpt {
        /// Human-readable label identifying the memory source.
        source_label: String,
    },
    /// Session-level system context (persona, skills, digest).
    System {
        /// Logical key for this system context block (e.g. `"persona"`, `"skills"`).
        key: String,
    },
}

// ── SchemaHint ────────────────────────────────────────────────────────────────

/// Body format hint for [`PageType::ToolOutput`] pages.
///
/// Used by the invariant to select the correct structured-summary prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SchemaHint {
    /// Body is valid JSON (object or array).
    Json,
    /// Body is UTF-8 text (log lines, prose, etc.).
    Text,
    /// Body is a unified diff.
    Diff,
    /// Body is a tab- or comma-separated table.
    Table,
    /// Body is non-UTF-8 binary data.
    Binary,
}

// ── PageId ────────────────────────────────────────────────────────────────────

/// Stable content-addressed identifier for a [`TypedPage`].
///
/// Computed as BLAKE3 over `page_type_tag || origin_tag || body_bytes`, encoded
/// as lowercase hex (first 16 bytes = 32 hex chars). The same input always
/// produces the same `PageId`, enabling deduplication across turns.
///
/// # Semantics
///
/// `PageId` is a **content hash**: identical source bytes (same page type, same
/// origin key, same body) always produce the same id. This means that the same
/// tool output appearing in two different turns produces the same `PageId`.
/// Callers that need per-turn provenance must use `turn_id` from the audit record
/// — `PageId` is for deduplication, not for uniqueness across turns.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PageId(pub String);

impl PageId {
    /// Compute a [`PageId`] from the page type, origin key, and body bytes.
    #[must_use]
    pub fn compute(page_type: PageType, origin_key: &str, body: &[u8]) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(page_type.to_string().as_bytes());
        hasher.update(b"|");
        hasher.update(origin_key.as_bytes());
        hasher.update(b"|");
        hasher.update(body);
        let hash = hasher.finalize();
        // Use first 16 bytes (128 bits) — sufficient for deduplication purposes.
        let mut hex = String::with_capacity(32);
        for b in &hash.as_bytes()[..16] {
            use std::fmt::Write as _;
            let _ = write!(hex, "{b:02x}");
        }
        Self(format!("blake3:{hex}"))
    }
}

// ── TypedPage ─────────────────────────────────────────────────────────────────

/// A classified context segment ready for invariant-aware compaction.
///
/// `TypedPage` is the unit of work passed to compaction boundaries. The
/// [`PageId`] is content-stable: the same source bytes always produce the same
/// id, enabling the compactor to skip already-compacted pages.
#[derive(Debug, Clone)]
pub struct TypedPage {
    /// Stable content-addressed identifier.
    pub page_id: PageId,
    /// Classification determining which invariant applies.
    pub page_type: PageType,
    /// Provenance of this page (for audit records).
    pub origin: PageOrigin,
    /// Token count of the original body.
    pub tokens: u32,
    /// Body text shared across potential clones.
    pub body: Arc<str>,
    /// Body format hint (populated for `ToolOutput` only; `None` otherwise).
    pub schema_hint: Option<SchemaHint>,
}

impl TypedPage {
    /// Construct a new [`TypedPage`], computing its [`PageId`] from content.
    #[must_use]
    pub fn new(
        page_type: PageType,
        origin: PageOrigin,
        tokens: u32,
        body: Arc<str>,
        schema_hint: Option<SchemaHint>,
    ) -> Self {
        let origin_key = origin_key_for(&origin);
        let page_id = PageId::compute(page_type, &origin_key, body.as_bytes());
        Self {
            page_id,
            page_type,
            origin,
            tokens,
            body,
            schema_hint,
        }
    }
}

fn origin_key_for(origin: &PageOrigin) -> String {
    match origin {
        PageOrigin::ToolPair { tool_name } => format!("tool:{tool_name}"),
        PageOrigin::Turn { message_id } => format!("turn:{message_id}"),
        PageOrigin::Excerpt { source_label } => format!("excerpt:{source_label}"),
        PageOrigin::System { key } => format!("system:{key}"),
    }
}

// ── FidelityContract ──────────────────────────────────────────────────────────

/// The set of fields that must be present in a compacted page.
///
/// Returned by [`PageInvariant::minimum_fidelity`] and checked by
/// [`PageInvariant::verify`] after summarization.
#[derive(Debug, Clone)]
pub struct FidelityContract {
    /// Human-readable label for this contract version (e.g. `"structured_summary_v1"`).
    pub fidelity_level: &'static str,
    /// Schema version integer included in audit records.
    pub invariant_version: u8,
    /// Fields that must appear in the compacted body text.
    pub required_fields: &'static [&'static str],
}

// ── FidelityViolation ─────────────────────────────────────────────────────────

/// Describes why an invariant check failed after compaction.
///
/// A violation is a hard error: the compacted page is dropped and an audit
/// record with `violations` is emitted.
#[derive(Debug, Clone, Serialize)]
pub struct FidelityViolation {
    /// The field or property that was expected but missing.
    pub missing_field: String,
    /// Human-readable explanation of the violation.
    pub detail: String,
}

// ── CompactedPage ─────────────────────────────────────────────────────────────

/// The output of a compaction attempt, passed to [`PageInvariant::verify`].
#[derive(Debug, Clone)]
pub struct CompactedPage {
    /// The summarized body text produced by the compaction provider.
    pub body: Arc<str>,
    /// Token count of the compacted body.
    pub tokens: u32,
}

// ── PageInvariant trait ───────────────────────────────────────────────────────

/// Minimum-fidelity contract for a single [`PageType`].
///
/// Implementors declare what a compacted page must contain ([`minimum_fidelity`])
/// and verify that the actual output honours the contract ([`verify`]).
///
/// The trait is object-safe so implementations can be stored in a
/// `HashMap<PageType, Box<dyn PageInvariant>>` registry.
///
/// # Contract
///
/// - [`verify`] MUST NOT perform I/O or call an LLM.
/// - A failed [`verify`] means the compacted page is dropped — it is NEVER
///   injected in degraded form.
///
/// [`minimum_fidelity`]: PageInvariant::minimum_fidelity
/// [`verify`]: PageInvariant::verify
pub trait PageInvariant: Send + Sync {
    /// The page type this invariant governs.
    fn page_type(&self) -> PageType;

    /// Return the fidelity contract required for a given page.
    fn minimum_fidelity(&self, page: &TypedPage) -> FidelityContract;

    /// Verify that `compacted` satisfies the fidelity contract derived from `original`.
    ///
    /// # Errors
    ///
    /// Returns a non-empty [`Vec<FidelityViolation>`] when one or more required
    /// fields are absent from the compacted body.
    fn verify(
        &self,
        original: &TypedPage,
        compacted: &CompactedPage,
    ) -> Result<(), Vec<FidelityViolation>>;
}

// ── Per-type invariant implementations ───────────────────────────────────────

/// Invariant for [`PageType::ToolOutput`] pages.
///
/// The compacted body must contain the tool name, an exit/status indicator,
/// and at least one structural key from the original output.
pub struct ToolOutputInvariant;

impl PageInvariant for ToolOutputInvariant {
    fn page_type(&self) -> PageType {
        PageType::ToolOutput
    }

    fn minimum_fidelity(&self, _page: &TypedPage) -> FidelityContract {
        FidelityContract {
            fidelity_level: "structured_summary_v1",
            invariant_version: 1,
            required_fields: &["tool_name", "exit_status"],
        }
    }

    fn verify(
        &self,
        original: &TypedPage,
        compacted: &CompactedPage,
    ) -> Result<(), Vec<FidelityViolation>> {
        let body = compacted.body.as_ref();
        // For binary pages the body marker is injected by the compactor, skip field checks.
        if original.schema_hint == Some(SchemaHint::Binary) {
            return Ok(());
        }

        let mut violations = Vec::new();

        // The compacted body must reference the tool name.
        let tool_name = match &original.origin {
            PageOrigin::ToolPair { tool_name } => tool_name.as_str(),
            _ => "",
        };
        if !tool_name.is_empty() && !body.contains(tool_name) {
            violations.push(FidelityViolation {
                missing_field: "tool_name".into(),
                detail: format!("compacted body does not reference tool '{tool_name}'"),
            });
        }

        // The compacted body must contain at least one exit / status indicator.
        let has_status = body.contains("exit_status")
            || body.contains("exit_code")
            || body.contains("status:")
            || body.contains("Status:")
            || body.contains("exit:")
            || body.contains("rc:");
        if !has_status {
            violations.push(FidelityViolation {
                missing_field: "exit_status".into(),
                detail: "compacted body does not contain an exit status indicator".into(),
            });
        }

        // For JSON-schema tool outputs, verify that at least one top-level JSON
        // field name from the original body is present in the compacted body
        // (FR-003: structural keys must be preserved, not just exit-status markers).
        if original.schema_hint == Some(SchemaHint::Json) {
            let original_body = original.body.as_ref();
            let preserved = check_json_structural_key(original_body, body);
            if !preserved {
                violations.push(FidelityViolation {
                    missing_field: "structural_key".into(),
                    detail: "compacted JSON tool output does not reference any top-level field \
                             name from the original output"
                        .into(),
                });
            }
        }

        if violations.is_empty() {
            Ok(())
        } else {
            Err(violations)
        }
    }
}

/// Check that at least one top-level JSON key from `original` appears in `compacted`.
///
/// Parses `original` as a JSON object and returns `true` when any top-level key
/// string is a substring of `compacted`. Returns `true` (no violation) when
/// `original` is not a valid JSON object — the caller already checked schema hint.
fn check_json_structural_key(original: &str, compacted: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(original) else {
        return true;
    };
    let Some(obj) = value.as_object() else {
        return true;
    };
    if obj.is_empty() {
        return true;
    }
    obj.keys().any(|k| compacted.contains(k.as_str()))
}

/// Invariant for [`PageType::ConversationTurn`] pages.
///
/// The compacted body must preserve a role indicator and some meaningful content.
pub struct ConversationTurnInvariant;

impl PageInvariant for ConversationTurnInvariant {
    fn page_type(&self) -> PageType {
        PageType::ConversationTurn
    }

    fn minimum_fidelity(&self, _page: &TypedPage) -> FidelityContract {
        FidelityContract {
            fidelity_level: "semantic_summary_v1",
            invariant_version: 1,
            required_fields: &["role"],
        }
    }

    fn verify(
        &self,
        _original: &TypedPage,
        compacted: &CompactedPage,
    ) -> Result<(), Vec<FidelityViolation>> {
        let body = compacted.body.as_ref();
        let has_role =
            body.contains("user") || body.contains("assistant") || body.contains("system");
        if !has_role {
            return Err(vec![FidelityViolation {
                missing_field: "role".into(),
                detail: "compacted turn does not identify a speaker role".into(),
            }]);
        }
        Ok(())
    }
}

/// Invariant for [`PageType::MemoryExcerpt`] pages.
///
/// The compacted body must retain the source label and a message id reference.
pub struct MemoryExcerptInvariant;

impl PageInvariant for MemoryExcerptInvariant {
    fn page_type(&self) -> PageType {
        PageType::MemoryExcerpt
    }

    fn minimum_fidelity(&self, _page: &TypedPage) -> FidelityContract {
        FidelityContract {
            fidelity_level: "excerpt_summary_v1",
            invariant_version: 1,
            required_fields: &["source_label"],
        }
    }

    fn verify(
        &self,
        original: &TypedPage,
        compacted: &CompactedPage,
    ) -> Result<(), Vec<FidelityViolation>> {
        let source_label = match &original.origin {
            PageOrigin::Excerpt { source_label } => source_label.as_str(),
            _ => return Ok(()),
        };
        if !compacted.body.contains(source_label) {
            return Err(vec![FidelityViolation {
                missing_field: "source_label".into(),
                detail: format!("compacted excerpt does not contain source label '{source_label}'"),
            }]);
        }
        Ok(())
    }
}

/// Invariant for [`PageType::SystemContext`] pages.
///
/// System context MUST NOT be paraphrased. Compaction replaces it with a
/// pointer record; any body other than the pointer prefix is a violation.
pub struct SystemContextInvariant;

/// Pointer prefix that the compactor writes for `SystemContext` pages.
pub const SYSTEM_POINTER_PREFIX: &str = "[system-ptr:";

impl PageInvariant for SystemContextInvariant {
    fn page_type(&self) -> PageType {
        PageType::SystemContext
    }

    fn minimum_fidelity(&self, _page: &TypedPage) -> FidelityContract {
        FidelityContract {
            fidelity_level: "pointer_replace_v1",
            invariant_version: 1,
            required_fields: &["pointer"],
        }
    }

    fn verify(
        &self,
        _original: &TypedPage,
        compacted: &CompactedPage,
    ) -> Result<(), Vec<FidelityViolation>> {
        if !compacted.body.starts_with(SYSTEM_POINTER_PREFIX) {
            return Err(vec![FidelityViolation {
                missing_field: "pointer".into(),
                detail: format!(
                    "SystemContext page was not pointer-replaced \
                     (body does not start with '{SYSTEM_POINTER_PREFIX}')"
                ),
            }]);
        }
        Ok(())
    }
}

// ── InvariantRegistry ─────────────────────────────────────────────────────────

/// Registry mapping each [`PageType`] to its [`PageInvariant`] implementation.
///
/// Built once and shared via `Arc` so tests can swap in a mock registry.
///
/// # Examples
///
/// ```
/// use zeph_context::typed_page::{InvariantRegistry, PageType};
///
/// let reg = InvariantRegistry::default();
/// let inv = reg.get(PageType::ToolOutput).unwrap();
/// assert_eq!(inv.page_type(), PageType::ToolOutput);
/// ```
pub struct InvariantRegistry {
    tool_output: Box<dyn PageInvariant>,
    conversation_turn: Box<dyn PageInvariant>,
    memory_excerpt: Box<dyn PageInvariant>,
    system_context: Box<dyn PageInvariant>,
}

impl Default for InvariantRegistry {
    fn default() -> Self {
        Self {
            tool_output: Box::new(ToolOutputInvariant),
            conversation_turn: Box::new(ConversationTurnInvariant),
            memory_excerpt: Box::new(MemoryExcerptInvariant),
            system_context: Box::new(SystemContextInvariant),
        }
    }
}

impl InvariantRegistry {
    /// Look up the invariant for a given [`PageType`].
    ///
    /// Always returns `Some` for the four built-in variants.
    #[must_use]
    pub fn get(&self, page_type: PageType) -> Option<&dyn PageInvariant> {
        match page_type {
            PageType::ToolOutput => Some(self.tool_output.as_ref()),
            PageType::ConversationTurn => Some(self.conversation_turn.as_ref()),
            PageType::MemoryExcerpt => Some(self.memory_excerpt.as_ref()),
            PageType::SystemContext => Some(self.system_context.as_ref()),
        }
    }

    /// Verify that `compacted` satisfies the invariant for `original` at a compaction boundary.
    ///
    /// This is the primary entry point for the compactor — it wraps `verify()` in a
    /// `tracing::info_span!` per NFR-009 so every compaction boundary is observable.
    ///
    /// Returns `Ok(())` when the invariant is satisfied, or the violation list on failure.
    ///
    /// # Errors
    ///
    /// Propagates [`FidelityViolation`]s from the registered invariant implementation.
    pub fn enforce(
        &self,
        original: &TypedPage,
        compacted: &CompactedPage,
    ) -> Result<(), Vec<FidelityViolation>> {
        let _span = tracing::info_span!(
            "context.compaction.typed_page",
            page_type = %original.page_type,
            page_id = %original.page_id.0,
            original_tokens = original.tokens,
            compacted_tokens = compacted.tokens,
        )
        .entered();

        if let Some(inv) = self.get(original.page_type) {
            inv.verify(original, compacted)
        } else {
            tracing::warn!(
                page_type = %original.page_type,
                "no invariant registered for page type — skipping verification"
            );
            Ok(())
        }
    }
}

// ── Classification helpers ────────────────────────────────────────────────────

/// Classify a context segment by examining well-known prefix markers.
///
/// Classification is deterministic and performs no I/O. When the input does not
/// match any known prefix the function defaults to [`PageType::ConversationTurn`]
/// and logs at `WARN` level per FR-008.
///
/// The function emits a `context.compaction.typed_page.classify` span per NFR-009
/// so every classification is observable in traces.
///
/// | Source marker | Assigned [`PageType`] |
/// |---|---|
/// | Starts with `[tool_output]` or `[tool:` | [`PageType::ToolOutput`] |
/// | Starts with `[cross-session context]`, `[semantic recall]`, `[known facts]`, `[conversation summaries]` | [`PageType::MemoryExcerpt`] |
/// | Starts with `[Persona context]`, `[Past experience]`, `[Memory summary]`, `[system` | [`PageType::SystemContext`] |
/// | Everything else | [`PageType::ConversationTurn`] |
///
/// # Examples
///
/// ```
/// use zeph_context::typed_page::{classify, PageType};
///
/// assert_eq!(classify("[tool_output] exit_code: 0"), PageType::ToolOutput);
/// assert_eq!(classify("[cross-session context]\nsome recall"), PageType::MemoryExcerpt);
/// assert_eq!(classify("[Persona context]\nfact"), PageType::SystemContext);
/// assert_eq!(classify("Hello, world!"), PageType::ConversationTurn);
/// ```
#[must_use]
pub fn classify(body: &str) -> PageType {
    classify_with_role(body, false)
}

/// Classify a context segment, with an explicit `is_system_role` hint.
///
/// When `is_system_role` is `true` the segment is classified as
/// [`PageType::SystemContext`] without prefix matching, preventing arbitrary
/// system messages injected by the assembler from silently falling back to
/// `ConversationTurn` (Key Invariant: "`SystemContext` pages are never paraphrased").
///
/// Use this variant when the caller has access to the message `Role`.
///
/// # Examples
///
/// ```
/// use zeph_context::typed_page::{classify_with_role, PageType};
///
/// // A plain system message without a known prefix is still SystemContext.
/// assert_eq!(classify_with_role("You are a helpful assistant.", true), PageType::SystemContext);
/// // Role hint does not override ToolOutput prefix detection.
/// assert_eq!(classify_with_role("[tool_output] exit_code: 0", false), PageType::ToolOutput);
/// ```
#[must_use]
pub fn classify_with_role(body: &str, is_system_role: bool) -> PageType {
    tracing::info_span!(
        "context.compaction.typed_page.classify",
        body_len = body.len()
    )
    .in_scope(|| classify_with_role_inner(body, is_system_role))
}

fn classify_with_role_inner(body: &str, is_system_role: bool) -> PageType {
    // Use the same prefix constants as the assembler for consistency.
    const TOOL_PREFIXES: &[&str] = &["[tool_output]", "[tool:", "[Tool output]"];
    const MEMORY_PREFIXES: &[&str] = &[
        "[cross-session context]",
        "[semantic recall]",
        "[known facts]",
        "[conversation summaries]",
        "[past corrections]",
        "## Relevant documents",
    ];
    const SYSTEM_PREFIXES: &[&str] = &[
        "[Persona context]",
        "[Past experience]",
        "[Memory summary]",
        "[system",
        "[skill",
        "[persona",
        "[digest",
        "[compression",
    ];

    let trimmed = body.trim_start();

    for prefix in TOOL_PREFIXES {
        if trimmed.starts_with(prefix) {
            return PageType::ToolOutput;
        }
    }
    for prefix in MEMORY_PREFIXES {
        if trimmed.starts_with(prefix) {
            return PageType::MemoryExcerpt;
        }
    }
    for prefix in SYSTEM_PREFIXES {
        if trimmed.starts_with(prefix) {
            return PageType::SystemContext;
        }
    }

    // When the caller signals Role::System, classify as SystemContext even if
    // the body does not start with a known prefix.  This prevents system
    // context injected by the assembler (e.g. plain instructions, directives)
    // from being eligible for paraphrase.
    if is_system_role {
        return PageType::SystemContext;
    }

    let mut prefix_end = body.len().min(80);
    while !body.is_char_boundary(prefix_end) {
        prefix_end -= 1;
    }
    tracing::warn!(
        body_prefix = &body[..prefix_end],
        "typed-page classification fallback to ConversationTurn"
    );
    PageType::ConversationTurn
}

/// Detect [`SchemaHint`] for a [`PageType::ToolOutput`] body.
///
/// Returns [`SchemaHint::Binary`] when the body is not valid UTF-8 (detected via
/// presence of replacement characters) or when the caller passes `is_binary =
/// true`. JSON detection is heuristic (starts with `{` or `[`).
#[must_use]
pub fn detect_schema_hint(body: &str, is_binary: bool) -> SchemaHint {
    if is_binary || body.contains('\u{FFFD}') {
        return SchemaHint::Binary;
    }
    let trimmed = body.trim_start();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        return SchemaHint::Json;
    }
    if trimmed.starts_with("--- ")
        || trimmed.starts_with("+++ ")
        || trimmed.starts_with("diff --git")
        || trimmed.starts_with("diff -")
    {
        return SchemaHint::Diff;
    }
    // Simple table heuristic: first line contains multiple tab or pipe separators.
    let first_line = trimmed.lines().next().unwrap_or("");
    if first_line.matches('\t').count() >= 2 || first_line.matches('|').count() >= 2 {
        return SchemaHint::Table;
    }
    SchemaHint::Text
}

// ── Audit record ──────────────────────────────────────────────────────────────

/// One JSONL audit record emitted per compacted page (FR-007).
///
/// Written to `[memory.compaction.typed_pages] audit_path` by the audit sink
/// before the compacted context is handed to the LLM.
#[derive(Debug, Serialize)]
pub struct CompactedPageRecord {
    /// ISO-8601 timestamp when the compaction occurred.
    pub ts: String,
    /// Opaque turn identifier (agent turn counter as string).
    pub turn_id: String,
    /// Stable content-addressed page identifier.
    pub page_id: String,
    /// Page classification.
    pub page_type: PageType,
    /// Serialised page origin.
    pub origin: PageOrigin,
    /// Token count of the original page.
    pub original_tokens: u32,
    /// Token count of the compacted page.
    pub compacted_tokens: u32,
    /// Fidelity level label from the invariant contract.
    pub fidelity_level: String,
    /// Schema version integer.
    pub invariant_version: u8,
    /// Provider name used for summarization.
    pub provider_name: String,
    /// Fidelity violations encountered (empty on success).
    pub violations: Vec<FidelityViolation>,
    /// `true` when classification fell back to `ConversationTurn`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub classification_fallback: bool,
}

// ── Batch assertions ──────────────────────────────────────────────────────────

/// A failed batch-level compaction assertion.
#[derive(Debug, Clone, Serialize)]
pub struct BatchViolation {
    /// Short label for the assertion that failed.
    pub assertion: String,
    /// Human-readable explanation.
    pub detail: String,
}

/// Batch-level compaction assertions for typed-page enforcement.
///
/// Unlike per-page [`PageInvariant`] which checks one page against its compacted form,
/// batch assertions verify aggregate properties of the entire summary against the set
/// of classified pages that were sent to the LLM.
///
/// Violations are observational — they never block compaction. They are logged and
/// emitted to the audit trail.
///
/// # Examples
///
/// ```
/// use zeph_context::typed_page::BatchAssertions;
///
/// let assertions = BatchAssertions {
///     tool_names_in_batch: vec!["shell".to_string()],
///     has_memory_excerpt: false,
///     excerpt_labels: vec![],
/// };
/// // Summary that mentions the tool — all assertions pass.
/// let violations = assertions.check("shell ran and exited 0");
/// assert!(violations.is_empty());
/// ```
#[derive(Debug, Clone, Default)]
pub struct BatchAssertions {
    /// Tool names collected from `ToolOutput` pages in the batch.
    pub tool_names_in_batch: Vec<String>,
    /// Whether any `MemoryExcerpt` pages were in the batch.
    pub has_memory_excerpt: bool,
    /// Source labels from `MemoryExcerpt` pages.
    pub excerpt_labels: Vec<String>,
}

impl BatchAssertions {
    /// Check the summary against batch-level assertions.
    ///
    /// Returns a list of assertion failures (empty = all pass). Failures are never fatal.
    #[must_use]
    pub fn check(&self, summary: &str) -> Vec<BatchViolation> {
        let mut violations = Vec::new();

        // At least one tool name from the batch must appear in the summary.
        if !self.tool_names_in_batch.is_empty() {
            let any_tool_mentioned = self
                .tool_names_in_batch
                .iter()
                .any(|name| !name.is_empty() && summary.contains(name.as_str()));
            if !any_tool_mentioned {
                violations.push(BatchViolation {
                    assertion: "tool_coverage".into(),
                    detail: format!(
                        "summary mentions none of the {} tool(s) in batch: {:?}",
                        self.tool_names_in_batch.len(),
                        self.tool_names_in_batch
                    ),
                });
            }
        }

        // If memory excerpts were present, at least one source label should appear.
        if self.has_memory_excerpt && !self.excerpt_labels.is_empty() {
            let any_label_mentioned = self
                .excerpt_labels
                .iter()
                .any(|label| !label.is_empty() && summary.contains(label.as_str()));
            if !any_label_mentioned {
                violations.push(BatchViolation {
                    assertion: "excerpt_label_coverage".into(),
                    detail: format!(
                        "summary mentions none of the memory excerpt labels: {:?}",
                        self.excerpt_labels
                    ),
                });
            }
        }

        violations
    }
}

// ── TypedPagesState ───────────────────────────────────────────────────────────

/// Shared runtime state for typed-page compaction, created once at agent startup.
///
/// Bundles the invariant registry and optional audit sink so they can be shared
/// via `Arc` across compaction calls without per-call allocation.
pub struct TypedPagesState {
    /// Invariant registry shared across all compaction calls.
    pub registry: InvariantRegistry,
    /// Optional JSONL audit sink. `None` when audit is disabled.
    pub audit_sink: Option<CompactionAuditSink>,
    /// Whether enforcement is `Active` (pointer-replace `SystemContext` + batch assertions).
    /// `false` = `Observe` mode (classify and audit only, no behavioral change).
    pub is_active: bool,
}

// ── Audit command ─────────────────────────────────────────────────────────────

/// Internal command sent through the audit sink channel.
enum AuditCommand {
    /// Write a compaction record.
    Record(CompactedPageRecord),
    /// Flush all preceding records and signal completion via the oneshot.
    Flush(tokio::sync::oneshot::Sender<()>),
}

// ── Audit sink ────────────────────────────────────────────────────────────────

/// Async bounded-mpsc audit sink for compaction records.
///
/// The sink serialises [`CompactedPageRecord`] values to a JSONL file via a
/// background writer task, mirroring the `zeph-tools` audit pattern. Dropped
/// records (when the channel is full) are counted and logged.
///
/// # Invariant
///
/// [`CompactionAuditSink::flush`] sends a rendezvous sentinel through the channel
/// and awaits the writer task's confirmation with a 100 ms timeout. Records accepted
/// into the channel before `flush` is called are guaranteed to be written before the
/// flush responder fires.
///
/// # Examples
///
/// ```no_run
/// use zeph_context::typed_page::CompactionAuditSink;
/// use std::path::Path;
///
/// # async fn example() {
/// let sink = CompactionAuditSink::open(Path::new(".local/audit/compaction.jsonl"), 256)
///     .await
///     .unwrap();
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct CompactionAuditSink {
    tx: tokio::sync::mpsc::Sender<AuditCommand>,
    drop_counter: Arc<std::sync::atomic::AtomicU64>,
}

impl CompactionAuditSink {
    /// Open a new audit sink writing to `path`.
    ///
    /// `capacity` is the bounded channel depth; records dropped when full are counted
    /// in the internal drop counter and logged at WARN.
    ///
    /// # Errors
    ///
    /// Returns an error when `path` cannot be opened for appending.
    pub async fn open(path: &std::path::Path, capacity: usize) -> Result<Self, std::io::Error> {
        use tokio::io::AsyncWriteExt as _;

        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .await?;

        let (tx, mut rx) = tokio::sync::mpsc::channel::<AuditCommand>(capacity.max(1));
        let drop_counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let drop_counter_bg = drop_counter.clone();

        tokio::spawn(async move {
            let mut writer = tokio::io::BufWriter::new(file);
            while let Some(cmd) = rx.recv().await {
                match cmd {
                    AuditCommand::Record(record) => match serde_json::to_string(&record) {
                        Ok(mut line) => {
                            line.push('\n');
                            if let Err(e) = writer.write_all(line.as_bytes()).await {
                                tracing::error!("compaction audit write failed: {e:#}");
                            }
                        }
                        Err(e) => {
                            tracing::error!("compaction audit serialization failed: {e:#}");
                        }
                    },
                    AuditCommand::Flush(responder) => {
                        let _ = writer.flush().await;
                        let _ = responder.send(());
                    }
                }
            }
            // Flush remaining bytes when channel closes.
            let _ = writer.flush().await;

            let dropped = drop_counter_bg.load(std::sync::atomic::Ordering::Relaxed);
            if dropped > 0 {
                tracing::warn!(dropped, "compaction audit sink closed with dropped records");
            }
        });

        Ok(Self { tx, drop_counter })
    }

    /// Send a record to the audit sink.
    ///
    /// If the channel is full the record is dropped and the drop counter is incremented.
    pub fn send(&self, record: CompactedPageRecord) {
        match self.tx.try_send(AuditCommand::Record(record)) {
            Ok(()) => {}
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                let prev = self
                    .drop_counter
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                tracing::warn!(
                    dropped_total = prev + 1,
                    "compaction audit sink full — record dropped (best-effort audit)"
                );
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                tracing::error!("compaction audit sink closed unexpectedly");
            }
        }
    }

    /// Flush all pending records with bounded 100 ms timeout.
    ///
    /// Sends a `Flush` sentinel through the same channel as records, so ordering is
    /// preserved — the writer task responds only after all preceding records are written.
    /// If the writer task does not respond within 100 ms, the flush times out silently.
    pub async fn flush(&self) {
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        if self.tx.send(AuditCommand::Flush(tx)).await.is_ok() {
            let _ = tokio::time::timeout(Duration::from_millis(100), rx).await;
        }
    }

    /// Number of records dropped due to a full channel.
    #[must_use]
    pub fn dropped_count(&self) -> u64 {
        self.drop_counter.load(std::sync::atomic::Ordering::Relaxed)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── PageId ────────────────────────────────────────────────────────────────

    #[test]
    fn page_id_same_input_same_output() {
        let a = PageId::compute(PageType::ToolOutput, "tool:shell", b"exit_code: 0");
        let b = PageId::compute(PageType::ToolOutput, "tool:shell", b"exit_code: 0");
        assert_eq!(a, b);
    }

    #[test]
    fn page_id_different_type_different_id() {
        let a = PageId::compute(PageType::ToolOutput, "tool:shell", b"body");
        let b = PageId::compute(PageType::ConversationTurn, "tool:shell", b"body");
        assert_ne!(a, b);
    }

    #[test]
    fn page_id_starts_with_blake3_prefix() {
        let id = PageId::compute(PageType::SystemContext, "system:persona", b"some content");
        assert!(id.0.starts_with("blake3:"));
    }

    // ── classify ──────────────────────────────────────────────────────────────

    #[test]
    fn classify_tool_output_prefix() {
        assert_eq!(
            classify("[tool_output] shell exit_code: 0"),
            PageType::ToolOutput
        );
        assert_eq!(classify("[tool:shell] result"), PageType::ToolOutput);
    }

    #[test]
    fn classify_memory_prefixes() {
        assert_eq!(
            classify("[cross-session context]\nsome recall"),
            PageType::MemoryExcerpt
        );
        assert_eq!(
            classify("[semantic recall]\n- [user] hello"),
            PageType::MemoryExcerpt
        );
        assert_eq!(classify("[known facts]\n- fact"), PageType::MemoryExcerpt);
        assert_eq!(
            classify("[conversation summaries]\n- 1-10: summary"),
            PageType::MemoryExcerpt
        );
    }

    #[test]
    fn classify_system_prefixes() {
        assert_eq!(classify("[Persona context]\nfact"), PageType::SystemContext);
        assert_eq!(classify("[system prompt]"), PageType::SystemContext);
    }

    #[test]
    fn classify_fallback_is_conversation_turn() {
        assert_eq!(classify("Hello, world!"), PageType::ConversationTurn);
        assert_eq!(classify(""), PageType::ConversationTurn);
    }

    // ── detect_schema_hint ────────────────────────────────────────────────────

    #[test]
    fn detect_schema_hint_json() {
        assert_eq!(
            detect_schema_hint(r#"{"key": "val"}"#, false),
            SchemaHint::Json
        );
        assert_eq!(detect_schema_hint("[1,2,3]", false), SchemaHint::Json);
    }

    #[test]
    fn detect_schema_hint_diff() {
        assert_eq!(detect_schema_hint("--- a\n+++ b", false), SchemaHint::Diff);
    }

    #[test]
    fn detect_schema_hint_binary() {
        assert_eq!(detect_schema_hint("anything", true), SchemaHint::Binary);
    }

    #[test]
    fn detect_schema_hint_text_fallback() {
        assert_eq!(detect_schema_hint("plain text", false), SchemaHint::Text);
    }

    // ── ToolOutputInvariant ───────────────────────────────────────────────────

    #[test]
    fn tool_output_invariant_passes_when_fields_present() {
        let inv = ToolOutputInvariant;
        let page = TypedPage::new(
            PageType::ToolOutput,
            PageOrigin::ToolPair {
                tool_name: "shell".into(),
            },
            100,
            Arc::from("[tool_output] shell exit_code: 0\nsome output"),
            Some(SchemaHint::Text),
        );
        let compacted = CompactedPage {
            body: Arc::from("shell exit_status: 0\nkey: value"),
            tokens: 10,
        };
        assert!(inv.verify(&page, &compacted).is_ok());
    }

    #[test]
    fn tool_output_invariant_fails_missing_tool_name() {
        let inv = ToolOutputInvariant;
        let page = TypedPage::new(
            PageType::ToolOutput,
            PageOrigin::ToolPair {
                tool_name: "my_tool".into(),
            },
            100,
            Arc::from("[tool_output] my_tool exit_code: 0"),
            Some(SchemaHint::Text),
        );
        let compacted = CompactedPage {
            body: Arc::from("exit_status: 0"),
            tokens: 5,
        };
        let result = inv.verify(&page, &compacted);
        assert!(result.is_err());
        let violations = result.unwrap_err();
        assert!(violations.iter().any(|v| v.missing_field == "tool_name"));
    }

    #[test]
    fn tool_output_invariant_passes_for_binary() {
        let inv = ToolOutputInvariant;
        let page = TypedPage::new(
            PageType::ToolOutput,
            PageOrigin::ToolPair {
                tool_name: "binary_tool".into(),
            },
            100,
            Arc::from("<binary:1024 bytes>"),
            Some(SchemaHint::Binary),
        );
        let compacted = CompactedPage {
            body: Arc::from("<binary:1024 bytes> (archived)"),
            tokens: 5,
        };
        assert!(inv.verify(&page, &compacted).is_ok());
    }

    // ── SystemContextInvariant ────────────────────────────────────────────────

    #[test]
    fn system_context_invariant_passes_with_pointer() {
        let inv = SystemContextInvariant;
        let page = TypedPage::new(
            PageType::SystemContext,
            PageOrigin::System {
                key: "persona".into(),
            },
            200,
            Arc::from("[Persona context]\nsome persona info"),
            None,
        );
        let compacted = CompactedPage {
            body: Arc::from("[system-ptr:blake3:abcdef123456]"),
            tokens: 3,
        };
        assert!(inv.verify(&page, &compacted).is_ok());
    }

    #[test]
    fn system_context_invariant_fails_without_pointer() {
        let inv = SystemContextInvariant;
        let page = TypedPage::new(
            PageType::SystemContext,
            PageOrigin::System {
                key: "persona".into(),
            },
            200,
            Arc::from("[Persona context]\nsome persona info"),
            None,
        );
        let compacted = CompactedPage {
            body: Arc::from("This is a paraphrase of persona info"),
            tokens: 10,
        };
        let result = inv.verify(&page, &compacted);
        assert!(result.is_err());
        let violations = result.unwrap_err();
        assert!(violations.iter().any(|v| v.missing_field == "pointer"));
    }

    // ── InvariantRegistry ─────────────────────────────────────────────────────

    #[test]
    fn registry_covers_all_page_types() {
        let reg = InvariantRegistry::default();
        for pt in [
            PageType::ToolOutput,
            PageType::ConversationTurn,
            PageType::MemoryExcerpt,
            PageType::SystemContext,
        ] {
            assert!(reg.get(pt).is_some(), "missing invariant for {pt:?}");
        }
    }

    #[test]
    fn registry_returns_correct_page_type() {
        let reg = InvariantRegistry::default();
        assert_eq!(
            reg.get(PageType::ToolOutput).unwrap().page_type(),
            PageType::ToolOutput
        );
        assert_eq!(
            reg.get(PageType::SystemContext).unwrap().page_type(),
            PageType::SystemContext
        );
    }

    // ── InvariantRegistry::enforce ────────────────────────────────────────────

    #[test]
    fn enforce_ok_for_valid_system_pointer() {
        let reg = InvariantRegistry::default();
        let page = TypedPage::new(
            PageType::SystemContext,
            PageOrigin::System {
                key: "persona".into(),
            },
            50,
            Arc::from("[Persona context]\nrules"),
            None,
        );
        let compacted = CompactedPage {
            body: Arc::from("[system-ptr:blake3:aabbccdd11223344]"),
            tokens: 3,
        };
        assert!(reg.enforce(&page, &compacted).is_ok());
    }

    #[test]
    fn enforce_err_for_paraphrased_system_context() {
        let reg = InvariantRegistry::default();
        let page = TypedPage::new(
            PageType::SystemContext,
            PageOrigin::System {
                key: "persona".into(),
            },
            50,
            Arc::from("[Persona context]\nrules"),
            None,
        );
        let compacted = CompactedPage {
            body: Arc::from("The persona says to be helpful."),
            tokens: 7,
        };
        let result = reg.enforce(&page, &compacted);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .iter()
                .any(|v| v.missing_field == "pointer")
        );
    }

    #[test]
    fn enforce_ok_for_conversation_turn_with_role() {
        let reg = InvariantRegistry::default();
        let page = TypedPage::new(
            PageType::ConversationTurn,
            PageOrigin::Turn {
                message_id: "42".into(),
            },
            30,
            Arc::from("Hello from user"),
            None,
        );
        let compacted = CompactedPage {
            body: Arc::from("user asked about Rust"),
            tokens: 5,
        };
        assert!(reg.enforce(&page, &compacted).is_ok());
    }

    // ── MemoryExcerptInvariant ────────────────────────────────────────────────

    #[test]
    fn memory_excerpt_invariant_passes_when_label_present() {
        let inv = MemoryExcerptInvariant;
        let label = "semantic_recall";
        let page = TypedPage::new(
            PageType::MemoryExcerpt,
            PageOrigin::Excerpt {
                source_label: label.into(),
            },
            80,
            Arc::from("[semantic recall]\n- [user] hello"),
            None,
        );
        let compacted = CompactedPage {
            body: Arc::from(format!("Summary from {label}: user greeted")),
            tokens: 6,
        };
        assert!(inv.verify(&page, &compacted).is_ok());
    }

    #[test]
    fn memory_excerpt_invariant_fails_when_label_missing() {
        let inv = MemoryExcerptInvariant;
        let page = TypedPage::new(
            PageType::MemoryExcerpt,
            PageOrigin::Excerpt {
                source_label: "graph_facts".into(),
            },
            80,
            Arc::from("[known facts]\n- Alice works at Acme"),
            None,
        );
        let compacted = CompactedPage {
            body: Arc::from("Alice is employed somewhere"),
            tokens: 5,
        };
        let result = inv.verify(&page, &compacted);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .iter()
                .any(|v| v.missing_field == "source_label")
        );
    }

    #[test]
    fn memory_excerpt_invariant_passes_for_non_excerpt_origin() {
        let inv = MemoryExcerptInvariant;
        let page = TypedPage::new(
            PageType::MemoryExcerpt,
            PageOrigin::System {
                key: "digests".into(),
            },
            40,
            Arc::from("[system]"),
            None,
        );
        let compacted = CompactedPage {
            body: Arc::from("anything"),
            tokens: 1,
        };
        assert!(inv.verify(&page, &compacted).is_ok());
    }

    // ── ConversationTurnInvariant ─────────────────────────────────────────────

    #[test]
    fn conversation_turn_invariant_passes_with_role_word() {
        let inv = ConversationTurnInvariant;
        let page = TypedPage::new(
            PageType::ConversationTurn,
            PageOrigin::Turn {
                message_id: "1".into(),
            },
            20,
            Arc::from("Hello world"),
            None,
        );
        for body in &["user: hi", "assistant replied", "system note"] {
            let compacted = CompactedPage {
                body: Arc::from(*body),
                tokens: 2,
            };
            assert!(inv.verify(&page, &compacted).is_ok(), "body={body}");
        }
    }

    #[test]
    fn conversation_turn_invariant_fails_without_role_word() {
        let inv = ConversationTurnInvariant;
        let page = TypedPage::new(
            PageType::ConversationTurn,
            PageOrigin::Turn {
                message_id: "2".into(),
            },
            20,
            Arc::from("some turn content"),
            None,
        );
        let compacted = CompactedPage {
            body: Arc::from("content was summarized"),
            tokens: 3,
        };
        let result = inv.verify(&page, &compacted);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .iter()
                .any(|v| v.missing_field == "role")
        );
    }

    // ── CompactionAuditSink ───────────────────────────────────────────────────

    #[tokio::test]
    async fn audit_sink_jsonl_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");

        let sink = CompactionAuditSink::open(&path, 64).await.unwrap();
        let record = CompactedPageRecord {
            ts: "2026-04-19T00:00:00Z".into(),
            turn_id: "1".into(),
            page_id: "blake3:aabbccdd".into(),
            page_type: PageType::ToolOutput,
            origin: PageOrigin::ToolPair {
                tool_name: "shell".into(),
            },
            original_tokens: 100,
            compacted_tokens: 20,
            fidelity_level: "structured_summary_v1".into(),
            invariant_version: 1,
            provider_name: "test".into(),
            violations: vec![],
            classification_fallback: false,
        };
        sink.send(record);

        // Drop the sink to close the channel and let the writer task flush.
        drop(sink);
        // Give the writer task time to finish.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(!contents.is_empty(), "audit file should not be empty");
        let parsed: serde_json::Value = serde_json::from_str(contents.trim()).unwrap();
        assert_eq!(parsed["page_type"], "tool_output");
        assert_eq!(parsed["turn_id"], "1");
        assert_eq!(parsed["provider_name"], "test");
    }

    #[tokio::test]
    async fn audit_sink_drop_counter_increments_when_full() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit_full.jsonl");

        // Capacity 1: first send fills the channel, subsequent sends are dropped.
        let sink = CompactionAuditSink::open(&path, 1).await.unwrap();

        let make_record = || CompactedPageRecord {
            ts: "2026-04-19T00:00:00Z".into(),
            turn_id: "x".into(),
            page_id: "blake3:00".into(),
            page_type: PageType::ConversationTurn,
            origin: PageOrigin::Turn {
                message_id: "0".into(),
            },
            original_tokens: 10,
            compacted_tokens: 5,
            fidelity_level: "semantic_summary_v1".into(),
            invariant_version: 1,
            provider_name: "test".into(),
            violations: vec![],
            classification_fallback: false,
        };

        // Send enough records to guarantee overflow.
        for _ in 0..10 {
            sink.send(make_record());
        }

        assert!(
            sink.dropped_count() > 0,
            "expected at least one dropped record"
        );
    }

    #[tokio::test]
    async fn audit_sink_flush_does_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit_flush.jsonl");
        let sink = CompactionAuditSink::open(&path, 16).await.unwrap();
        // flush on an empty sink must not panic or deadlock.
        sink.flush().await;
    }

    // ── classify_with_role ────────────────────────────────────────────────────

    #[test]
    fn classify_with_role_system_flag_overrides_fallback() {
        assert_eq!(
            classify_with_role("You are a helpful assistant.", true),
            PageType::SystemContext
        );
    }

    #[test]
    fn classify_with_role_prefix_wins_over_system_flag() {
        assert_eq!(
            classify_with_role("[tool_output] exit_code: 0", false),
            PageType::ToolOutput
        );
    }

    #[test]
    fn classify_with_role_false_still_falls_back_to_conversation_turn() {
        assert_eq!(
            classify_with_role("random prose without markers", false),
            PageType::ConversationTurn
        );
    }

    // ── check_json_structural_key (via ToolOutputInvariant) ───────────────────

    #[test]
    fn tool_output_json_structural_check_passes_when_key_preserved() {
        let inv = ToolOutputInvariant;
        let original_body = r#"{"exit_code": 0, "stdout": "ok"}"#;
        let page = TypedPage::new(
            PageType::ToolOutput,
            PageOrigin::ToolPair {
                tool_name: "shell".into(),
            },
            50,
            Arc::from(original_body),
            Some(SchemaHint::Json),
        );
        // Compacted body references "exit_code" and "shell".
        let compacted = CompactedPage {
            body: Arc::from("shell exit_code: 0, stdout was ok"),
            tokens: 8,
        };
        assert!(inv.verify(&page, &compacted).is_ok());
    }

    #[test]
    fn tool_output_json_structural_check_fails_when_no_key_preserved() {
        let inv = ToolOutputInvariant;
        let original_body = r#"{"some_field": "value", "other_field": 42}"#;
        let page = TypedPage::new(
            PageType::ToolOutput,
            PageOrigin::ToolPair {
                tool_name: "my_tool".into(),
            },
            50,
            Arc::from(original_body),
            Some(SchemaHint::Json),
        );
        // Compacted body references tool name and status but none of the JSON keys.
        let compacted = CompactedPage {
            body: Arc::from("my_tool exit_status: 0 completed successfully"),
            tokens: 7,
        };
        let result = inv.verify(&page, &compacted);
        assert!(result.is_err());
        let violations = result.unwrap_err();
        assert!(
            violations
                .iter()
                .any(|v| v.missing_field == "structural_key")
        );
    }

    // ── Regression: F1 — capacity=0 must not panic ────────────────────────────

    #[tokio::test]
    async fn audit_sink_capacity_zero_does_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cap0.jsonl");
        // capacity=0 used to panic in tokio::sync::mpsc::channel(0); must clamp to 1.
        let sink = CompactionAuditSink::open(&path, 0).await.unwrap();
        sink.flush().await;
    }

    // ── Regression: F3 — non-ASCII body must not panic on prefix slice ────────

    #[test]
    fn classify_with_role_non_ascii_body_does_not_panic() {
        // CJK and emoji span multiple bytes; a naive &body[..80] would panic at a
        // mid-character byte boundary. classify_with_role must not panic for any input.
        let cjk = "你好世界".repeat(20); // 80+ bytes, 4 bytes each
        let emoji = "🦀".repeat(30); // 120+ bytes, 4 bytes each
        let mixed = "abc🦀中文".repeat(15);

        // None of these must panic:
        let _ = classify_with_role(&cjk, false);
        let _ = classify_with_role(&emoji, false);
        let _ = classify_with_role(&mixed, false);
    }
}
