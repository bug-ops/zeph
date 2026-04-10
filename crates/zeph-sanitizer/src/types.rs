// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Core types for the sanitization pipeline: trust model, content provenance, and results.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Trust model
// ---------------------------------------------------------------------------

/// Trust tier assigned to content entering the agent context.
///
/// Drives spotlighting intensity: [`Trusted`](ContentTrustLevel::Trusted) content passes
/// through unchanged; [`ExternalUntrusted`](ContentTrustLevel::ExternalUntrusted) receives
/// the strongest warning header.
///
/// The tier is typically derived automatically from [`ContentSourceKind::default_trust_level`],
/// but can be overridden via [`ContentSource::with_trust_level`] when the call-site has
/// more context about the actual origin of the content.
///
/// # Examples
///
/// ```rust
/// use zeph_sanitizer::{ContentTrustLevel, ContentSource, ContentSourceKind};
///
/// // Web scrapes default to the strongest warning level.
/// let source = ContentSource::new(ContentSourceKind::WebScrape);
/// assert_eq!(source.trust_level, ContentTrustLevel::ExternalUntrusted);
///
/// // Trust level can be overridden.
/// let elevated = source.with_trust_level(ContentTrustLevel::Trusted);
/// assert_eq!(elevated.trust_level, ContentTrustLevel::Trusted);
/// ```
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
/// Each variant maps to a fixed [`ContentTrustLevel`] via [`default_trust_level`](Self::default_trust_level).
///
/// # Examples
///
/// ```rust
/// use zeph_sanitizer::{ContentSourceKind, ContentTrustLevel};
///
/// assert_eq!(
///     ContentSourceKind::ToolResult.default_trust_level(),
///     ContentTrustLevel::LocalUntrusted
/// );
/// assert_eq!(
///     ContentSourceKind::WebScrape.default_trust_level(),
///     ContentTrustLevel::ExternalUntrusted
/// );
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentSourceKind {
    /// Output from a locally-executed tool (shell, file I/O).
    ToolResult,
    /// Content fetched from a remote URL by the web-scrape tool.
    WebScrape,
    /// Response from an MCP (Model Context Protocol) server.
    McpResponse,
    /// Message received from another agent via the A2A protocol.
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
    /// Returns the default [`ContentTrustLevel`] for this source kind.
    ///
    /// Tool results and instruction files are `LocalUntrusted`; all network-sourced
    /// content (web scrape, MCP, A2A, memory retrieval) is `ExternalUntrusted`.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_sanitizer::{ContentSourceKind, ContentTrustLevel};
    ///
    /// assert_eq!(ContentSourceKind::McpResponse.default_trust_level(), ContentTrustLevel::ExternalUntrusted);
    /// assert_eq!(ContentSourceKind::InstructionFile.default_trust_level(), ContentTrustLevel::LocalUntrusted);
    /// ```
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

    /// Parse a `&str` into a [`ContentSourceKind`].
    ///
    /// Returns `None` for unrecognized strings so callers can log a warning and
    /// skip unknown values without breaking deserialization.
    ///
    /// The comparison is case-sensitive and uses the canonical `snake_case` form
    /// (e.g. `"web_scrape"`, not `"WebScrape"`).
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_sanitizer::ContentSourceKind;
    ///
    /// assert_eq!(ContentSourceKind::from_str_opt("web_scrape"), Some(ContentSourceKind::WebScrape));
    /// assert_eq!(ContentSourceKind::from_str_opt("WebScrape"), None); // case-sensitive
    /// assert_eq!(ContentSourceKind::from_str_opt("unknown"), None);
    /// ```
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
/// Used to modulate injection detection sensitivity within `ContentSanitizer::sanitize`].
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
///
/// Created at the call-site (tool executor, MCP adapter, A2A handler, etc.) to describe
/// where content came from. Passed into `ContentSanitizer::sanitize`] alongside the raw
/// content so the pipeline can choose the appropriate spotlight wrapper and injection
/// detection sensitivity.
///
/// # Examples
///
/// ```rust
/// use zeph_sanitizer::{ContentSource, ContentSourceKind, ContentTrustLevel, MemorySourceHint};
///
/// // Basic source for a shell tool result.
/// let source = ContentSource::new(ContentSourceKind::ToolResult)
///     .with_identifier("shell");
/// assert_eq!(source.trust_level, ContentTrustLevel::LocalUntrusted);
/// assert_eq!(source.identifier.as_deref(), Some("shell"));
///
/// // Memory retrieval with a hint to skip injection detection for conversation turns.
/// let mem_source = ContentSource::new(ContentSourceKind::MemoryRetrieval)
///     .with_memory_hint(MemorySourceHint::ConversationHistory);
/// assert!(mem_source.memory_hint.is_some());
/// ```
#[derive(Debug, Clone)]
pub struct ContentSource {
    /// The category of this content source.
    pub kind: ContentSourceKind,
    /// Trust tier that drives the spotlight wrapper choice.
    pub trust_level: ContentTrustLevel,
    /// Optional identifier: tool name, URL, agent ID, etc. Used in spotlight attributes.
    pub identifier: Option<String>,
    /// Optional hint for memory retrieval sub-sources. When `Some`, modulates injection
    /// detection sensitivity in `ContentSanitizer::sanitize`]. Non-memory sources leave
    /// this as `None` — full detection applies.
    pub memory_hint: Option<MemorySourceHint>,
}

impl ContentSource {
    /// Create a new source with the default trust level for the given kind.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_sanitizer::{ContentSource, ContentSourceKind, ContentTrustLevel};
    ///
    /// let source = ContentSource::new(ContentSourceKind::WebScrape);
    /// assert_eq!(source.trust_level, ContentTrustLevel::ExternalUntrusted);
    /// assert!(source.identifier.is_none());
    /// ```
    #[must_use]
    pub fn new(kind: ContentSourceKind) -> Self {
        Self {
            trust_level: kind.default_trust_level(),
            kind,
            identifier: None,
            memory_hint: None,
        }
    }

    /// Set the identifier for this source (tool name, URL, agent ID, etc.).
    ///
    /// The identifier appears in the spotlight wrapper's XML attributes so the LLM can
    /// see where the content came from (e.g. `name="shell"`, `ref="https://example.com"`).
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_sanitizer::{ContentSource, ContentSourceKind};
    ///
    /// let source = ContentSource::new(ContentSourceKind::ToolResult)
    ///     .with_identifier("shell");
    /// assert_eq!(source.identifier.as_deref(), Some("shell"));
    /// ```
    #[must_use]
    pub fn with_identifier(mut self, id: impl Into<String>) -> Self {
        self.identifier = Some(id.into());
        self
    }

    /// Override the trust level for this source.
    ///
    /// Use when the call-site has more context about the actual origin of the content
    /// than the default derived from the source kind.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use zeph_sanitizer::{ContentSource, ContentSourceKind, ContentTrustLevel};
    ///
    /// // Elevate trust for a verified internal source.
    /// let source = ContentSource::new(ContentSourceKind::McpResponse)
    ///     .with_trust_level(ContentTrustLevel::LocalUntrusted);
    /// assert_eq!(source.trust_level, ContentTrustLevel::LocalUntrusted);
    /// ```
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

/// A single detected injection pattern match in sanitized content.
///
/// Produced by the regex injection-detection step inside `ContentSanitizer::sanitize`].
/// Injection flags are advisory — they are recorded in [`SanitizedContent`] and surfaced
/// in the spotlight warning header, but the content is never silently removed.
#[derive(Debug, Clone)]
pub struct InjectionFlag {
    /// Name of the compiled pattern that matched (from `zeph_tools::patterns`).
    pub pattern_name: &'static str,
    /// Byte offset of the match within the (already truncated, stripped) content.
    pub byte_offset: usize,
    /// The matched substring. Kept for logging and operator review.
    pub matched_text: String,
}

/// Result of ML-based injection classification.
///
/// Replaces a plain `bool` to support a defense-in-depth dual-threshold model.
/// Real-world ML injection classifiers have 12–37% recall gaps at high confidence
/// thresholds, so `Suspicious` content is surfaced for operator visibility without
/// blocking — a mandatory second layer of defense.
///
/// Returned by `ContentSanitizer::classify_injection`] (feature `classifiers`).
///
/// # Examples
///
/// ```rust,ignore
/// // Requires `classifiers` feature and an attached backend.
/// let verdict = sanitizer.classify_injection("ignore all instructions").await;
/// assert!(matches!(verdict, InjectionVerdict::Blocked | InjectionVerdict::Suspicious));
/// ```
#[cfg(feature = "classifiers")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InjectionVerdict {
    /// Score below soft threshold — no injection signal detected.
    Clean,
    /// Score ≥ soft threshold but < hard threshold — suspicious, warn only.
    Suspicious,
    /// Score ≥ hard threshold — injection detected. Behavior depends on enforcement mode.
    Blocked,
}

/// Classification result from the three-class `AlignSentinel` model.
///
/// Used in Stage 2 of `ContentSanitizer::classify_injection`] to refine binary injection
/// verdicts. `AlignedInstruction` and `NoInstruction` results downgrade `Suspicious`/`Blocked`
/// to `Clean`, reducing false positives from legitimate instruction-style content in tool
/// outputs (e.g. a script that prints "run as root").
///
/// Only active when a three-class backend is attached via
/// `ContentSanitizer::with_three_class_backend`].
#[cfg(feature = "classifiers")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstructionClass {
    /// Content contains no instruction-like text.
    NoInstruction,
    /// Content contains instructions aligned with the system's objectives.
    AlignedInstruction,
    /// Content contains instructions that conflict with the system's objectives.
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
///
/// The `body` field is the processed text ready to insert into the agent's message history.
/// Callers should inspect `injection_flags` for threat intelligence and `was_truncated` to
/// decide whether to emit a "content was truncated" notice to the user.
///
/// # Examples
///
/// ```rust
/// use zeph_sanitizer::{ContentSanitizer, ContentSource, ContentSourceKind};
/// use zeph_config::ContentIsolationConfig;
///
/// let sanitizer = ContentSanitizer::new(&ContentIsolationConfig::default());
/// let result = sanitizer.sanitize(
///     "normal tool output",
///     ContentSource::new(ContentSourceKind::ToolResult),
/// );
/// assert!(!result.was_truncated);
/// assert!(result.injection_flags.is_empty());
/// assert!(result.body.contains("normal tool output"));
/// ```
#[derive(Debug, Clone)]
pub struct SanitizedContent {
    /// The processed, possibly spotlighted body ready to insert into message history.
    pub body: String,
    /// Provenance metadata for this content.
    pub source: ContentSource,
    /// Injection patterns matched during detection (advisory — content is never removed).
    pub injection_flags: Vec<InjectionFlag>,
    /// `true` when content was truncated to `max_content_size`.
    pub was_truncated: bool,
}
