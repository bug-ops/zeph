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
    #[serde(
        default = "default_embedding_threshold",
        deserialize_with = "validate_embedding_threshold"
    )]
    pub threshold: f64,
    /// Minimum clean samples before centroid-based detection activates.
    /// Before this count, regex fallback is used instead.
    #[serde(
        default = "default_embedding_min_samples",
        deserialize_with = "validate_min_samples"
    )]
    pub min_samples: usize,
    /// EMA alpha floor for centroid updates after stabilization (n >= `min_samples`).
    ///
    /// Once the centroid has accumulated `min_samples` clean outputs, each new sample
    /// can shift it by at most this fraction. Lower values make the centroid more
    /// resistant to slow drift attacks but slower to adapt to legitimate distribution
    /// changes. Default: 0.01 (1% per sample).
    #[serde(default = "default_ema_floor")]
    pub ema_floor: f32,
}

fn validate_embedding_threshold<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = <f64 as serde::Deserialize>::deserialize(deserializer)?;
    if value.is_nan() || value.is_infinite() {
        return Err(serde::de::Error::custom(
            "embedding_guard.threshold must be a finite number",
        ));
    }
    if !(value > 0.0 && value <= 1.0) {
        return Err(serde::de::Error::custom(
            "embedding_guard.threshold must be in (0.0, 1.0]",
        ));
    }
    Ok(value)
}

fn validate_min_samples<'de, D>(deserializer: D) -> Result<usize, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = <usize as serde::Deserialize>::deserialize(deserializer)?;
    if value == 0 {
        return Err(serde::de::Error::custom(
            "embedding_guard.min_samples must be >= 1",
        ));
    }
    Ok(value)
}

fn default_embedding_threshold() -> f64 {
    0.35
}

fn default_embedding_min_samples() -> usize {
    10
}

fn default_ema_floor() -> f32 {
    0.01
}

impl Default for EmbeddingGuardConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            threshold: default_embedding_threshold(),
            min_samples: default_embedding_min_samples(),
            ema_floor: default_ema_floor(),
        }
    }
}

