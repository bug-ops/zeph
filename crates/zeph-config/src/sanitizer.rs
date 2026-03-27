// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use serde::{Deserialize, Serialize};

use crate::defaults::default_true;

// ---------------------------------------------------------------------------
// ContentIsolationConfig
// ---------------------------------------------------------------------------

fn default_max_content_size() -> usize {
    65_536
}

/// Configuration for the embedding anomaly guard, nested under
/// `[security.content_isolation.embedding_guard]`.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct EmbeddingGuardConfig {
    /// Enable embedding-based anomaly detection (default: false — opt-in).
    #[serde(default)]
    pub enabled: bool,
    /// Cosine distance threshold above which outputs are flagged as anomalous.
    #[serde(default = "default_embedding_threshold")]
    pub threshold: f64,
    /// Minimum clean samples before centroid-based detection activates.
    /// Before this count, regex fallback is used instead.
    #[serde(default = "default_embedding_min_samples")]
    pub min_samples: usize,
}

fn default_embedding_threshold() -> f64 {
    0.35
}

fn default_embedding_min_samples() -> usize {
    10
}

impl Default for EmbeddingGuardConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            threshold: default_embedding_threshold(),
            min_samples: default_embedding_min_samples(),
        }
    }
}

/// Configuration for the content isolation pipeline, nested under
/// `[security.content_isolation]` in the agent config file.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct ContentIsolationConfig {
    /// When `false`, the sanitizer is a no-op: content passes through unchanged.
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Maximum byte length of untrusted content before truncation.
    #[serde(default = "default_max_content_size")]
    pub max_content_size: usize,

    /// When `true`, injection patterns detected in content are recorded as
    /// flags and a warning is prepended to the spotlighting wrapper.
    #[serde(default = "default_true")]
    pub flag_injection_patterns: bool,

    /// When `true`, untrusted content is wrapped in spotlighting XML delimiters
    /// that instruct the LLM to treat the enclosed text as data, not instructions.
    #[serde(default = "default_true")]
    pub spotlight_untrusted: bool,

    /// Quarantine summarizer configuration.
    #[serde(default)]
    pub quarantine: QuarantineConfig,

    /// Embedding anomaly guard configuration.
    #[serde(default)]
    pub embedding_guard: EmbeddingGuardConfig,
}

impl Default for ContentIsolationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_content_size: default_max_content_size(),
            flag_injection_patterns: true,
            spotlight_untrusted: true,
            quarantine: QuarantineConfig::default(),
            embedding_guard: EmbeddingGuardConfig::default(),
        }
    }
}

/// Configuration for the quarantine summarizer, nested under
/// `[security.content_isolation.quarantine]` in the agent config file.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct QuarantineConfig {
    /// When `false`, quarantine summarization is disabled entirely.
    #[serde(default)]
    pub enabled: bool,

    /// Source kinds to route through the quarantine LLM.
    #[serde(default = "default_quarantine_sources")]
    pub sources: Vec<String>,

    /// Provider name passed to `create_named_provider`.
    #[serde(default = "default_quarantine_model")]
    pub model: String,
}

fn default_quarantine_sources() -> Vec<String> {
    vec!["web_scrape".to_owned(), "a2a_message".to_owned()]
}

fn default_quarantine_model() -> String {
    "claude".to_owned()
}

impl Default for QuarantineConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            sources: default_quarantine_sources(),
            model: default_quarantine_model(),
        }
    }
}

// ---------------------------------------------------------------------------
// ExfiltrationGuardConfig
// ---------------------------------------------------------------------------

/// Configuration for exfiltration guards, nested under
/// `[security.exfiltration_guard]` in the agent config file.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct ExfiltrationGuardConfig {
    /// Strip external markdown images from LLM output to prevent pixel-tracking exfiltration.
    #[serde(default = "default_true")]
    pub block_markdown_images: bool,

    /// Cross-reference tool call arguments against URLs seen in flagged untrusted content.
    #[serde(default = "default_true")]
    pub validate_tool_urls: bool,

    /// Skip Qdrant embedding for messages that contained injection-flagged content.
    #[serde(default = "default_true")]
    pub guard_memory_writes: bool,
}

