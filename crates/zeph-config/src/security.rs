// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use serde::{Deserialize, Serialize};
use zeph_tools::AutonomyLevel;
use zeph_tools::PreExecutionVerifierConfig;
use zeph_tools::SkillTrustLevel;

use crate::defaults::default_true;
use crate::vigil::VigilConfig;

/// Fine-grained controls for the skill body scanner.
///
/// Nested under `[skills.trust.scanner]` in TOML.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ScannerConfig {
    /// Scan skill body content for injection patterns at load time.
    ///
    /// More specific than `scan_on_load` (which controls whether `scan_loaded()` is called at
    /// all). When `scan_on_load = true` and `injection_patterns = false`, the scan loop still
    /// runs but skips the injection pattern check.
    #[serde(default = "default_true")]
    pub injection_patterns: bool,
    /// Check whether a skill's `allowed_tools` exceed its trust level's permissions.
    ///
    /// When enabled, the bootstrap calls `check_escalations()` on the registry and logs
    /// warnings for any tool declarations that violate the trust boundary.
    #[serde(default)]
    pub capability_escalation_check: bool,
}

impl Default for ScannerConfig {
    fn default() -> Self {
        Self {
            injection_patterns: true,
            capability_escalation_check: false,
        }
    }
}
use crate::rate_limit::RateLimitConfig;
use crate::sanitizer::GuardrailConfig;
use crate::sanitizer::{
    CausalIpiConfig, ContentIsolationConfig, ExfiltrationGuardConfig, MemoryWriteValidationConfig,
    PiiFilterConfig, ResponseVerificationConfig,
};

fn default_trust_default_level() -> SkillTrustLevel {
    SkillTrustLevel::Quarantined
}

fn default_trust_local_level() -> SkillTrustLevel {
    SkillTrustLevel::Trusted
}

fn default_trust_hash_mismatch_level() -> SkillTrustLevel {
    SkillTrustLevel::Quarantined
}

fn default_trust_bundled_level() -> SkillTrustLevel {
    SkillTrustLevel::Trusted
}

fn default_llm_timeout() -> u64 {
    120
}

fn default_embedding_timeout() -> u64 {
    30
}

fn default_a2a_timeout() -> u64 {
    30
}

fn default_max_parallel_tools() -> usize {
    8
}

fn default_llm_request_timeout() -> u64 {
    600
}

fn default_context_prep_timeout() -> u64 {
    30
}

fn default_no_providers_backoff_secs() -> u64 {
    2
}

/// Skill trust policy configuration, nested under `[skills.trust]` in TOML.
///
/// Controls how trust levels are assigned to skills at load time based on their
/// origin (local filesystem vs network) and integrity (hash verification result).
///
/// # Example (TOML)
///
/// ```toml
/// [skills.trust]
/// default_level = "quarantined"
/// local_level = "trusted"
/// scan_on_load = true
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TrustConfig {
    /// Trust level assigned to skills from unknown or remote origins. Default: `quarantined`.
    #[serde(default = "default_trust_default_level")]
    pub default_level: SkillTrustLevel,
    /// Trust level assigned to skills found on the local filesystem. Default: `trusted`.
    #[serde(default = "default_trust_local_level")]
    pub local_level: SkillTrustLevel,
    /// Trust level assigned when a skill's content hash does not match the stored hash.
    /// Default: `quarantined`.
    #[serde(default = "default_trust_hash_mismatch_level")]
    pub hash_mismatch_level: SkillTrustLevel,
    /// Trust level assigned to bundled (built-in) skills shipped with the binary. Default: `trusted`.
    #[serde(default = "default_trust_bundled_level")]
    pub bundled_level: SkillTrustLevel,
    /// Scan skill body content for injection patterns at load time.
    ///
    /// When `true`, `SkillRegistry::scan_loaded()` is called at agent startup.
    /// This is **advisory only** — scan results are logged as warnings and do not
    /// automatically change trust levels or block tool calls.
    ///
    /// Defaults to `true` (secure by default).
    #[serde(default = "default_true")]
    pub scan_on_load: bool,
    /// Fine-grained scanner controls (injection patterns, capability escalation).
    #[serde(default)]
    pub scanner: ScannerConfig,
}

impl Default for TrustConfig {
    fn default() -> Self {
        Self {
            default_level: default_trust_default_level(),
            local_level: default_trust_local_level(),
            hash_mismatch_level: default_trust_hash_mismatch_level(),
            bundled_level: default_trust_bundled_level(),
            scan_on_load: true,
            scanner: ScannerConfig::default(),
        }
    }
}

