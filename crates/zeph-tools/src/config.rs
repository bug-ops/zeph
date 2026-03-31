// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use serde::{Deserialize, Serialize};

use crate::permissions::{AutonomyLevel, PermissionPolicy, PermissionsConfig};
#[cfg(feature = "policy-enforcer")]
use crate::policy::PolicyConfig;

fn default_true() -> bool {
    true
}

#[cfg(feature = "policy-enforcer")]
fn default_adversarial_timeout_ms() -> u64 {
    3_000
}

fn default_timeout() -> u64 {
    30
}

fn default_cache_ttl_secs() -> u64 {
    300
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

fn default_audit_destination() -> String {
    "stdout".into()
}

fn default_overflow_threshold() -> usize {
    50_000
}

fn default_retention_days() -> u64 {
    7
}

fn default_max_overflow_bytes() -> usize {
    10 * 1024 * 1024 // 10 MiB
}

/// Configuration for large tool response offload to `SQLite`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OverflowConfig {
    #[serde(default = "default_overflow_threshold")]
    pub threshold: usize,
    #[serde(default = "default_retention_days")]
    pub retention_days: u64,
    /// Maximum bytes per overflow entry. `0` means unlimited.
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
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_anomaly_window")]
    pub window_size: usize,
    #[serde(default = "default_anomaly_error_threshold")]
    pub error_threshold: f64,
    #[serde(default = "default_anomaly_critical_threshold")]
    pub critical_threshold: f64,
    /// Emit a WARN log when a reasoning-enhanced model (o1, o3, `QwQ`, etc.) produces
    /// a quality failure (`ToolNotFound`, `InvalidParameters`, `TypeMismatch`). Default: `true`.
    ///
    /// Based on arXiv:2510.22977 — CoT/RL reasoning amplifies tool hallucination.
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
    /// Enable TAFC schema augmentation (default: false).
    #[serde(default)]
    pub enabled: bool,
    /// Complexity threshold tau in [0.0, 1.0]; tools with complexity >= tau are augmented.
    /// Default: 0.6
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
    /// Validate and clamp `complexity_threshold` to \[0.0, 1.0\]. Reset NaN/Infinity to 0.6.
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

/// Configuration for utility-guided tool dispatch (`[tools.utility]` TOML section).
///
/// Implements the utility gate from arXiv:2603.19896: each tool call is scored
/// `U = gain_weight*gain - cost_weight*cost - redundancy_weight*redundancy + uncertainty_bonus*uncertainty`.
/// Calls with `U < threshold` are skipped (fail-closed on scoring errors).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct UtilityScoringConfig {
    /// Enable utility-guided gating. Default: false (opt-in).
    pub enabled: bool,
    /// Minimum utility score required to execute a tool call. Default: 0.1.
    #[serde(default = "default_utility_threshold")]
    pub threshold: f32,
    /// Weight for the estimated gain component. Must be >= 0. Default: 1.0.
    #[serde(default = "default_utility_gain_weight")]
    pub gain_weight: f32,
    /// Weight for the step cost component. Must be >= 0. Default: 0.5.
    #[serde(default = "default_utility_cost_weight")]
    pub cost_weight: f32,
    /// Weight for the redundancy penalty. Must be >= 0. Default: 0.3.
    #[serde(default = "default_utility_redundancy_weight")]
    pub redundancy_weight: f32,
    /// Weight for the exploration bonus. Must be >= 0. Default: 0.2.
    #[serde(default = "default_utility_uncertainty_bonus")]
    pub uncertainty_bonus: f32,
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

fn default_boost_per_dep() -> f32 {
    0.15
}

fn default_max_total_boost() -> f32 {
    0.2
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

/// Configuration for the tool dependency graph feature.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DependencyConfig {
    /// Whether dependency gating is enabled. Default: false.
    #[serde(default)]
    pub enabled: bool,
    /// Similarity boost added per satisfied `prefers` dependency. Default: 0.15.
    #[serde(default = "default_boost_per_dep")]
    pub boost_per_dep: f32,
    /// Maximum total boost applied regardless of how many `prefers` deps are met. Default: 0.2.
    #[serde(default = "default_max_total_boost")]
    pub max_total_boost: f32,
    /// Per-tool dependency rules. Key is `tool_id`.
    #[serde(default)]
    pub rules: std::collections::HashMap<String, ToolDependency>,
}

impl Default for DependencyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            boost_per_dep: default_boost_per_dep(),
            max_total_boost: default_max_total_boost(),
            rules: std::collections::HashMap::new(),
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
    /// Maximum retry attempts for transient errors per tool call. 0 = disabled.
    #[serde(default = "default_retry_max_attempts")]
    pub max_attempts: usize,
    /// Base delay (ms) for exponential backoff.
    #[serde(default = "default_retry_base_ms")]
    pub base_ms: u64,
    /// Maximum delay cap (ms) for exponential backoff.
    #[serde(default = "default_retry_max_ms")]
    pub max_ms: u64,
    /// Maximum wall-clock time (seconds) for all retries of a single tool call. 0 = unlimited.
    #[serde(default = "default_retry_budget_secs")]
    pub budget_secs: u64,
    /// Provider name from `[[llm.providers]]` for LLM-based parameter reformatting on
    /// `InvalidParameters`/`TypeMismatch` errors. Empty string = disabled.
    #[serde(default)]
    pub parameter_reformat_provider: String,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: default_retry_max_attempts(),
            base_ms: default_retry_base_ms(),
            max_ms: default_retry_max_ms(),
            budget_secs: default_retry_budget_secs(),
            parameter_reformat_provider: String::new(),
        }
    }
}

