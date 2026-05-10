// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use zeph_common::SkillTrustLevel;

use crate::tools::{AutonomyLevel, PreExecutionVerifierConfig};

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

// ── Trajectory Sentinel ──────────────────────────────────────────────────────

fn default_decay_per_turn() -> f32 {
    0.85
}
fn default_window_turns() -> u32 {
    8
}
fn default_elevated_at() -> f32 {
    2.0
}
fn default_high_at() -> f32 {
    4.0
}
fn default_critical_at() -> f32 {
    8.0
}
fn default_alert_threshold() -> f32 {
    4.0
}
fn default_auto_recover_after_turns() -> u32 {
    16
}
fn default_subagent_inheritance_factor() -> f32 {
    0.5
}
fn default_high_call_rate_threshold() -> u32 {
    12
}
fn default_unusual_read_threshold() -> u32 {
    24
}
fn default_auto_recover_floor() -> u32 {
    4
}

/// Configuration for `TrajectorySentinel`, nested under `[security.trajectory]` in TOML.
///
/// Controls signal decay, risk level thresholds, auto-recovery, and subagent inheritance.
///
/// # Example (TOML)
///
/// ```toml
/// [security.trajectory]
/// decay_per_turn = 0.85
/// elevated_at = 2.0
/// high_at = 4.0
/// critical_at = 8.0
/// alert_threshold = 4.0
/// auto_recover_after_turns = 16
/// subagent_inheritance_factor = 0.5
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TrajectorySentinelConfig {
    /// Multiplicative decay applied to the running score at each `advance_turn()` call.
    ///
    /// Must be in `(0.0, 1.0]`. Default 0.85 gives a half-life of ≈ 4.3 turns.
    #[serde(default = "default_decay_per_turn")]
    pub decay_per_turn: f32,
    /// Number of past turns to keep in the signal buffer.
    ///
    /// Older signals are evicted once the buffer exceeds this size. Default 8.
    #[serde(default = "default_window_turns")]
    pub window_turns: u32,
    /// Score threshold for transitioning from `Calm` to `Elevated`. Default 2.0.
    #[serde(default = "default_elevated_at")]
    pub elevated_at: f32,
    /// Score threshold for transitioning from `Elevated` to `High`. Default 4.0.
    #[serde(default = "default_high_at")]
    pub high_at: f32,
    /// Score threshold for transitioning from `High` to `Critical`. Default 8.0.
    #[serde(default = "default_critical_at")]
    pub critical_at: f32,
    /// Score at which `PolicyGateExecutor` is notified via `RiskAlert`. Default 4.0.
    ///
    /// Decoupled from `elevated_at` to prevent alert noise for routine minor events.
    #[serde(default = "default_alert_threshold")]
    pub alert_threshold: f32,
    /// Consecutive `Critical` turns before a hard auto-recover reset. Minimum 4. Default 16.
    #[serde(default = "default_auto_recover_after_turns")]
    pub auto_recover_after_turns: u32,
    /// Fraction of parent score inherited by a subagent when parent is `>= Elevated`.
    ///
    /// Default 0.5 (≈ one decay half-life). Config validator warns when this deviates
    /// more than 0.1 from `decay_per_turn ^ (ln(0.5) / ln(decay_per_turn))`.
    #[serde(default = "default_subagent_inheritance_factor")]
    pub subagent_inheritance_factor: f32,
    /// Tool-call count per 3-turn window above which `HighCallRate` fires. Default 12.
    #[serde(default = "default_high_call_rate_threshold")]
    pub high_call_rate_threshold: u32,
    /// Distinct paths read within `window_turns` above which `UnusualReadVolume` fires. Default 24.
    #[serde(default = "default_unusual_read_threshold")]
    pub unusual_read_threshold: u32,
}

impl Default for TrajectorySentinelConfig {
    fn default() -> Self {
        Self {
            decay_per_turn: default_decay_per_turn(),
            window_turns: default_window_turns(),
            elevated_at: default_elevated_at(),
            high_at: default_high_at(),
            critical_at: default_critical_at(),
            alert_threshold: default_alert_threshold(),
            auto_recover_after_turns: default_auto_recover_after_turns(),
            subagent_inheritance_factor: default_subagent_inheritance_factor(),
            high_call_rate_threshold: default_high_call_rate_threshold(),
            unusual_read_threshold: default_unusual_read_threshold(),
        }
    }
}

