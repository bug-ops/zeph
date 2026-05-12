// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Pure-data tool configuration types.
//!
//! Contains all TOML-deserializable configuration structs for tool execution. Runtime
//! types (executors, permission policy enforcement) remain in `zeph-tools`. That crate
//! re-exports the types here so existing import paths continue to resolve.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::providers::ProviderName;
use zeph_common::SkillTrustLevel;

// ── Permissions ──────────────────────────────────────────────────────────────

/// Tool access level controlling agent autonomy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AutonomyLevel {
    /// Read-only tools: `read`, `find_path`, `grep`, `list_directory`, `web_scrape`, `fetch`
    ReadOnly,
    /// Default: rule-based permissions with confirmations.
    #[default]
    Supervised,
    /// All tools allowed, no confirmations.
    Full,
}

/// Action a permission rule resolves to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionAction {
    /// Allow the tool call unconditionally.
    Allow,
    /// Prompt the user before allowing.
    Ask,
    /// Deny the tool call.
    Deny,
}

/// Single permission rule: glob `pattern` + action.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PermissionRule {
    /// Glob pattern matched against the tool input string.
    pub pattern: String,
    /// Action to take when the pattern matches.
    pub action: PermissionAction,
}

/// TOML-deserializable permissions config section.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct PermissionsConfig {
    /// Per-tool permission rules. Key is `tool_id`.
    #[serde(flatten)]
    pub tools: HashMap<String, Vec<PermissionRule>>,
}

// ── Verifiers ────────────────────────────────────────────────────────────────

fn default_true() -> bool {
    true
}

fn default_shell_tools() -> Vec<String> {
    vec![
        "bash".to_string(),
        "shell".to_string(),
        "terminal".to_string(),
    ]
}

fn default_guarded_tools() -> Vec<String> {
    vec!["fetch".to_string(), "web_scrape".to_string()]
}

/// Configuration for the destructive command verifier.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DestructiveVerifierConfig {
    /// Enable the verifier. Default: `true`.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Explicit path prefixes under which destructive commands are permitted.
    #[serde(default)]
    pub allowed_paths: Vec<String>,
    /// Additional command patterns to treat as destructive (substring match).
    #[serde(default)]
    pub extra_patterns: Vec<String>,
    /// Tool names to treat as shell executors (case-insensitive).
    #[serde(default = "default_shell_tools")]
    pub shell_tools: Vec<String>,
}

impl Default for DestructiveVerifierConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            allowed_paths: Vec::new(),
            extra_patterns: Vec::new(),
            shell_tools: default_shell_tools(),
        }
    }
}

/// Configuration for the injection pattern verifier.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct InjectionVerifierConfig {
    /// Enable the verifier. Default: `true`.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Additional injection patterns to block (regex strings).
    #[serde(default)]
    pub extra_patterns: Vec<String>,
    /// URLs explicitly permitted even if they match SSRF patterns.
    #[serde(default)]
    pub allowlisted_urls: Vec<String>,
}

impl Default for InjectionVerifierConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            extra_patterns: Vec::new(),
            allowlisted_urls: Vec::new(),
        }
    }
}

/// Configuration for the URL grounding verifier.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct UrlGroundingVerifierConfig {
    /// Enable the verifier. Default: `true`.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Tool IDs subject to URL grounding checks.
    #[serde(default = "default_guarded_tools")]
    pub guarded_tools: Vec<String>,
}

impl Default for UrlGroundingVerifierConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            guarded_tools: default_guarded_tools(),
        }
    }
}

/// Configuration for the firewall verifier.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FirewallVerifierConfig {
    /// Enable the verifier. Default: `true`.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Glob patterns for additional paths to block.
    #[serde(default)]
    pub blocked_paths: Vec<String>,
    /// Additional environment variable names to block from tool arguments.
    #[serde(default)]
    pub blocked_env_vars: Vec<String>,
    /// Tool IDs exempt from firewall scanning.
    #[serde(default)]
    pub exempt_tools: Vec<String>,
}

impl Default for FirewallVerifierConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            blocked_paths: Vec::new(),
            blocked_env_vars: Vec::new(),
            exempt_tools: Vec::new(),
        }
    }
}

/// Top-level configuration for all pre-execution verifiers.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PreExecutionVerifierConfig {
    /// Enable all verifiers globally. Default: `true`.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Destructive command verifier settings.
    #[serde(default)]
    pub destructive_commands: DestructiveVerifierConfig,
    /// Injection pattern verifier settings.
    #[serde(default)]
    pub injection_patterns: InjectionVerifierConfig,
    /// URL grounding verifier settings.
    #[serde(default)]
    pub url_grounding: UrlGroundingVerifierConfig,
    /// Firewall verifier settings.
    #[serde(default)]
    pub firewall: FirewallVerifierConfig,
}

