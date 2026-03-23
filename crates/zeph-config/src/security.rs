// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use serde::{Deserialize, Serialize};
use zeph_tools::AutonomyLevel;
use zeph_tools::PreExecutionVerifierConfig;
use zeph_tools::TrustLevel;

use crate::defaults::default_true;
use crate::rate_limit::RateLimitConfig;
use crate::sanitizer::{
    ContentIsolationConfig, ExfiltrationGuardConfig, MemoryWriteValidationConfig, PiiFilterConfig,
    ResponseVerificationConfig,
};

#[cfg(feature = "guardrail")]
use crate::sanitizer::GuardrailConfig;

fn default_trust_default_level() -> TrustLevel {
    TrustLevel::Quarantined
}

fn default_trust_local_level() -> TrustLevel {
    TrustLevel::Trusted
}

fn default_trust_hash_mismatch_level() -> TrustLevel {
    TrustLevel::Quarantined
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

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TrustConfig {
    #[serde(default = "default_trust_default_level")]
    pub default_level: TrustLevel,
    #[serde(default = "default_trust_local_level")]
    pub local_level: TrustLevel,
    #[serde(default = "default_trust_hash_mismatch_level")]
    pub hash_mismatch_level: TrustLevel,
    /// Scan skill body content for injection patterns at load time.
    ///
    /// When `true`, `SkillRegistry::scan_loaded()` is called at agent startup.
    /// This is **advisory only** — scan results are logged as warnings and do not
    /// automatically change trust levels or block tool calls.
    ///
    /// Defaults to `true` (secure by default).
    #[serde(default = "default_true")]
    pub scan_on_load: bool,
}

impl Default for TrustConfig {
    fn default() -> Self {
        Self {
            default_level: default_trust_default_level(),
            local_level: default_trust_local_level(),
            hash_mismatch_level: default_trust_hash_mismatch_level(),
            scan_on_load: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SecurityConfig {
    #[serde(default = "default_true")]
    pub redact_secrets: bool,
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
    #[cfg(feature = "guardrail")]
    #[serde(default)]
    pub guardrail: GuardrailConfig,
    /// Post-LLM response verification layer (enabled by default).
    #[serde(default)]
    pub response_verification: ResponseVerificationConfig,
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
            #[cfg(feature = "guardrail")]
            guardrail: GuardrailConfig::default(),
            response_verification: ResponseVerificationConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
pub struct TimeoutConfig {
    #[serde(default = "default_llm_timeout")]
    pub llm_seconds: u64,
    #[serde(default = "default_llm_request_timeout")]
    pub llm_request_timeout_secs: u64,
    #[serde(default = "default_embedding_timeout")]
    pub embedding_seconds: u64,
    #[serde(default = "default_a2a_timeout")]
    pub a2a_seconds: u64,
    #[serde(default = "default_max_parallel_tools")]
    pub max_parallel_tools: usize,
}

impl Default for TimeoutConfig {
    fn default() -> Self {
        Self {
            llm_seconds: default_llm_timeout(),
            llm_request_timeout_secs: default_llm_request_timeout(),
            embedding_seconds: default_embedding_timeout(),
            a2a_seconds: default_a2a_timeout(),
            max_parallel_tools: default_max_parallel_tools(),
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
            default_level: TrustLevel::Quarantined,
            local_level: TrustLevel::Trusted,
            hash_mismatch_level: TrustLevel::Quarantined,
            scan_on_load: false,
        };
        let toml = toml::to_string(&config).expect("serialize");
        let deserialized: TrustConfig = toml::from_str(&toml).expect("deserialize");
        assert!(!deserialized.scan_on_load);
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
}