impl TrajectorySentinelConfig {
    /// Validate numeric bounds. Returns an error string when validation fails.
    ///
    /// # Errors
    ///
    /// Returns a description of the first validation failure found.
    pub fn validate(&self) -> Result<(), String> {
        if self.decay_per_turn <= 0.0 || self.decay_per_turn > 1.0 {
            return Err(format!(
                "trajectory.decay_per_turn must be in (0.0, 1.0]; got {}",
                self.decay_per_turn
            ));
        }
        if self.elevated_at >= self.high_at {
            return Err(format!(
                "trajectory: elevated_at ({}) must be < high_at ({})",
                self.elevated_at, self.high_at
            ));
        }
        if self.high_at >= self.critical_at {
            return Err(format!(
                "trajectory: high_at ({}) must be < critical_at ({})",
                self.high_at, self.critical_at
            ));
        }
        if self.auto_recover_after_turns < default_auto_recover_floor() {
            return Err(format!(
                "trajectory.auto_recover_after_turns must be >= {}; got {}",
                default_auto_recover_floor(),
                self.auto_recover_after_turns
            ));
        }
        // Advisory: warn when subagent_inheritance_factor deviates from calibrated value.
        if self.decay_per_turn < 1.0 {
            let ideal = self
                .decay_per_turn
                .powf(0.5_f32.ln() / self.decay_per_turn.ln());
            if (self.subagent_inheritance_factor - ideal).abs() > 0.1 {
                // Not a hard error — warn only.
                tracing::warn!(
                    configured = self.subagent_inheritance_factor,
                    ideal = ideal,
                    decay = self.decay_per_turn,
                    "trajectory.subagent_inheritance_factor deviates from calibrated value by more than 0.1"
                );
            }
        }
        Ok(())
    }
}

// ── ShadowSentinel ──────────────────────────────────────────────────────────

fn default_shadow_max_context_events() -> usize {
    50
}
fn default_shadow_probe_timeout_ms() -> u64 {
    2000
}
fn default_shadow_max_probes_per_turn() -> usize {
    3
}
fn default_shadow_probe_patterns() -> Vec<String> {
    vec![
        "builtin:shell".to_owned(),
        "builtin:write".to_owned(),
        "builtin:edit".to_owned(),
        "mcp:*/file_*".to_owned(),
        "mcp:*/exec_*".to_owned(),
    ]
}

/// Configuration for the `ShadowSentinel` subsystem, nested under `[security.shadow_sentinel]`.
///
/// `ShadowSentinel` is a defence-in-depth layer (Phase 2 of spec 050) that persists safety
/// events across sessions and runs an LLM probe before high-risk tool execution. It is NOT
/// the primary security gate — `PolicyGateExecutor` and `TrajectorySentinel` remain the
/// primary enforcement mechanisms and are unaffected by probe timeouts.
///
/// # Example (TOML)
///
/// ```toml
/// [security.shadow_sentinel]
/// enabled = true
/// probe_provider = "fast"
/// probe_timeout_ms = 2000
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ShadowSentinelConfig {
    /// Whether the feature is enabled. Default: `false` (opt-in).
    #[serde(default)]
    pub enabled: bool,
    /// Provider name (from `[[llm.providers]]`) used for the safety probe LLM call.
    ///
    /// Empty string means use the main/default provider. A fast, cheap provider
    /// (e.g. `gpt-4o-mini`) is strongly recommended to minimise turn latency.
    #[serde(default)]
    pub probe_provider: String,
    /// Maximum number of trajectory events to include in the probe context. Default: 50.
    #[serde(default = "default_shadow_max_context_events")]
    pub max_context_events: usize,
    /// Timeout for the probe LLM call in milliseconds. Default: 2000.
    #[serde(default = "default_shadow_probe_timeout_ms")]
    pub probe_timeout_ms: u64,
    /// Maximum probe calls per turn to cap LLM costs. Default: 3.
    #[serde(default = "default_shadow_max_probes_per_turn")]
    pub max_probes_per_turn: usize,
    /// Glob patterns over fully-qualified tool ids that trigger the safety probe.
    ///
    /// Default covers shell execution, file writes, and MCP file/exec tools.
    #[serde(default = "default_shadow_probe_patterns")]
    pub probe_patterns: Vec<String>,
    /// When `true`, a probe timeout or LLM error causes the tool call to be denied.
    /// When `false` (default), a probe failure causes the call to be allowed (fail-open).
    ///
    /// Fail-open is the correct default because:
    /// - `ShadowSentinel` is defence-in-depth, not the primary gate.
    /// - Failing closed on probe timeout would allow a `DoS` (slow context → disabled tools).
    /// - `PolicyGateExecutor` + `TrajectorySentinel` continue to enforce policy regardless.
    #[serde(default)]
    pub deny_on_timeout: bool,
}