impl Default for PreExecutionVerifierConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            destructive_commands: DestructiveVerifierConfig::default(),
            injection_patterns: InjectionVerifierConfig::default(),
            url_grounding: UrlGroundingVerifierConfig::default(),
            firewall: FirewallVerifierConfig::default(),
        }
    }
}

// ── Policy ───────────────────────────────────────────────────────────────────

/// Effect applied when a policy rule matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyEffect {
    /// Allow the tool call.
    Allow,
    /// Deny the tool call.
    Deny,
}

/// Default effect when no policy rule matches.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DefaultEffect {
    /// Allow the call when no rule matches.
    Allow,
    /// Deny the call when no rule matches (default, fail-closed).
    #[default]
    Deny,
}

fn default_deny() -> DefaultEffect {
    DefaultEffect::Deny
}

/// TOML-deserializable policy configuration.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct PolicyConfig {
    /// Whether to enforce policy rules. When false, all calls are allowed.
    #[serde(default)]
    pub enabled: bool,
    /// Fallback effect when no rule matches.
    #[serde(default = "default_deny")]
    pub default_effect: DefaultEffect,
    /// Inline policy rules.
    #[serde(default)]
    pub rules: Vec<PolicyRuleConfig>,
    /// Optional external policy file (TOML). When set, overrides inline rules.
    pub policy_file: Option<String>,
}

/// A single policy rule as read from TOML.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PolicyRuleConfig {
    /// Effect when the rule matches.
    pub effect: PolicyEffect,
    /// Glob pattern matching the tool id. Required.
    pub tool: String,
    /// Path globs matched against path-like params. Rule fires if ANY path matches.
    #[serde(default)]
    pub paths: Vec<String>,
    /// Env var names that must all be present in the policy context.
    #[serde(default)]
    pub env: Vec<String>,
    /// Minimum required trust level (rule fires only when context trust <= threshold).
    pub trust_level: Option<SkillTrustLevel>,
    /// Regex matched against individual string param values.
    pub args_match: Option<String>,
    /// Named capabilities associated with this rule.
    #[serde(default)]
    pub capabilities: Vec<String>,
}

// ── Sandbox ──────────────────────────────────────────────────────────────────

/// Baseline restriction profile for the OS-level sandbox.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxProfile {
    /// Read-only to `allow_read` paths, no writes, no network.
    ReadOnly,
    /// Read/write to configured paths; network egress blocked.
    #[default]
    Workspace,
    /// Workspace-level filesystem access plus unrestricted network egress.
    #[serde(rename = "network-allow-all", alias = "network")]
    NetworkAllowAll,
    /// Sandbox disabled. The subprocess inherits the parent's full capabilities.
    Off,
}

fn default_sandbox_profile() -> SandboxProfile {
    SandboxProfile::Workspace
}

fn default_sandbox_backend() -> String {
    "auto".into()
}

/// OS-level subprocess sandbox configuration (`[tools.sandbox]` TOML section).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SandboxConfig {
    /// Enable OS-level sandbox. Default: `false`.
    #[serde(default)]
    pub enabled: bool,
    /// Enforcement profile controlling the baseline restrictions.
    #[serde(default = "default_sandbox_profile")]
    pub profile: SandboxProfile,
    /// Additional paths granted read access.
    #[serde(default)]
    pub allow_read: Vec<PathBuf>,
    /// Additional paths granted write access.
    #[serde(default)]
    pub allow_write: Vec<PathBuf>,
    /// When `true`, sandbox initialization failure aborts startup (fail-closed). Default: `true`.
    #[serde(default = "default_true")]
    pub strict: bool,
    /// OS backend hint: `"auto"` / `"seatbelt"` / `"landlock-bwrap"` / `"noop"`.
    #[serde(default = "default_sandbox_backend")]
    pub backend: String,
    /// Hostnames denied network egress from sandboxed subprocesses.
    #[serde(default)]
    pub denied_domains: Vec<String>,
    /// When `true`, failure to activate an effective OS sandbox aborts startup.
    #[serde(default)]
    pub fail_if_unavailable: bool,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            profile: default_sandbox_profile(),
            allow_read: Vec::new(),
            allow_write: Vec::new(),
            strict: true,
            backend: default_sandbox_backend(),
            denied_domains: Vec::new(),
            fail_if_unavailable: false,
        }
    }
}

// ── Output filter config ─────────────────────────────────────────────────────

/// Configuration for tool output security filter.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SecurityFilterConfig {
    /// Enable security filtering. Default: `true`.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Additional regex patterns to block in tool output.
    #[serde(default)]
    pub extra_patterns: Vec<String>,
}

impl Default for SecurityFilterConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            extra_patterns: Vec::new(),
        }
    }
}