impl Default for ExfiltrationGuardConfig {
    fn default() -> Self {
        Self {
            block_markdown_images: true,
            validate_tool_urls: true,
            guard_memory_writes: true,
        }
    }
}

// ---------------------------------------------------------------------------
// MemoryWriteValidationConfig
// ---------------------------------------------------------------------------

fn default_max_content_bytes() -> usize {
    4096
}

fn default_max_entity_name_bytes() -> usize {
    256
}

fn default_min_entity_name_bytes() -> usize {
    3
}

fn default_max_fact_bytes() -> usize {
    1024
}

fn default_max_entities() -> usize {
    50
}

fn default_max_edges() -> usize {
    100
}

/// Configuration for memory write validation, nested under `[security.memory_validation]`.
///
/// Enabled by default with conservative limits.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct MemoryWriteValidationConfig {
    /// Master switch. When `false`, validation is a no-op.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Maximum byte length of content passed to `memory_save`.
    #[serde(default = "default_max_content_bytes")]
    pub max_content_bytes: usize,
    /// Minimum byte length of an entity name in graph extraction.
    #[serde(default = "default_min_entity_name_bytes")]
    pub min_entity_name_bytes: usize,
    /// Maximum byte length of a single entity name in graph extraction.
    #[serde(default = "default_max_entity_name_bytes")]
    pub max_entity_name_bytes: usize,
    /// Maximum byte length of an edge fact string in graph extraction.
    #[serde(default = "default_max_fact_bytes")]
    pub max_fact_bytes: usize,
    /// Maximum number of entities allowed per graph extraction result.
    #[serde(default = "default_max_entities")]
    pub max_entities_per_extraction: usize,
    /// Maximum number of edges allowed per graph extraction result.
    #[serde(default = "default_max_edges")]
    pub max_edges_per_extraction: usize,
    /// Forbidden substring patterns.
    #[serde(default)]
    pub forbidden_content_patterns: Vec<String>,
}

impl Default for MemoryWriteValidationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_content_bytes: default_max_content_bytes(),
            min_entity_name_bytes: default_min_entity_name_bytes(),
            max_entity_name_bytes: default_max_entity_name_bytes(),
            max_fact_bytes: default_max_fact_bytes(),
            max_entities_per_extraction: default_max_entities(),
            max_edges_per_extraction: default_max_edges(),
            forbidden_content_patterns: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// PiiFilterConfig
// ---------------------------------------------------------------------------

/// A single user-defined PII pattern.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct CustomPiiPattern {
    /// Human-readable name used in the replacement label.
    pub name: String,
    /// Regular expression pattern.
    pub pattern: String,
    /// Replacement text. Defaults to `[PII:custom]`.
    #[serde(default = "default_custom_replacement")]
    pub replacement: String,
}

fn default_custom_replacement() -> String {
    "[PII:custom]".to_owned()
}

/// Configuration for the PII filter, nested under `[security.pii_filter]` in the config file.
///
/// Disabled by default — opt-in to avoid unexpected data loss.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct PiiFilterConfig {
    /// Master switch. When `false`, the filter is a no-op.
    #[serde(default)]
    pub enabled: bool,
    /// Scrub email addresses.
    #[serde(default = "default_true")]
    pub filter_email: bool,
    /// Scrub US phone numbers.
    #[serde(default = "default_true")]
    pub filter_phone: bool,
    /// Scrub US Social Security Numbers.
    #[serde(default = "default_true")]
    pub filter_ssn: bool,
    /// Scrub credit card numbers (16-digit patterns).
    #[serde(default = "default_true")]
    pub filter_credit_card: bool,
    /// Custom regex patterns to add on top of the built-ins.
    #[serde(default)]
    pub custom_patterns: Vec<CustomPiiPattern>,
}

