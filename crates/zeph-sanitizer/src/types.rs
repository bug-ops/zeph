// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Trust model
// ---------------------------------------------------------------------------

/// Trust tier assigned to content entering the agent context.
///
/// Drives spotlighting intensity: [`Trusted`](ContentTrustLevel::Trusted) content passes
/// through unchanged; [`ExternalUntrusted`](ContentTrustLevel::ExternalUntrusted) receives
/// the strongest warning header.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentTrustLevel {
    /// System prompt, hardcoded instructions, direct user input. No wrapping applied.
    Trusted,
    /// Tool results from local executors (shell, file I/O). Lighter warning.
    LocalUntrusted,
    /// External sources: web scrape, MCP, A2A, memory retrieval. Strongest warning.
    ExternalUntrusted,
}

/// All known content source categories.
///
/// Used for spotlighting annotation and future per-source config overrides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentSourceKind {
    ToolResult,
    WebScrape,
    McpResponse,
    A2aMessage,
    /// Content retrieved from Qdrant/SQLite semantic memory.
    ///
    /// Memory poisoning is a documented attack vector: an adversary can plant injection
    /// payloads in web content that gets stored, then recalled in future sessions.
    MemoryRetrieval,
    /// Project-level instruction files (`.zeph/zeph.md`, CLAUDE.md, etc.).
    ///
    /// Treated as `LocalUntrusted` by default. Path-based trust inference (e.g. treating
    /// user-authored files as `Trusted`) is a Phase 2 concern.
    InstructionFile,
}

impl ContentSourceKind {
    /// Returns the default trust level for this source kind.
    #[must_use]
    pub fn default_trust_level(self) -> ContentTrustLevel {
        match self {
            Self::ToolResult | Self::InstructionFile => ContentTrustLevel::LocalUntrusted,
            Self::WebScrape | Self::McpResponse | Self::A2aMessage | Self::MemoryRetrieval => {
                ContentTrustLevel::ExternalUntrusted
            }
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::ToolResult => "tool_result",
            Self::WebScrape => "web_scrape",
            Self::McpResponse => "mcp_response",
            Self::A2aMessage => "a2a_message",
            Self::MemoryRetrieval => "memory_retrieval",
            Self::InstructionFile => "instruction_file",
        }
    }

    /// Parse a string into a `ContentSourceKind`.
    ///
    /// Returns `None` for unrecognized strings (instead of an error) so callers
    /// can log a warning and skip unknown values without breaking deserialization.
    #[must_use]
    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s {
            "tool_result" => Some(Self::ToolResult),
            "web_scrape" => Some(Self::WebScrape),
            "mcp_response" => Some(Self::McpResponse),
            "a2a_message" => Some(Self::A2aMessage),
            "memory_retrieval" => Some(Self::MemoryRetrieval),
            "instruction_file" => Some(Self::InstructionFile),
            _ => None,
        }
    }
}

/// Hint about the origin of memory-retrieved content.
///
/// Used to modulate injection detection sensitivity within [`ContentSanitizer::sanitize`].
/// The hint is set at call-site (compile-time) based on which retrieval path produced the
/// content — it cannot be influenced by the content itself and thus cannot be spoofed.
///
/// # Defense-in-depth invariant
///
/// Setting a hint to [`ConversationHistory`](MemorySourceHint::ConversationHistory) or
/// [`LlmSummary`](MemorySourceHint::LlmSummary) **only** skips injection pattern detection
/// (step 3). Truncation, control-character stripping, delimiter escaping, and spotlighting
/// remain active for all sources regardless of this hint.
///
/// # Known limitation: indirect memory poisoning
///
/// Conversation history is treated as first-party (user-typed) content. However, the LLM
/// may call `memory_save` with content derived from a prior injection in external sources
/// (web scrape → spotlighted → LLM stores payload → recalled as `[assistant]` turn).
/// Mitigate by configuring `forbidden_content_patterns` in `[memory.validation]` to block
/// known injection strings on the write path. This risk is pre-existing and is not worsened
/// by the hint mechanism.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemorySourceHint {
    /// Prior user/assistant conversation turns (semantic recall, corrections).
    ///
    /// Injection patterns in recalled user text are expected false positives — the user
    /// legitimately discussed topics like "system prompt" or "show your instructions".
    ConversationHistory,
    /// LLM-generated summaries (session summaries, cross-session context).
    ///
    /// Low risk: generated by the agent's own model from already-sanitized content.
    LlmSummary,
    /// External document chunks or graph entity facts.
    ///
    /// Full detection applies — may contain adversarial content from web scrapes,
    /// MCP responses, or other untrusted sources that were stored in the corpus.
    ExternalContent,
}