/// Configuration for the LLM-based adversarial policy agent.
#[cfg(feature = "policy-enforcer")]
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AdversarialPolicyConfig {
    /// Enable the adversarial policy agent. Default: `false`.
    #[serde(default)]
    pub enabled: bool,
    /// Provider name from `[[llm.providers]]` for the policy validation LLM.
    /// Should reference a fast, cheap model (e.g. `gpt-4o-mini`).
    /// Empty string = fall back to the default provider.
    #[serde(default)]
    pub policy_provider: String,
    /// Path to a plain-text policy file. Each non-empty, non-comment line is one policy.
    pub policy_file: Option<String>,
    /// Whether to allow tool calls when the policy LLM fails (timeout/error).
    /// Default: `false` (fail-closed / deny on error).
    ///
    /// Setting this to `true` trades security for availability. Use only in
    /// deployments where the declarative `PolicyEnforcer` already covers hard rules.
    #[serde(default)]
    pub fail_open: bool,
    /// Timeout in milliseconds for a single policy LLM call. Default: 3000.
    #[serde(default = "default_adversarial_timeout_ms")]
    pub timeout_ms: u64,
    /// Tool names that are always allowed through the adversarial policy gate,
    /// regardless of policy content. Covers internal agent operations that are
    /// not externally visible side effects.
    #[serde(default = "AdversarialPolicyConfig::default_exempt_tools")]
    pub exempt_tools: Vec<String>,
}

#[cfg(feature = "policy-enforcer")]
impl Default for AdversarialPolicyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            policy_provider: String::new(),
            policy_file: None,
            fail_open: false,
            timeout_ms: default_adversarial_timeout_ms(),
            exempt_tools: Self::default_exempt_tools(),
        }
    }
}

#[cfg(feature = "policy-enforcer")]
impl AdversarialPolicyConfig {
    fn default_exempt_tools() -> Vec<String> {
        vec![
            "memory_save".into(),
            "memory_search".into(),
            "read_overflow".into(),
            "load_skill".into(),
            "schedule_deferred".into(),
        ]
    }
}

/// Per-path read allow/deny sandbox for the file tool.
///
/// Evaluation order: deny-then-allow. If a path matches `deny_read` and does NOT
/// match `allow_read`, access is denied. Empty `deny_read` means no read restrictions.
///
/// All patterns are matched against the canonicalized (absolute, symlink-resolved) path.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct FileConfig {
    /// Glob patterns for paths denied for reading. Evaluated first.
    #[serde(default)]
    pub deny_read: Vec<String>,
    /// Glob patterns for paths allowed for reading. Evaluated second (overrides deny).
    #[serde(default)]
    pub allow_read: Vec<String>,
}