impl Default for PiiFilterConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            filter_email: true,
            filter_phone: true,
            filter_ssn: true,
            filter_credit_card: true,
            custom_patterns: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// GuardrailConfig
// ---------------------------------------------------------------------------

/// What happens when the guardrail flags input.
#[cfg(feature = "guardrail")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum GuardrailAction {
    /// Block the input and return an error message to the user.
    #[default]
    Block,
    /// Allow the input but emit a warning message.
    Warn,
}

/// Behavior on timeout or LLM error.
#[cfg(feature = "guardrail")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum GuardrailFailStrategy {
    /// Block input on timeout/error (safe default for security-sensitive deployments).
    #[default]
    Closed,
    /// Allow input on timeout/error (for availability-sensitive deployments).
    Open,
}

/// Configuration for the LLM-based guardrail, nested under `[security.guardrail]`.
#[cfg(feature = "guardrail")]
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct GuardrailConfig {
    /// Enable the guardrail (default: false).
    #[serde(default)]
    pub enabled: bool,
    /// Provider to use for guardrail classification (e.g. `"ollama"`, `"claude"`).
    #[serde(default)]
    pub provider: Option<String>,
    /// Model to use (e.g. `"llama-guard-3:1b"`).
    #[serde(default)]
    pub model: Option<String>,
    /// Timeout for each guardrail LLM call in milliseconds (default: 500).
    #[serde(default = "default_guardrail_timeout_ms")]
    pub timeout_ms: u64,
    /// Action to take when a message is flagged (default: block).
    #[serde(default)]
    pub action: GuardrailAction,
    /// What to do on timeout or LLM error (default: closed — block).
    #[serde(default = "default_fail_strategy")]
    pub fail_strategy: GuardrailFailStrategy,
    /// When `true`, also scan tool outputs before they enter message history (default: false).
    #[serde(default)]
    pub scan_tool_output: bool,
    /// Maximum number of characters to send to the guard model (default: 4096).
    #[serde(default = "default_max_input_chars")]
    pub max_input_chars: usize,
}

#[cfg(feature = "guardrail")]
fn default_guardrail_timeout_ms() -> u64 {
    500
}

#[cfg(feature = "guardrail")]
fn default_max_input_chars() -> usize {
    4096
}

#[cfg(feature = "guardrail")]
fn default_fail_strategy() -> GuardrailFailStrategy {
    GuardrailFailStrategy::Closed
}

#[cfg(feature = "guardrail")]
impl Default for GuardrailConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: None,
            model: None,
            timeout_ms: default_guardrail_timeout_ms(),
            action: GuardrailAction::default(),
            fail_strategy: default_fail_strategy(),
            scan_tool_output: false,
            max_input_chars: default_max_input_chars(),
        }
    }
}

// ---------------------------------------------------------------------------
// ResponseVerificationConfig
// ---------------------------------------------------------------------------

/// Configuration for post-LLM response verification, nested under
/// `[security.response_verification]` in the agent config file.
///
/// Scans LLM responses for injected instruction patterns before tool dispatch.
/// This is defense-in-depth layer 3 (after input sanitization and pre-execution verification).
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct ResponseVerificationConfig {
    /// Enable post-LLM response verification (default: true).
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Block tool dispatch when injection patterns are detected (default: false).
    ///
    /// When `false`, flagged responses are logged and shown in the TUI SEC panel
    /// but still delivered. When `true`, the response is suppressed and the user
    /// is notified.
    #[serde(default)]
    pub block_on_detection: bool,
    /// Optional LLM provider for async deep verification of flagged responses.
    ///
    /// When set: suspicious responses are delivered immediately with a `[FLAGGED]`
    /// annotation, and background LLM verification runs asynchronously. The verifier
    /// receives a sanitized summary (via `QuarantinedSummarizer`) to prevent recursive
    /// injection. Empty string = disabled (regex-only verification).
    #[serde(default)]
    pub verifier_provider: String,
}

impl Default for ResponseVerificationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            block_on_detection: false,
            verifier_provider: String::new(),
        }
    }
}