/// Configuration for output filters.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FilterConfig {
    /// Master switch for output filtering. Default: `true`.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Security filter settings.
    #[serde(default)]
    pub security: SecurityFilterConfig,
    /// Directory containing a `filters.toml` override file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filters_path: Option<PathBuf>,
}

impl Default for FilterConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            security: SecurityFilterConfig::default(),
            filters_path: None,
        }
    }
}

// ── ToolsConfig sub-types ────────────────────────────────────────────────────

fn default_overflow_threshold() -> usize {
    50_000
}

fn default_retention_days() -> u64 {
    7
}

fn default_max_overflow_bytes() -> usize {
    10 * 1024 * 1024
}

/// Configuration for large tool response offload to `SQLite`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OverflowConfig {
    /// Character threshold above which tool output is offloaded. Default: `50000`.
    #[serde(default = "default_overflow_threshold")]
    pub threshold: usize,
    /// Days to retain offloaded entries. Default: `7`.
    #[serde(default = "default_retention_days")]
    pub retention_days: u64,
    /// Maximum bytes per overflow entry. `0` means unlimited. Default: `10 MiB`.
    #[serde(default = "default_max_overflow_bytes")]
    pub max_overflow_bytes: usize,
}

impl Default for OverflowConfig {
    fn default() -> Self {
        Self {
            threshold: default_overflow_threshold(),
            retention_days: default_retention_days(),
            max_overflow_bytes: default_max_overflow_bytes(),
        }
    }
}

fn default_anomaly_window() -> usize {
    10
}

fn default_anomaly_error_threshold() -> f64 {
    0.5
}

fn default_anomaly_critical_threshold() -> f64 {
    0.8
}

/// Configuration for the sliding-window anomaly detector.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AnomalyConfig {
    /// Enable the anomaly detector. Default: `true`.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Number of recent tool calls in the sliding window. Default: `10`.
    #[serde(default = "default_anomaly_window")]
    pub window_size: usize,
    /// Error-rate fraction triggering a WARN. Default: `0.5`.
    #[serde(default = "default_anomaly_error_threshold")]
    pub error_threshold: f64,
    /// Error-rate fraction triggering a CRIT. Default: `0.8`.
    #[serde(default = "default_anomaly_critical_threshold")]
    pub critical_threshold: f64,
    /// Emit a WARN when a reasoning model produces a quality failure. Default: `true`.
    #[serde(default = "default_true")]
    pub reasoning_model_warning: bool,
}

impl Default for AnomalyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            window_size: default_anomaly_window(),
            error_threshold: default_anomaly_error_threshold(),
            critical_threshold: default_anomaly_critical_threshold(),
            reasoning_model_warning: true,
        }
    }
}

fn default_cache_ttl_secs() -> u64 {
    300
}

/// Configuration for the tool result cache.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ResultCacheConfig {
    /// Whether caching is enabled. Default: `true`.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Time-to-live in seconds. `0` means entries never expire. Default: `300`.
    #[serde(default = "default_cache_ttl_secs")]
    pub ttl_secs: u64,
}

impl Default for ResultCacheConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            ttl_secs: default_cache_ttl_secs(),
        }
    }
}

fn default_tafc_complexity_threshold() -> f64 {
    0.6
}

/// Configuration for Think-Augmented Function Calling (TAFC).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TafcConfig {
    /// Enable TAFC schema augmentation. Default: `false`.
    #[serde(default)]
    pub enabled: bool,
    /// Complexity threshold tau in [0.0, 1.0]; tools >= tau are augmented. Default: `0.6`.
    #[serde(default = "default_tafc_complexity_threshold")]
    pub complexity_threshold: f64,
}

impl Default for TafcConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            complexity_threshold: default_tafc_complexity_threshold(),
        }
    }
}

impl TafcConfig {
    /// Validate and clamp `complexity_threshold` to [0.0, 1.0]. Resets NaN/Infinity to 0.6.
    #[must_use]
    pub fn validated(mut self) -> Self {
        if self.complexity_threshold.is_finite() {
            self.complexity_threshold = self.complexity_threshold.clamp(0.0, 1.0);
        } else {
            self.complexity_threshold = 0.6;
        }
        self
    }
}

fn default_utility_exempt_tools() -> Vec<String> {
    vec!["invoke_skill".to_string(), "load_skill".to_string()]
}

fn default_utility_threshold() -> f32 {
    0.1
}

fn default_utility_gain_weight() -> f32 {
    1.0
}

fn default_utility_cost_weight() -> f32 {
    0.5
}

fn default_utility_redundancy_weight() -> f32 {
    0.3
}

fn default_utility_uncertainty_bonus() -> f32 {
    0.2
}