/// Provenance metadata attached to a piece of untrusted content.
#[derive(Debug, Clone)]
pub struct ContentSource {
    pub kind: ContentSourceKind,
    pub trust_level: ContentTrustLevel,
    /// Optional identifier: tool name, URL, agent ID, etc.
    pub identifier: Option<String>,
    /// Optional hint for memory retrieval sub-sources. When `Some`, modulates injection
    /// detection sensitivity in [`ContentSanitizer::sanitize`]. Non-memory sources leave
    /// this as `None` — full detection applies.
    pub memory_hint: Option<MemorySourceHint>,
}

impl ContentSource {
    #[must_use]
    pub fn new(kind: ContentSourceKind) -> Self {
        Self {
            trust_level: kind.default_trust_level(),
            kind,
            identifier: None,
            memory_hint: None,
        }
    }

    #[must_use]
    pub fn with_identifier(mut self, id: impl Into<String>) -> Self {
        self.identifier = Some(id.into());
        self
    }

    #[must_use]
    pub fn with_trust_level(mut self, level: ContentTrustLevel) -> Self {
        self.trust_level = level;
        self
    }

    /// Attach a memory source hint to modulate injection detection sensitivity.
    ///
    /// Only meaningful for `ContentSourceKind::MemoryRetrieval` sources.
    #[must_use]
    pub fn with_memory_hint(mut self, hint: MemorySourceHint) -> Self {
        self.memory_hint = Some(hint);
        self
    }
}

// ---------------------------------------------------------------------------
// Output types
// ---------------------------------------------------------------------------

/// A single detected injection pattern match.
#[derive(Debug, Clone)]
pub struct InjectionFlag {
    pub pattern_name: &'static str,
    /// Byte offset of the match within the (already truncated, stripped) content.
    pub byte_offset: usize,
    pub matched_text: String,
}

/// Result of ML-based injection classification.
///
/// Replaces the previous `bool` return type of `classify_injection` to support
/// a defense-in-depth dual-threshold model. Real-world ML injection classifiers
/// have 12–37% recall gaps at high confidence thresholds, so `Suspicious` content
/// is surfaced for operator visibility without blocking — a mandatory second layer.
#[cfg(feature = "classifiers")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InjectionVerdict {
    /// Score below soft threshold — no injection signal detected.
    Clean,
    /// Score ≥ soft threshold but < hard threshold — suspicious, warn only.
    Suspicious,
    /// Score ≥ hard threshold — injection detected, block.
    Blocked,
}

/// Classification result from the three-class `AlignSentinel` model.
///
/// Used to refine binary injection verdicts: `AlignedInstruction` and `NoInstruction`
/// results downgrade `Suspicious`/`Blocked` to `Clean`, reducing false positives from
/// legitimate instruction-style content in tool outputs.
#[cfg(feature = "classifiers")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstructionClass {
    NoInstruction,
    AlignedInstruction,
    MisalignedInstruction,
    /// Model returned an unknown label. Treated conservatively — verdict is NOT downgraded.
    Unknown,
}

#[cfg(feature = "classifiers")]
impl InstructionClass {
    pub(crate) fn from_label(label: &str) -> Self {
        match label.to_lowercase().as_str() {
            "no_instruction" | "no-instruction" | "none" => Self::NoInstruction,
            "aligned_instruction" | "aligned-instruction" | "aligned" => Self::AlignedInstruction,
            "misaligned_instruction" | "misaligned-instruction" | "misaligned" => {
                Self::MisalignedInstruction
            }
            _ => Self::Unknown,
        }
    }
}

/// Result of the sanitization pipeline for a single piece of content.
#[derive(Debug, Clone)]
pub struct SanitizedContent {
    /// The processed, possibly spotlighted body to insert into message history.
    pub body: String,
    pub source: ContentSource,
    pub injection_flags: Vec<InjectionFlag>,
    /// `true` when content was truncated to `max_content_size`.
    pub was_truncated: bool,
}