/// Top-level configuration for tool execution.
#[derive(Debug, Deserialize, Serialize)]
pub struct ToolsConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub summarize_output: bool,
    #[serde(default)]
    pub shell: ShellConfig,
    #[serde(default)]
    pub scrape: ScrapeConfig,
    #[serde(default)]
    pub audit: AuditConfig,
    #[serde(default)]
    pub permissions: Option<PermissionsConfig>,
    #[serde(default)]
    pub filters: crate::filter::FilterConfig,
    #[serde(default)]
    pub overflow: OverflowConfig,
    #[serde(default)]
    pub anomaly: AnomalyConfig,
    #[serde(default)]
    pub result_cache: ResultCacheConfig,
    #[serde(default)]
    pub tafc: TafcConfig,
    #[serde(default)]
    pub dependencies: DependencyConfig,
    #[serde(default)]
    pub retry: RetryConfig,
    /// Declarative policy compiler for tool call authorization.
    #[cfg(feature = "policy-enforcer")]
    #[serde(default)]
    pub policy: PolicyConfig,
    /// LLM-based adversarial policy agent for natural-language policy enforcement.
    #[cfg(feature = "policy-enforcer")]
    #[serde(default)]
    pub adversarial_policy: AdversarialPolicyConfig,
    /// Utility-guided tool dispatch gate.
    #[serde(default)]
    pub utility: UtilityScoringConfig,
    /// Per-path read allow/deny sandbox for the file tool.
    #[serde(default)]
    pub file: FileConfig,
}

impl ToolsConfig {
    /// Build a `PermissionPolicy` from explicit config or legacy shell fields.
    #[must_use]
    pub fn permission_policy(&self, autonomy_level: AutonomyLevel) -> PermissionPolicy {
        let policy = if let Some(ref perms) = self.permissions {
            PermissionPolicy::from(perms.clone())
        } else {
            PermissionPolicy::from_legacy(
                &self.shell.blocked_commands,
                &self.shell.confirm_patterns,
            )
        };
        policy.with_autonomy(autonomy_level)
    }
}

/// Shell-specific configuration: timeout, command blocklist, and allowlist overrides.
#[derive(Debug, Deserialize, Serialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct ShellConfig {
    #[serde(default = "default_timeout")]
    pub timeout: u64,
    #[serde(default)]
    pub blocked_commands: Vec<String>,
    #[serde(default)]
    pub allowed_commands: Vec<String>,
    #[serde(default)]
    pub allowed_paths: Vec<String>,
    #[serde(default = "default_true")]
    pub allow_network: bool,
    #[serde(default = "default_confirm_patterns")]
    pub confirm_patterns: Vec<String>,
    /// Environment variable name prefixes to strip from subprocess environment.
    /// Variables whose names start with any of these prefixes are removed before
    /// spawning shell commands. Default covers common credential naming conventions.
    #[serde(default = "ShellConfig::default_env_blocklist")]
    pub env_blocklist: Vec<String>,
    /// Enable transactional mode: snapshot files before write commands, rollback on failure.
    #[serde(default)]
    pub transactional: bool,
    /// Glob patterns defining which paths are eligible for snapshotting.
    /// Only files matching these patterns (relative to cwd) are captured.
    /// Empty = snapshot all files referenced in the command.
    #[serde(default)]
    pub transaction_scope: Vec<String>,
    /// Automatically rollback when exit code >= 2. Default: false.
    /// Exit code 1 is excluded because many tools (grep, diff, test) use it for
    /// non-error conditions.
    #[serde(default)]
    pub auto_rollback: bool,
    /// Exit codes that trigger auto-rollback. Default: empty (uses >= 2 heuristic).
    /// When non-empty, only these exact exit codes trigger rollback.
    #[serde(default)]
    pub auto_rollback_exit_codes: Vec<i32>,
    /// When true, snapshot failure aborts execution with an error.
    /// When false (default), snapshot failure emits a warning and execution proceeds.
    #[serde(default)]
    pub snapshot_required: bool,
    /// Maximum cumulative bytes for transaction snapshots. 0 = unlimited.
    #[serde(default)]
    pub max_snapshot_bytes: u64,
}

impl ShellConfig {
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

/// Configuration for audit logging of tool executions.
#[derive(Debug, Deserialize, Serialize)]
pub struct AuditConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_audit_destination")]
    pub destination: String,
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
            filters: crate::filter::FilterConfig::default(),
            overflow: OverflowConfig::default(),
            anomaly: AnomalyConfig::default(),
            result_cache: ResultCacheConfig::default(),
            tafc: TafcConfig::default(),
            dependencies: DependencyConfig::default(),
            retry: RetryConfig::default(),
            #[cfg(feature = "policy-enforcer")]
            policy: PolicyConfig::default(),
            #[cfg(feature = "policy-enforcer")]
            adversarial_policy: AdversarialPolicyConfig::default(),
            utility: UtilityScoringConfig::default(),
            file: FileConfig::default(),
        }
    }
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
        }
    }
}