/// Configuration for utility-guided tool dispatch.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct UtilityScoringConfig {
    /// Enable utility-guided gating. Default: `false`.
    pub enabled: bool,
    /// Minimum utility score required to execute a tool call. Default: `0.1`.
    #[serde(default = "default_utility_threshold")]
    pub threshold: f32,
    /// Weight for the estimated gain component. Must be >= 0. Default: `1.0`.
    #[serde(default = "default_utility_gain_weight")]
    pub gain_weight: f32,
    /// Weight for the step cost component. Must be >= 0. Default: `0.5`.
    #[serde(default = "default_utility_cost_weight")]
    pub cost_weight: f32,
    /// Weight for the redundancy penalty. Must be >= 0. Default: `0.3`.
    #[serde(default = "default_utility_redundancy_weight")]
    pub redundancy_weight: f32,
    /// Weight for the exploration bonus. Must be >= 0. Default: `0.2`.
    #[serde(default = "default_utility_uncertainty_bonus")]
    pub uncertainty_bonus: f32,
    /// Tool names that bypass the utility gate unconditionally.
    #[serde(default = "default_utility_exempt_tools")]
    pub exempt_tools: Vec<String>,
}

impl Default for UtilityScoringConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            threshold: default_utility_threshold(),
            gain_weight: default_utility_gain_weight(),
            cost_weight: default_utility_cost_weight(),
            redundancy_weight: default_utility_redundancy_weight(),
            uncertainty_bonus: default_utility_uncertainty_bonus(),
            exempt_tools: default_utility_exempt_tools(),
        }
    }
}

impl UtilityScoringConfig {
    /// Validate that all weights and threshold are non-negative and finite.
    ///
    /// # Errors
    ///
    /// Returns a description of the first invalid field found.
    pub fn validate(&self) -> Result<(), String> {
        let fields = [
            ("threshold", self.threshold),
            ("gain_weight", self.gain_weight),
            ("cost_weight", self.cost_weight),
            ("redundancy_weight", self.redundancy_weight),
            ("uncertainty_bonus", self.uncertainty_bonus),
        ];
        for (name, val) in fields {
            if !val.is_finite() {
                return Err(format!("[tools.utility] {name} must be finite, got {val}"));
            }
            if val < 0.0 {
                return Err(format!("[tools.utility] {name} must be >= 0, got {val}"));
            }
        }
        Ok(())
    }
}

/// Dependency specification for a single tool.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ToolDependency {
    /// Hard prerequisites: tool is hidden until ALL of these have completed successfully.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires: Vec<String>,
    /// Soft prerequisites: tool gets a similarity boost when these have completed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prefers: Vec<String>,
}

fn default_boost_per_dep() -> f32 {
    0.15
}

fn default_max_total_boost() -> f32 {
    0.2
}

/// Configuration for the tool dependency graph feature.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DependencyConfig {
    /// Whether dependency gating is enabled. Default: `false`.
    #[serde(default)]
    pub enabled: bool,
    /// Similarity boost added per satisfied `prefers` dependency. Default: `0.15`.
    #[serde(default = "default_boost_per_dep")]
    pub boost_per_dep: f32,
    /// Maximum total boost applied regardless of how many `prefers` deps are met. Default: `0.2`.
    #[serde(default = "default_max_total_boost")]
    pub max_total_boost: f32,
    /// Per-tool dependency rules. Key is `tool_id`.
    #[serde(default)]
    pub rules: HashMap<String, ToolDependency>,
}

impl Default for DependencyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            boost_per_dep: default_boost_per_dep(),
            max_total_boost: default_max_total_boost(),
            rules: HashMap::new(),
        }
    }
}

fn default_retry_max_attempts() -> usize {
    2
}

fn default_retry_base_ms() -> u64 {
    500
}

fn default_retry_max_ms() -> u64 {
    5_000
}

fn default_retry_budget_secs() -> u64 {
    30
}

/// Configuration for tool error retry behavior.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RetryConfig {
    /// Maximum retry attempts for transient errors per tool call. `0` = disabled.
    #[serde(default = "default_retry_max_attempts")]
    pub max_attempts: usize,
    /// Base delay (ms) for exponential backoff.
    #[serde(default = "default_retry_base_ms")]
    pub base_ms: u64,
    /// Maximum delay cap (ms) for exponential backoff.
    #[serde(default = "default_retry_max_ms")]
    pub max_ms: u64,
    /// Maximum wall-clock time (seconds) for all retries of a single tool call. `0` = unlimited.
    #[serde(default = "default_retry_budget_secs")]
    pub budget_secs: u64,
    /// Provider name for LLM-based parameter reformatting on `InvalidParameters`/`TypeMismatch`.
    /// Empty string = disabled.
    #[serde(default)]
    pub parameter_reformat_provider: ProviderName,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: default_retry_max_attempts(),
            base_ms: default_retry_base_ms(),
            max_ms: default_retry_max_ms(),
            budget_secs: default_retry_budget_secs(),
            parameter_reformat_provider: ProviderName::default(),
        }
    }
}