impl Default for ShadowSentinelConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            probe_provider: String::new(),
            max_context_events: default_shadow_max_context_events(),
            probe_timeout_ms: default_shadow_probe_timeout_ms(),
            max_probes_per_turn: default_shadow_max_probes_per_turn(),
            probe_patterns: default_shadow_probe_patterns(),
            deny_on_timeout: false,
        }
    }
}

// ── Capability Scopes ────────────────────────────────────────────────────────

/// Strictness mode for glob pattern matching against the tool registry.
///
/// Controls whether a zero-match glob is a fatal error or a warning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PatternStrictness {
    /// All namespaces are strict — zero-match globs are fatal.
    Strict,
    /// All namespaces are permissive — zero-match globs are warnings only.
    Permissive,
    /// `builtin:` and `skill:` globs are strict; `mcp:`, `acp:`, `a2a:` are provisional.
    ///
    /// This is the default because MCP servers may not be connected at startup.
    #[default]
    ProvisionalForDynamicNamespaces,
}

/// Configuration for a single task-type scope, nested under
/// `[security.capability_scopes.<task_type>]`.
///
/// # Example (TOML)
///
/// ```toml
/// [security.capability_scopes.research]
/// patterns = ["builtin:fetch", "builtin:web_scrape", "builtin:search_*"]
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ScopeConfig {
    /// Glob patterns over fully-qualified tool ids (`<namespace>:<tool>`).
    ///
    /// Evaluated against the materialised tool registry at agent build time.
    #[serde(default)]
    pub patterns: Vec<String>,
}

/// Top-level capability scopes configuration, nested under `[security.capability_scopes]`.
///
/// # Example (TOML)
///
/// ```toml
/// [security.capability_scopes]
/// default_scope = "general"
/// strict = true
///
/// [security.capability_scopes.general]
/// patterns = ["*"]
///
/// [security.capability_scopes.research]
/// patterns = ["builtin:fetch", "builtin:web_scrape", "builtin:search_*", "builtin:read"]
///
/// [security.capability_scopes.code_edit]
/// patterns = ["builtin:read", "builtin:edit", "builtin:write", "builtin:shell", "builtin:glob"]
/// ```
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct CapabilityScopesConfig {
    /// Name of the scope used when no task type is specified. Default: `"general"`.
    ///
    /// When `default_scope = "general"` and a `[security.capability_scopes.general]` section
    /// with `patterns = ["*"]` exists, scoping is a no-op identity (full tool set surfaced).
    #[serde(default = "default_scope_name")]
    pub default_scope: String,
    /// When `true`, an unrecognised `task_type` is a fatal startup error.
    /// When `false`, falls back to `default_scope`. Default: `false`.
    #[serde(default)]
    pub strict: bool,
    /// Per-namespace strictness for zero-match glob patterns.
    #[serde(default)]
    pub pattern_strictness: PatternStrictness,
    /// Named scopes. Keys are task-type names; values are their scope configurations.
    #[serde(default, flatten)]
    pub scopes: HashMap<String, ScopeConfig>,
}

fn default_scope_name() -> String {
    "general".to_owned()
}

// ── Agent security configuration ─────────────────────────────────────────────

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
    /// Trajectory risk sentinel configuration.
    ///
    /// Controls signal decay, risk level thresholds, auto-recovery, and subagent inheritance.
    /// See spec 050 and `crates/zeph-core/src/agent/trajectory.rs`.
    #[serde(default)]
    pub trajectory: TrajectorySentinelConfig,
    /// Capability scope configuration.
    ///
    /// Maps task-type names to glob-pattern allow-lists over fully-qualified tool ids.
    /// When empty, scoping is a no-op (full tool set surfaced to LLM).
    #[serde(default)]
    pub capability_scopes: CapabilityScopesConfig,
    /// `ShadowSentinel` Phase 2: persistent safety event stream + LLM pre-execution probe.
    ///
    /// Disabled by default. When enabled, high-risk tool calls are probed by an LLM
    /// before execution. `ShadowSentinel` is defence-in-depth only — `PolicyGateExecutor`
    /// and `TrajectorySentinel` remain the primary enforcement mechanisms.
    #[serde(default)]
    pub shadow_sentinel: ShadowSentinelConfig,
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
            trajectory: TrajectorySentinelConfig::default(),
            capability_scopes: CapabilityScopesConfig::default(),
            shadow_sentinel: ShadowSentinelConfig::default(),
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
        let toml = r"
context_prep_timeout_secs = 60
no_providers_backoff_secs = 10
";
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