impl Default for AuditConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            destination: default_audit_destination(),
        }
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
    #[serde(default = "default_scrape_timeout")]
    pub timeout: u64,
    #[serde(default = "default_max_body_bytes")]
    pub max_body_bytes: usize,
}

impl Default for ScrapeConfig {
    fn default() -> Self {
        Self {
            timeout: default_scrape_timeout(),
            max_body_bytes: default_max_body_bytes(),
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
        assert_eq!(config.shell.blocked_commands[0], "rm -rf /");
        assert_eq!(config.shell.blocked_commands[1], "sudo");
    }

    #[test]
    fn empty_blocked_commands() {
        let toml_str = r"
            [shell]
            timeout = 30
        ";

        let config: ToolsConfig = toml::from_str(toml_str).unwrap();
        assert!(config.enabled);
        assert_eq!(config.shell.timeout, 30);
        assert!(config.shell.blocked_commands.is_empty());
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

    #[test]
    fn tools_summarize_output_default_true() {
        let config = ToolsConfig::default();
        assert!(config.summarize_output);
    }

    #[test]
    fn tools_summarize_output_parsing() {
        let toml_str = r"
            summarize_output = true
        ";
        let config: ToolsConfig = toml::from_str(toml_str).unwrap();
        assert!(config.summarize_output);
    }

    #[test]
    fn default_shell_config() {
        let config = ShellConfig::default();
        assert_eq!(config.timeout, 30);
        assert!(config.blocked_commands.is_empty());
        assert!(config.allowed_paths.is_empty());
        assert!(config.allow_network);
        assert!(!config.confirm_patterns.is_empty());
    }

    #[test]
    fn deserialize_omitted_fields_use_defaults() {
        let toml_str = "";
        let config: ToolsConfig = toml::from_str(toml_str).unwrap();
        assert!(config.enabled);
        assert_eq!(config.shell.timeout, 30);
        assert!(config.shell.blocked_commands.is_empty());
        assert!(config.shell.allow_network);
        assert!(!config.shell.confirm_patterns.is_empty());
        assert_eq!(config.scrape.timeout, 15);
        assert_eq!(config.scrape.max_body_bytes, 4_194_304);
        assert!(config.audit.enabled);
        assert_eq!(config.audit.destination, "stdout");
        assert!(config.summarize_output);
    }

    #[test]
    fn default_scrape_config() {
        let config = ScrapeConfig::default();
        assert_eq!(config.timeout, 15);
        assert_eq!(config.max_body_bytes, 4_194_304);
    }

    #[test]
    fn deserialize_scrape_config() {
        let toml_str = r"
            [scrape]
            timeout = 30
            max_body_bytes = 2097152
        ";

        let config: ToolsConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.scrape.timeout, 30);
        assert_eq!(config.scrape.max_body_bytes, 2_097_152);
    }

    #[test]
    fn tools_config_default_includes_scrape() {
        let config = ToolsConfig::default();
        assert_eq!(config.scrape.timeout, 15);
        assert_eq!(config.scrape.max_body_bytes, 4_194_304);
    }

    #[test]
    fn deserialize_allowed_commands() {
        let toml_str = r#"
            [shell]
            timeout = 30
            allowed_commands = ["curl", "wget"]
        "#;

        let config: ToolsConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.shell.allowed_commands, vec!["curl", "wget"]);
    }

    #[test]
    fn default_allowed_commands_empty() {
        let config = ShellConfig::default();
        assert!(config.allowed_commands.is_empty());
    }