fn default_adversarial_timeout_ms() -> u64 {
    3_000
}

/// Configuration for the LLM-based adversarial policy agent.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AdversarialPolicyConfig {
    /// Enable the adversarial policy agent. Default: `false`.
    #[serde(default)]
    pub enabled: bool,
    /// Provider name for the policy validation LLM.
    #[serde(default)]
    pub policy_provider: ProviderName,
    /// Path to a plain-text policy file.
    pub policy_file: Option<String>,
    /// Whether to allow tool calls when the policy LLM fails. Default: `false` (fail-closed).
    #[serde(default)]
    pub fail_open: bool,
    /// Timeout in milliseconds for a single policy LLM call. Default: `3000`.
    #[serde(default = "default_adversarial_timeout_ms")]
    pub timeout_ms: u64,
    /// Tool names always allowed through the adversarial policy gate.
    #[serde(default = "AdversarialPolicyConfig::default_exempt_tools")]
    pub exempt_tools: Vec<String>,
}

impl Default for AdversarialPolicyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            policy_provider: ProviderName::default(),
            policy_file: None,
            fail_open: false,
            timeout_ms: default_adversarial_timeout_ms(),
            exempt_tools: Self::default_exempt_tools(),
        }
    }
}

impl AdversarialPolicyConfig {
    #[must_use]
    pub fn default_exempt_tools() -> Vec<String> {
        vec![
            "memory_save".into(),
            "memory_search".into(),
            "read_overflow".into(),
            "load_skill".into(),
            "invoke_skill".into(),
            "schedule_deferred".into(),
        ]
    }
}

/// Per-path read allow/deny sandbox for the file tool.
///
/// Evaluation order: deny-then-allow. If a path matches `deny_read` and does NOT
/// match `allow_read`, access is denied. Empty `deny_read` means no read restrictions.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct FileConfig {
    /// Glob patterns for paths denied for reading. Evaluated first.
    #[serde(default)]
    pub deny_read: Vec<String>,
    /// Glob patterns for paths allowed for reading. Evaluated second (overrides deny).
    #[serde(default)]
    pub allow_read: Vec<String>,
}

/// OAP-style declarative authorization config.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct AuthorizationConfig {
    /// Enable OAP authorization checks. Default: `false`.
    #[serde(default)]
    pub enabled: bool,
    /// Per-tool authorization rules appended after `[tools.policy]` rules at startup.
    #[serde(default)]
    pub rules: Vec<PolicyRuleConfig>,
}

/// Configuration for audit logging of tool executions.
#[derive(Debug, Deserialize, Serialize)]
pub struct AuditConfig {
    /// Enable audit logging. Default: `true`.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Log destination: `"stdout"`, `"stderr"`, or a file path. Default: `"stdout"`.
    #[serde(default = "default_audit_destination")]
    pub destination: String,
    /// When `true`, log a per-tool risk summary at startup. Default: `false`.
    #[serde(default)]
    pub tool_risk_summary: bool,
}

fn default_audit_destination() -> String {
    "stdout".into()
}

impl Default for AuditConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            destination: default_audit_destination(),
            tool_risk_summary: false,
        }
    }
}

fn default_timeout() -> u64 {
    30
}

fn default_confirm_patterns() -> Vec<String> {
    vec![
        "rm ".into(),
        "git push -f".into(),
        "git push --force".into(),
        "drop table".into(),
        "drop database".into(),
        "truncate ".into(),
        "$(".into(),
        "`".into(),
        "<(".into(),
        ">(".into(),
        "<<<".into(),
        "eval ".into(),
    ]
}

fn default_max_background_runs() -> usize {
    8
}

fn default_background_timeout_secs() -> u64 {
    1800
}

/// Shell-specific configuration: timeout, command blocklist, and allowlist overrides.
#[derive(Debug, Deserialize, Serialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct ShellConfig {
    /// Shell command timeout in seconds. Default: `30`.
    #[serde(default = "default_timeout")]
    pub timeout: u64,
    /// Commands blocked from execution.
    #[serde(default)]
    pub blocked_commands: Vec<String>,
    /// Commands explicitly allowed (overrides blocklist).
    #[serde(default)]
    pub allowed_commands: Vec<String>,
    /// Filesystem paths the shell is permitted to access.
    #[serde(default)]
    pub allowed_paths: Vec<String>,
    /// Allow outbound network from shell. Default: `true`.
    #[serde(default = "default_true")]
    pub allow_network: bool,
    /// Patterns that trigger a confirmation prompt before execution.
    #[serde(default = "default_confirm_patterns")]
    pub confirm_patterns: Vec<String>,
    /// Environment variable name prefixes to strip from subprocess environment.
    #[serde(default = "ShellConfig::default_env_blocklist")]
    pub env_blocklist: Vec<String>,
    /// Enable transactional mode: snapshot files before write commands. Default: `false`.
    #[serde(default)]
    pub transactional: bool,
    /// Glob patterns for paths eligible for snapshotting.
    #[serde(default)]
    pub transaction_scope: Vec<String>,
    /// Automatically rollback when exit code >= 2. Default: `false`.
    #[serde(default)]
    pub auto_rollback: bool,
    /// Exit codes that trigger auto-rollback.
    #[serde(default)]
    pub auto_rollback_exit_codes: Vec<i32>,
    /// When `true`, snapshot failure aborts execution. Default: `false`.
    #[serde(default)]
    pub snapshot_required: bool,
    /// Maximum cumulative bytes for transaction snapshots. `0` = unlimited.
    #[serde(default)]
    pub max_snapshot_bytes: u64,
    /// Maximum concurrent background shell runs. Default: `8`.
    #[serde(default = "default_max_background_runs")]
    pub max_background_runs: usize,
    /// Timeout in seconds for each background shell run. Default: `1800`.
    #[serde(default = "default_background_timeout_secs")]
    pub background_timeout_secs: u64,
}