/// Agent security configuration, nested under `[security]` in TOML.
///
/// Aggregates all security-related subsystems: content isolation, exfiltration guards,
/// memory write validation, PII filtering, rate limiting, prompt injection screening,
/// and response verification.
///
/// # Example (TOML)
///
/// ```toml
/// [security]
/// redact_secrets = true
/// autonomy_level = "moderate"
///
/// [security.rate_limit]
/// enabled = true
/// shell_calls_per_minute = 20
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SecurityConfig {
    /// Automatically redact detected secrets from tool outputs before they reach the LLM.
    /// Default: `true`.
    #[serde(default = "default_true")]
    pub redact_secrets: bool,
    /// Autonomy level controlling which tool actions require explicit user confirmation.
    #[serde(default)]
    pub autonomy_level: AutonomyLevel,
    #[serde(default)]
    pub content_isolation: ContentIsolationConfig,
    #[serde(default)]
    pub exfiltration_guard: ExfiltrationGuardConfig,
    /// Memory write validation (enabled by default).
    #[serde(default)]
    pub memory_validation: MemoryWriteValidationConfig,
    /// PII filter for tool outputs and debug dumps (opt-in, disabled by default).
    #[serde(default)]
    pub pii_filter: PiiFilterConfig,
    /// Tool action rate limiter (opt-in, disabled by default).
    #[serde(default)]
    pub rate_limit: RateLimitConfig,
    /// Pre-execution verifiers (enabled by default).
    #[serde(default)]
    pub pre_execution_verify: PreExecutionVerifierConfig,
    /// LLM-based prompt injection pre-screener (opt-in, disabled by default).
    #[serde(default)]
    pub guardrail: GuardrailConfig,
    /// Post-LLM response verification layer (enabled by default).
    #[serde(default)]
    pub response_verification: ResponseVerificationConfig,
    /// Temporal causal IPI analysis at tool-return boundaries (opt-in, disabled by default).
    #[serde(default)]
    pub causal_ipi: CausalIpiConfig,
    /// VIGIL verify-before-commit intent anchoring gate (enabled by default).
    ///
    /// Runs a regex tripwire before `sanitize_tool_output` to intercept low-effort injection
    /// patterns. See `[[security.vigil]]` in TOML and spec `010-6-vigil-intent-anchoring`.
    #[serde(default)]
    pub vigil: VigilConfig,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            redact_secrets: true,
            autonomy_level: AutonomyLevel::default(),
            content_isolation: ContentIsolationConfig::default(),
            exfiltration_guard: ExfiltrationGuardConfig::default(),
            memory_validation: MemoryWriteValidationConfig::default(),
            pii_filter: PiiFilterConfig::default(),
            rate_limit: RateLimitConfig::default(),
            pre_execution_verify: PreExecutionVerifierConfig::default(),
            guardrail: GuardrailConfig::default(),
            response_verification: ResponseVerificationConfig::default(),
            causal_ipi: CausalIpiConfig::default(),
            vigil: VigilConfig::default(),
        }
    }
}

/// Timeout configuration for external operations, nested under `[timeouts]` in TOML.
///
/// All timeouts are in seconds. Exceeding a timeout returns an error to the agent
/// loop rather than blocking indefinitely.
///
/// # Example (TOML)
///
/// ```toml
/// [timeouts]
/// llm_seconds = 60
/// embedding_seconds = 15
/// max_parallel_tools = 4
/// ```
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
pub struct TimeoutConfig {
    /// Timeout for streaming LLM first-token responses, in seconds. Default: `120`.
    #[serde(default = "default_llm_timeout")]
    pub llm_seconds: u64,
    /// Total wall-clock timeout for a complete LLM request (all tokens), in seconds.
    /// Default: `600`.
    #[serde(default = "default_llm_request_timeout")]
    pub llm_request_timeout_secs: u64,
    /// Timeout for embedding API calls, in seconds. Default: `30`.
    #[serde(default = "default_embedding_timeout")]
    pub embedding_seconds: u64,
    /// Timeout for A2A agent-to-agent calls, in seconds. Default: `30`.
    #[serde(default = "default_a2a_timeout")]
    pub a2a_seconds: u64,
    /// Maximum number of tool calls that may execute concurrently in a single turn.
    /// Default: `8`.
    #[serde(default = "default_max_parallel_tools")]
    pub max_parallel_tools: usize,
    /// Maximum wall-clock time (seconds) allowed for `advance_context_lifecycle` (memory recall,
    /// graph retrieval, proactive compression, context assembly) before it is aborted and the
    /// agent proceeds with a degraded (cached) context.
    ///
    /// Setting this too low may skip useful memory recall; setting it too high blocks the agent
    /// when embed providers are rate-limited or unavailable. Default: `30`.
    #[serde(default = "default_context_prep_timeout")]
    pub context_prep_timeout_secs: u64,
    /// How long to wait (seconds) before retrying a turn after the previous turn ended with
    /// `no providers available`. Prevents a busy-wait loop when all LLM backends are down.
    /// Default: `2`.
    #[serde(default = "default_no_providers_backoff_secs")]
    pub no_providers_backoff_secs: u64,
}