    #[test]
    fn deserialize_shell_security_fields() {
        let toml_str = r#"
            [shell]
            allowed_paths = ["/tmp", "/home/user"]
            allow_network = false
            confirm_patterns = ["rm ", "drop table"]
        "#;

        let config: ToolsConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.shell.allowed_paths, vec!["/tmp", "/home/user"]);
        assert!(!config.shell.allow_network);
        assert_eq!(config.shell.confirm_patterns, vec!["rm ", "drop table"]);
    }

    #[test]
    fn deserialize_audit_config() {
        let toml_str = r#"
            [audit]
            enabled = true
            destination = "/var/log/zeph-audit.log"
        "#;

        let config: ToolsConfig = toml::from_str(toml_str).unwrap();
        assert!(config.audit.enabled);
        assert_eq!(config.audit.destination, "/var/log/zeph-audit.log");
    }

    #[test]
    fn default_audit_config() {
        let config = AuditConfig::default();
        assert!(config.enabled);
        assert_eq!(config.destination, "stdout");
    }

    #[test]
    fn permission_policy_from_legacy_fields() {
        let config = ToolsConfig {
            shell: ShellConfig {
                blocked_commands: vec!["sudo".to_owned()],
                confirm_patterns: vec!["rm ".to_owned()],
                ..ShellConfig::default()
            },
            ..ToolsConfig::default()
        };
        let policy = config.permission_policy(AutonomyLevel::Supervised);
        assert_eq!(
            policy.check("bash", "sudo apt"),
            crate::permissions::PermissionAction::Deny
        );
        assert_eq!(
            policy.check("bash", "rm file"),
            crate::permissions::PermissionAction::Ask
        );
    }

    #[test]
    fn permission_policy_from_explicit_config() {
        let toml_str = r#"
            [permissions]
            [[permissions.bash]]
            pattern = "*sudo*"
            action = "deny"
        "#;
        let config: ToolsConfig = toml::from_str(toml_str).unwrap();
        let policy = config.permission_policy(AutonomyLevel::Supervised);
        assert_eq!(
            policy.check("bash", "sudo rm"),
            crate::permissions::PermissionAction::Deny
        );
    }

    #[test]
    fn permission_policy_default_uses_legacy() {
        let config = ToolsConfig::default();
        assert!(config.permissions.is_none());
        let policy = config.permission_policy(AutonomyLevel::Supervised);
        // Default ShellConfig has confirm_patterns, so legacy rules are generated
        assert!(!config.shell.confirm_patterns.is_empty());
        assert!(policy.rules().contains_key("bash"));
    }

    #[test]
    fn deserialize_overflow_config_full() {
        let toml_str = r"
            [overflow]
            threshold = 100000
            retention_days = 14
            max_overflow_bytes = 5242880
        ";
        let config: ToolsConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.overflow.threshold, 100_000);
        assert_eq!(config.overflow.retention_days, 14);
        assert_eq!(config.overflow.max_overflow_bytes, 5_242_880);
    }

    #[test]
    fn deserialize_overflow_config_unknown_dir_field_is_ignored() {
        // Old configs with `dir = "..."` must not fail deserialization.
        let toml_str = r#"
            [overflow]
            threshold = 75000
            dir = "/tmp/overflow"
        "#;
        let config: ToolsConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.overflow.threshold, 75_000);
    }

    #[test]
    fn deserialize_overflow_config_partial_uses_defaults() {
        let toml_str = r"
            [overflow]
            threshold = 75000
        ";
        let config: ToolsConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.overflow.threshold, 75_000);
        assert_eq!(config.overflow.retention_days, 7);
    }

    #[test]
    fn deserialize_overflow_config_omitted_uses_defaults() {
        let config: ToolsConfig = toml::from_str("").unwrap();
        assert_eq!(config.overflow.threshold, 50_000);
        assert_eq!(config.overflow.retention_days, 7);
        assert_eq!(config.overflow.max_overflow_bytes, 10 * 1024 * 1024);
    }

    #[test]
    fn result_cache_config_defaults() {
        let config = ResultCacheConfig::default();
        assert!(config.enabled);
        assert_eq!(config.ttl_secs, 300);
    }

    #[test]
    fn deserialize_result_cache_config() {
        let toml_str = r"
            [result_cache]
            enabled = false
            ttl_secs = 60
        ";
        let config: ToolsConfig = toml::from_str(toml_str).unwrap();
        assert!(!config.result_cache.enabled);
        assert_eq!(config.result_cache.ttl_secs, 60);
    }

    #[test]
    fn result_cache_omitted_uses_defaults() {
        let config: ToolsConfig = toml::from_str("").unwrap();
        assert!(config.result_cache.enabled);
        assert_eq!(config.result_cache.ttl_secs, 300);
    }

    #[test]
    fn result_cache_ttl_zero_is_valid() {
        let toml_str = r"
            [result_cache]
            ttl_secs = 0
        ";
        let config: ToolsConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.result_cache.ttl_secs, 0);
    }
}