impl Default for ShellConfig {
    fn default() -> Self {
        Self {
            timeout: default_timeout(),
            blocked_commands: Vec::new(),
            allowed_commands: Vec::new(),
            allowed_paths: Vec::new(),
            allow_network: true,
            confirm_patterns: default_confirm_patterns(),
            env_blocklist: Self::default_env_blocklist(),
            transactional: false,
            transaction_scope: Vec::new(),
            auto_rollback: false,
            auto_rollback_exit_codes: Vec::new(),
            snapshot_required: false,
            max_snapshot_bytes: 0,
            max_background_runs: default_max_background_runs(),
            background_timeout_secs: default_background_timeout_secs(),
        }
    }
}

impl ShellConfig {
    /// Default environment variable prefixes to strip from subprocess environment.
    #[must_use]
    pub fn default_env_blocklist() -> Vec<String> {
        vec![
            "ZEPH_".into(),
            "AWS_".into(),
            "AZURE_".into(),
            "GCP_".into(),
            "GOOGLE_".into(),
            "OPENAI_".into(),
            "ANTHROPIC_".into(),
            "HF_".into(),
            "HUGGING".into(),
        ]
    }
}

fn default_scrape_timeout() -> u64 {
    15
}

fn default_max_body_bytes() -> usize {
    4_194_304
}

/// Configuration for the web scrape tool.
#[derive(Debug, Deserialize, Serialize)]
pub struct ScrapeConfig {
    /// Timeout in seconds for scrape requests. Default: `15`.
    #[serde(default = "default_scrape_timeout")]
    pub timeout: u64,
    /// Maximum response body bytes. Default: `4 MiB`.
    #[serde(default = "default_max_body_bytes")]
    pub max_body_bytes: usize,
    /// Domain allowlist. Empty = all public domains allowed.
    #[serde(default)]
    pub allowed_domains: Vec<String>,
    /// Domain denylist. Always enforced, regardless of allowlist state.
    #[serde(default)]
    pub denied_domains: Vec<String>,
}

impl Default for ScrapeConfig {
    fn default() -> Self {
        Self {
            timeout: default_scrape_timeout(),
            max_body_bytes: default_max_body_bytes(),
            allowed_domains: Vec::new(),
            denied_domains: Vec::new(),
        }
    }
}

/// Speculative tool execution mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SpeculationMode {
    /// No speculation; uses existing synchronous path.
    #[default]
    Off,
    /// LLM-decoding level: fires tools when streaming partial JSON has all required fields.
    Decoding,
    /// Application-level pattern (PASTE): predicts top-K calls from `SQLite` history.
    Pattern,
    /// Both decoding and pattern speculation active.
    Both,
}

/// Pattern-based (PASTE) speculative execution config.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SpeculativePatternConfig {
    /// Enable PASTE pattern learning and prediction. Default: `false`.
    #[serde(default)]
    pub enabled: bool,
    /// Minimum observed occurrences before a prediction is issued.
    #[serde(default = "default_min_observations")]
    pub min_observations: u32,
    /// Exponential decay half-life in days for pattern scoring.
    #[serde(default = "default_half_life_days")]
    pub half_life_days: f64,
    /// LLM provider name for optional reranking. Empty = disabled.
    #[serde(default)]
    pub rerank_provider: ProviderName,
}

fn default_min_observations() -> u32 {
    5
}

fn default_half_life_days() -> f64 {
    14.0
}

impl Default for SpeculativePatternConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            min_observations: default_min_observations(),
            half_life_days: default_half_life_days(),
            rerank_provider: ProviderName::default(),
        }
    }
}

/// Shell command regex allowlist for speculative execution.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct SpeculativeAllowlistConfig {
    /// Regexes matched against the full `bash` command string.
    #[serde(default)]
    pub shell: Vec<String>,
}