impl Default for TimeoutConfig {
    fn default() -> Self {
        Self {
            llm_seconds: default_llm_timeout(),
            llm_request_timeout_secs: default_llm_request_timeout(),
            embedding_seconds: default_embedding_timeout(),
            a2a_seconds: default_a2a_timeout(),
            max_parallel_tools: default_max_parallel_tools(),
            context_prep_timeout_secs: default_context_prep_timeout(),
            no_providers_backoff_secs: default_no_providers_backoff_secs(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trust_config_default_has_scan_on_load_true() {
        let config = TrustConfig::default();
        assert!(config.scan_on_load);
    }

    #[test]
    fn trust_config_serde_roundtrip_with_scan_on_load() {
        let config = TrustConfig {
            default_level: SkillTrustLevel::Quarantined,
            local_level: SkillTrustLevel::Trusted,
            hash_mismatch_level: SkillTrustLevel::Quarantined,
            bundled_level: SkillTrustLevel::Trusted,
            scan_on_load: false,
            scanner: ScannerConfig::default(),
        };
        let toml = toml::to_string(&config).expect("serialize");
        let deserialized: TrustConfig = toml::from_str(&toml).expect("deserialize");
        assert!(!deserialized.scan_on_load);
        assert_eq!(deserialized.bundled_level, SkillTrustLevel::Trusted);
    }

    #[test]
    fn trust_config_missing_scan_on_load_defaults_to_true() {
        let toml = r#"
default_level = "quarantined"
local_level = "trusted"
hash_mismatch_level = "quarantined"
"#;
        let config: TrustConfig = toml::from_str(toml).expect("deserialize");
        assert!(
            config.scan_on_load,
            "missing scan_on_load must default to true"
        );
    }

    #[test]
    fn trust_config_default_has_bundled_level_trusted() {
        let config = TrustConfig::default();
        assert_eq!(config.bundled_level, SkillTrustLevel::Trusted);
    }

    #[test]
    fn trust_config_missing_bundled_level_defaults_to_trusted() {
        let toml = r#"
default_level = "quarantined"
local_level = "trusted"
hash_mismatch_level = "quarantined"
"#;
        let config: TrustConfig = toml::from_str(toml).expect("deserialize");
        assert_eq!(
            config.bundled_level,
            SkillTrustLevel::Trusted,
            "missing bundled_level must default to trusted"
        );
    }

    #[test]
    fn scanner_config_defaults() {
        let cfg = ScannerConfig::default();
        assert!(cfg.injection_patterns);
        assert!(!cfg.capability_escalation_check);
    }

    #[test]
    fn scanner_config_serde_roundtrip() {
        let cfg = ScannerConfig {
            injection_patterns: false,
            capability_escalation_check: true,
        };
        let toml = toml::to_string(&cfg).expect("serialize");
        let back: ScannerConfig = toml::from_str(&toml).expect("deserialize");
        assert!(!back.injection_patterns);
        assert!(back.capability_escalation_check);
    }

    #[test]
    fn trust_config_scanner_defaults_when_missing() {
        let toml = r#"
default_level = "quarantined"
local_level = "trusted"
hash_mismatch_level = "quarantined"
"#;
        let config: TrustConfig = toml::from_str(toml).expect("deserialize");
        assert!(config.scanner.injection_patterns);
        assert!(!config.scanner.capability_escalation_check);
    }

    // ------------------------------------------------------------------
    // TimeoutConfig — new fields added in #3357
    // ------------------------------------------------------------------

    #[test]
    fn timeout_config_context_prep_timeout_default() {
        let cfg = TimeoutConfig::default();
        assert_eq!(
            cfg.context_prep_timeout_secs, 30,
            "context_prep_timeout_secs default must be 30s (#3357)"
        );
    }

    #[test]
    fn timeout_config_no_providers_backoff_default() {
        let cfg = TimeoutConfig::default();
        assert_eq!(
            cfg.no_providers_backoff_secs, 2,
            "no_providers_backoff_secs default must be 2s (#3357)"
        );
    }

    #[test]
    fn timeout_config_new_fields_deserialize_from_toml() {
        let toml = r#"
context_prep_timeout_secs = 60
no_providers_backoff_secs = 10
"#;
        let cfg: TimeoutConfig = toml::from_str(toml).expect("deserialize");
        assert_eq!(cfg.context_prep_timeout_secs, 60);
        assert_eq!(cfg.no_providers_backoff_secs, 10);
    }

    #[test]
    fn timeout_config_new_fields_default_when_missing_from_toml() {
        // An empty TOML section must produce the same values as TimeoutConfig::default().
        let cfg: TimeoutConfig = toml::from_str("").expect("deserialize empty");
        assert_eq!(cfg.context_prep_timeout_secs, 30);
        assert_eq!(cfg.no_providers_backoff_secs, 2);
    }
}