/// Configuration for the content isolation pipeline, nested under
/// `[security.content_isolation]` in the agent config file.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[allow(clippy::struct_excessive_bools)]
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

    /// When `true`, MCP tool results flowing through ACP-serving sessions receive
    /// unconditional quarantine summarization and cross-boundary audit log entries.
    /// This prevents confused-deputy attacks where untrusted MCP output influences
    /// responses served to ACP clients (e.g. IDE integrations).
    #[serde(default = "default_true")]
    pub mcp_to_acp_boundary: bool,
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
            mcp_to_acp_boundary: true,
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
fn default_guardrail_timeout_ms() -> u64 {
    500
}
fn default_max_input_chars() -> usize {
    4096
}
fn default_fail_strategy() -> GuardrailFailStrategy {
    GuardrailFailStrategy::Closed
}
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_isolation_default_mcp_to_acp_boundary_true() {
        let cfg = ContentIsolationConfig::default();
        assert!(cfg.mcp_to_acp_boundary);
    }

    #[test]
    fn content_isolation_deserialize_mcp_to_acp_boundary_false() {
        let toml = r"
            mcp_to_acp_boundary = false
        ";
        let cfg: ContentIsolationConfig = toml::from_str(toml).unwrap();
        assert!(!cfg.mcp_to_acp_boundary);
    }

    #[test]
    fn content_isolation_deserialize_absent_defaults_true() {
        let cfg: ContentIsolationConfig = toml::from_str("").unwrap();
        assert!(cfg.mcp_to_acp_boundary);
    }

    fn de_guard(toml: &str) -> Result<EmbeddingGuardConfig, toml::de::Error> {
        toml::from_str(toml)
    }

    #[test]
    fn threshold_valid() {
        let cfg = de_guard("threshold = 0.35\nmin_samples = 5").unwrap();
        assert!((cfg.threshold - 0.35).abs() < f64::EPSILON);
    }

    #[test]
    fn threshold_one_valid() {
        let cfg = de_guard("threshold = 1.0\nmin_samples = 1").unwrap();
        assert!((cfg.threshold - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn threshold_zero_rejected() {
        assert!(de_guard("threshold = 0.0\nmin_samples = 1").is_err());
    }

    #[test]
    fn threshold_above_one_rejected() {
        assert!(de_guard("threshold = 1.5\nmin_samples = 1").is_err());
    }

    #[test]
    fn threshold_negative_rejected() {
        assert!(de_guard("threshold = -0.1\nmin_samples = 1").is_err());
    }

    #[test]
    fn min_samples_zero_rejected() {
        assert!(de_guard("threshold = 0.35\nmin_samples = 0").is_err());
    }

    #[test]
    fn min_samples_one_valid() {
        let cfg = de_guard("threshold = 0.35\nmin_samples = 1").unwrap();
        assert_eq!(cfg.min_samples, 1);
    }
}

// ---------------------------------------------------------------------------
// CausalIpiConfig
// ---------------------------------------------------------------------------

fn default_causal_threshold() -> f32 {
    0.7
}

fn validate_causal_threshold<'de, D>(deserializer: D) -> Result<f32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = <f32 as serde::Deserialize>::deserialize(deserializer)?;
    if value.is_nan() || value.is_infinite() {
        return Err(serde::de::Error::custom(
            "causal_ipi.threshold must be a finite number",
        ));
    }
    if !(value > 0.0 && value <= 1.0) {
        return Err(serde::de::Error::custom(
            "causal_ipi.threshold must be in (0.0, 1.0]",
        ));
    }
    Ok(value)
}

fn default_probe_max_tokens() -> u32 {
    100
}

fn default_probe_timeout_ms() -> u64 {
    3000
}

/// Temporal causal IPI analysis at tool-return boundaries.
///
/// When enabled, the agent generates behavioral probes before and after tool batch dispatch
/// and compares them to detect behavioral deviation caused by injected instructions in
/// tool outputs. Probes are per-batch (2 LLM calls total), not per individual tool.
///
/// Config section: `[security.causal_ipi]`
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct CausalIpiConfig {
    /// Master switch. Default: false (opt-in).
    #[serde(default)]
    pub enabled: bool,

    /// Causal attribution score threshold for flagging. Range: (0.0, 1.0]. Default 0.7.
    ///
    /// Scores above this value trigger a WARN log, metric increment, and `SecurityEvent`.
    /// Content is never blocked — this is an observation layer only.
    #[serde(
        default = "default_causal_threshold",
        deserialize_with = "validate_causal_threshold"
    )]
    pub threshold: f32,

    /// LLM provider name from `[[llm.providers]]` for probe calls.
    ///
    /// Should reference a fast/cheap provider — probes run on every tool batch return.
    /// When `None`, falls back to the agent's default provider.
    #[serde(default)]
    pub provider: Option<String>,

    /// Maximum tokens for each probe response. Limits cost per probe call. Default: 100.
    ///
    /// Two probes per batch = max `2 * probe_max_tokens` output tokens per tool batch.
    #[serde(default = "default_probe_max_tokens")]
    pub probe_max_tokens: u32,

    /// Timeout in milliseconds for each individual probe LLM call. Default: 3000.
    ///
    /// On timeout: WARN log, skip causal analysis for the batch (never block).
    #[serde(default = "default_probe_timeout_ms")]
    pub probe_timeout_ms: u64,
}

impl Default for CausalIpiConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            threshold: default_causal_threshold(),
            provider: None,
            probe_max_tokens: default_probe_max_tokens(),
            probe_timeout_ms: default_probe_timeout_ms(),
        }
    }
}

#[cfg(test)]
mod causal_ipi_tests {
    use super::*;

    #[test]
    fn causal_ipi_defaults() {
        let cfg = CausalIpiConfig::default();
        assert!(!cfg.enabled);
        assert!((cfg.threshold - 0.7).abs() < 1e-6);
        assert!(cfg.provider.is_none());
        assert_eq!(cfg.probe_max_tokens, 100);
        assert_eq!(cfg.probe_timeout_ms, 3000);
    }

    #[test]
    fn causal_ipi_deserialize_enabled() {
        let toml = r#"
            enabled = true
            threshold = 0.8
            provider = "fast"
            probe_max_tokens = 150
            probe_timeout_ms = 5000
        "#;
        let cfg: CausalIpiConfig = toml::from_str(toml).unwrap();
        assert!(cfg.enabled);
        assert!((cfg.threshold - 0.8).abs() < 1e-6);
        assert_eq!(cfg.provider.as_deref(), Some("fast"));
        assert_eq!(cfg.probe_max_tokens, 150);
        assert_eq!(cfg.probe_timeout_ms, 5000);
    }

    #[test]
    fn causal_ipi_threshold_zero_rejected() {
        let result: Result<CausalIpiConfig, _> = toml::from_str("threshold = 0.0");
        assert!(result.is_err());
    }

    #[test]
    fn causal_ipi_threshold_above_one_rejected() {
        let result: Result<CausalIpiConfig, _> = toml::from_str("threshold = 1.1");
        assert!(result.is_err());
    }

    #[test]
    fn causal_ipi_threshold_exactly_one_accepted() {
        let cfg: CausalIpiConfig = toml::from_str("threshold = 1.0").unwrap();
        assert!((cfg.threshold - 1.0).abs() < 1e-6);
    }
}