fn default_max_in_flight() -> usize {
    4
}

fn default_confidence_threshold() -> f32 {
    0.55
}

fn default_max_wasted_per_minute() -> u64 {
    100
}

fn default_ttl_seconds() -> u64 {
    30
}

/// Top-level configuration for speculative tool execution.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SpeculativeConfig {
    /// Speculation mode. Default: `off`.
    #[serde(default)]
    pub mode: SpeculationMode,
    /// Maximum concurrent in-flight speculative tasks.
    #[serde(default = "default_max_in_flight")]
    pub max_in_flight: usize,
    /// Minimum confidence score [0, 1] to dispatch a speculative task.
    #[serde(default = "default_confidence_threshold")]
    pub confidence_threshold: f32,
    /// Circuit-breaker: disable speculation for 60 s when wasted ms exceeds this per minute.
    #[serde(default = "default_max_wasted_per_minute")]
    pub max_wasted_per_minute: u64,
    /// Per-handle wall-clock TTL in seconds before the handle is cancelled.
    #[serde(default = "default_ttl_seconds")]
    pub ttl_seconds: u64,
    /// Emit `AuditEntry` for speculative dispatches. Default: `true`.
    #[serde(default = "default_true")]
    pub audit: bool,
    /// PASTE pattern learning config.
    #[serde(default)]
    pub pattern: SpeculativePatternConfig,
    /// Per-executor command allowlists.
    #[serde(default)]
    pub allowlist: SpeculativeAllowlistConfig,
}

impl Default for SpeculativeConfig {
    fn default() -> Self {
        Self {
            mode: SpeculationMode::Off,
            max_in_flight: default_max_in_flight(),
            confidence_threshold: default_confidence_threshold(),
            max_wasted_per_minute: default_max_wasted_per_minute(),
            ttl_seconds: default_ttl_seconds(),
            audit: true,
            pattern: SpeculativePatternConfig::default(),
            allowlist: SpeculativeAllowlistConfig::default(),
        }
    }
}

/// Configuration for egress network event logging.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
#[allow(clippy::struct_excessive_bools)]
pub struct EgressConfig {
    /// Master switch for egress event emission. Default: `true`.
    pub enabled: bool,
    /// Emit events for requests blocked by SSRF/domain/scheme checks. Default: `true`.
    pub log_blocked: bool,
    /// Include `response_bytes` in the JSONL record. Default: `true`.
    pub log_response_bytes: bool,
    /// Show real hostname in TUI egress panel. Default: `true`.
    pub log_hosts_to_tui: bool,
}

impl Default for EgressConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            log_blocked: true,
            log_response_bytes: true,
            log_hosts_to_tui: true,
        }
    }
}

// ── ToolCompressionConfig ─────────────────────────────────────────────────────

fn default_compression_min_lines() -> usize {
    10
}

fn default_compression_max_rules() -> u32 {
    200
}

fn default_regex_compile_timeout_ms() -> u64 {
    500
}

fn default_evolution_min_interval_secs() -> u64 {
    3600
}

/// TACO self-evolving tool output compression configuration (`[tools.compression]` TOML section).
///
/// When enabled, a `RuleBasedCompressor` is wrapped around the root tool executor.
/// Rules are loaded from the `compression_rules` `SQLite` table and optionally evolved by an
/// LLM provider specified in `evolution_provider`.
///
/// # Example (TOML)
///
/// ```toml
/// [tools.compression]
/// enabled = true
/// evolution_provider = "fast"
/// min_lines_to_compress = 15
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ToolCompressionConfig {
    /// Enable rule-based tool output compression. Default: `false`.
    pub enabled: bool,
    /// Minimum output line count before compression is attempted. Default: `10`.
    #[serde(default = "default_compression_min_lines")]
    pub min_lines_to_compress: usize,
    /// LLM provider name for self-evolution. Empty string = evolution disabled. Default: `""`.
    #[serde(default)]
    pub evolution_provider: ProviderName,
    /// Minimum interval in seconds between self-evolution runs. Default: `3600`.
    #[serde(default = "default_evolution_min_interval_secs")]
    pub evolution_min_interval_secs: u64,
    /// Maximum number of rules to keep in the DB (prune lowest-hit rules above this). Default: `200`.
    #[serde(default = "default_compression_max_rules")]
    pub max_rules: u32,
    /// Timeout in milliseconds for safe regex compilation. Default: `500`.
    #[serde(default = "default_regex_compile_timeout_ms")]
    pub regex_compile_timeout_ms: u64,
}

impl Default for ToolCompressionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            min_lines_to_compress: default_compression_min_lines(),
            evolution_provider: ProviderName::default(),
            evolution_min_interval_secs: default_evolution_min_interval_secs(),
            max_rules: default_compression_max_rules(),
            regex_compile_timeout_ms: default_regex_compile_timeout_ms(),
        }
    }
}

