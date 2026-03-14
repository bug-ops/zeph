// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use serde::{Deserialize, Serialize};
use zeph_skills::TrustLevel;
use zeph_tools::AutonomyLevel;

use crate::agent::rate_limiter::RateLimitConfig;
use crate::sanitizer::ContentIsolationConfig;
use crate::sanitizer::exfiltration::ExfiltrationGuardConfig;
use crate::sanitizer::memory_validation::MemoryWriteValidationConfig;
use crate::sanitizer::pii::PiiFilterConfig;

use super::defaults::default_true;

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
}

impl Default for TrustConfig {
    fn default() -> Self {
        Self {
            default_level: default_trust_default_level(),
            local_level: default_trust_local_level(),
            hash_mismatch_level: default_trust_hash_mismatch_level(),
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
    ///
    /// Note: The legacy tool path (`providers without native function-calling`) is not
    /// covered by this rate limiter. For MVP, only the native tool dispatch path is
    /// rate-limited. See architecture decision S2.
    #[serde(default)]
    pub rate_limit: RateLimitConfig,
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