// ── ToolsConfig ───────────────────────────────────────────────────────────────

/// Top-level configuration for tool execution.
///
/// Deserialized from `[tools]` in TOML. The `permission_policy()` method (which constructs
/// a runtime `PermissionPolicy`) lives in `zeph-tools` as a free function to avoid
/// importing runtime types into this leaf crate.
#[derive(Debug, Deserialize, Serialize)]
pub struct ToolsConfig {
    /// Enable all tools. Default: `true`.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Summarize long tool output before injection into context. Default: `true`.
    #[serde(default = "default_true")]
    pub summarize_output: bool,
    /// Shell tool configuration.
    #[serde(default)]
    pub shell: ShellConfig,
    /// Web scrape tool configuration.
    #[serde(default)]
    pub scrape: ScrapeConfig,
    /// Audit log configuration.
    #[serde(default)]
    pub audit: AuditConfig,
    /// Declarative permissions. Overrides legacy `shell.blocked_commands` when set.
    #[serde(default)]
    pub permissions: Option<PermissionsConfig>,
    /// Output filter configuration.
    #[serde(default)]
    pub filters: FilterConfig,
    /// Large response offload configuration.
    #[serde(default)]
    pub overflow: OverflowConfig,
    /// Sliding-window anomaly detector.
    #[serde(default)]
    pub anomaly: AnomalyConfig,
    /// Tool result cache.
    #[serde(default)]
    pub result_cache: ResultCacheConfig,
    /// Think-Augmented Function Calling.
    #[serde(default)]
    pub tafc: TafcConfig,
    /// Tool dependency graph.
    #[serde(default)]
    pub dependencies: DependencyConfig,
    /// Error retry configuration.
    #[serde(default)]
    pub retry: RetryConfig,
    /// Declarative policy compiler for tool call authorization.
    #[serde(default)]
    pub policy: PolicyConfig,
    /// LLM-based adversarial policy agent.
    #[serde(default)]
    pub adversarial_policy: AdversarialPolicyConfig,
    /// Utility-guided tool dispatch gate.
    #[serde(default)]
    pub utility: UtilityScoringConfig,
    /// Per-path read allow/deny sandbox for the file tool.
    #[serde(default)]
    pub file: FileConfig,
    /// OAP declarative pre-action authorization.
    #[serde(default)]
    pub authorization: AuthorizationConfig,
    /// Maximum tool calls allowed per agent session. `None` = unlimited.
    #[serde(default)]
    pub max_tool_calls_per_session: Option<u32>,
    /// Speculative tool execution configuration.
    #[serde(default)]
    pub speculative: SpeculativeConfig,
    /// OS-level subprocess sandbox configuration.
    #[serde(default)]
    pub sandbox: SandboxConfig,
    /// Egress network event logging configuration.
    #[serde(default)]
    pub egress: EgressConfig,
    /// TACO self-evolving tool output compression configuration.
    #[serde(default)]
    pub compression: ToolCompressionConfig,
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            summarize_output: true,
            shell: ShellConfig::default(),
            scrape: ScrapeConfig::default(),
            audit: AuditConfig::default(),
            permissions: None,
            filters: FilterConfig::default(),
            overflow: OverflowConfig::default(),
            anomaly: AnomalyConfig::default(),
            result_cache: ResultCacheConfig::default(),
            tafc: TafcConfig::default(),
            dependencies: DependencyConfig::default(),
            retry: RetryConfig::default(),
            policy: PolicyConfig::default(),
            adversarial_policy: AdversarialPolicyConfig::default(),
            utility: UtilityScoringConfig::default(),
            file: FileConfig::default(),
            authorization: AuthorizationConfig::default(),
            max_tool_calls_per_session: None,
            speculative: SpeculativeConfig::default(),
            sandbox: SandboxConfig::default(),
            egress: EgressConfig::default(),
            compression: ToolCompressionConfig::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_default_config() {
        let toml_str = r#"
            enabled = true

            [shell]
            timeout = 60
            blocked_commands = ["rm -rf /", "sudo"]
        "#;

        let config: ToolsConfig = toml::from_str(toml_str).unwrap();
        assert!(config.enabled);
        assert_eq!(config.shell.timeout, 60);
        assert_eq!(config.shell.blocked_commands.len(), 2);
    }

    #[test]
    fn empty_blocked_commands() {
        let config: ToolsConfig = toml::from_str(r"[shell]\ntimeout = 30\n").unwrap_or_default();
        assert!(config.enabled);
    }

    #[test]
    fn default_tools_config() {
        let config = ToolsConfig::default();
        assert!(config.enabled);
        assert!(config.summarize_output);
        assert_eq!(config.shell.timeout, 30);
        assert!(config.shell.blocked_commands.is_empty());
        assert!(config.audit.enabled);
    }
}
