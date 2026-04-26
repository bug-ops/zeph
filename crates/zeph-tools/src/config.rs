// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Tool configuration re-exports and runtime helpers.
//!
//! Pure-data configuration types are defined in `zeph-config` and re-exported here
//! so that existing import paths (e.g. `zeph_tools::ShellConfig`) continue to resolve.

pub(crate) use zeph_config::tools::{
    AuditConfig, EgressConfig, FileConfig, SandboxConfig, ScrapeConfig, ShellConfig,
    ToolDependency, ToolsConfig, UtilityScoringConfig,
};

use crate::domain_match;
use crate::permissions::{AutonomyLevel, PermissionPolicy};

/// Validate `denied_domains` entries in a [`SandboxConfig`].
///
/// Each entry must contain only alphanumeric characters, dots, hyphens, and an
/// optional leading `*` wildcard. Returns `Err` with a descriptive message on the
/// first invalid entry.
///
/// # Errors
///
/// Returns an error string when any pattern contains invalid characters.
pub fn validate_sandbox_denied_domains(config: &SandboxConfig) -> Result<(), String> {
    domain_match::validate_domain_patterns(&config.denied_domains)
}

/// Build a [`PermissionPolicy`] from a [`ToolsConfig`].
///
/// Uses the explicit `[tools.permissions]` TOML section when present;
/// otherwise falls back to legacy `blocked_commands` / `confirm_patterns` shell fields.
///
/// # Examples
///
/// ```no_run
/// use zeph_tools::{ToolsConfig, build_permission_policy};
/// use zeph_tools::AutonomyLevel;
///
/// let config = ToolsConfig::default();
/// let policy = build_permission_policy(&config, AutonomyLevel::Supervised);
/// ```
#[must_use]
pub fn build_permission_policy(
    config: &ToolsConfig,
    autonomy_level: AutonomyLevel,
) -> PermissionPolicy {
    let policy = if let Some(ref perms) = config.permissions {
        PermissionPolicy::from(perms.clone())
    } else {
        PermissionPolicy::from_legacy(
            &config.shell.blocked_commands,
            &config.shell.confirm_patterns,
        )
    };
    policy.with_autonomy(autonomy_level)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeph_config::tools::{AdversarialPolicyConfig, ResultCacheConfig};

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
        let policy = build_permission_policy(&config, AutonomyLevel::Supervised);
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
        let policy = build_permission_policy(&config, AutonomyLevel::Supervised);
        assert_eq!(
            policy.check("bash", "sudo rm"),
            crate::permissions::PermissionAction::Deny
        );
    }

    #[test]
    fn permission_policy_default_uses_legacy() {
        let config = ToolsConfig::default();
        assert!(config.permissions.is_none());
        let policy = build_permission_policy(&config, AutonomyLevel::Supervised);
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

    #[test]
    fn adversarial_policy_default_exempt_tools_contains_skill_ops() {
        let exempt = AdversarialPolicyConfig::default_exempt_tools();
        assert!(
            exempt.contains(&"load_skill".to_string()),
            "default exempt_tools must contain load_skill"
        );
        assert!(
            exempt.contains(&"invoke_skill".to_string()),
            "default exempt_tools must contain invoke_skill"
        );
    }

    #[test]
    fn utility_scoring_default_exempt_tools_contains_skill_ops() {
        let cfg = UtilityScoringConfig::default();
        assert!(
            cfg.exempt_tools.contains(&"invoke_skill".to_string()),
            "UtilityScoringConfig default exempt_tools must contain invoke_skill"
        );
        assert!(
            cfg.exempt_tools.contains(&"load_skill".to_string()),
            "UtilityScoringConfig default exempt_tools must contain load_skill"
        );
    }

    #[test]
    fn utility_partial_toml_exempt_tools_uses_default_not_empty_vec() {
        // Regression: #[serde(default)] on exempt_tools called Vec::default() (empty)
        // instead of the struct-level Default which sets ["invoke_skill", "load_skill"].
        let toml_str = r"
            [utility]
            enabled = true
            threshold = 0.1
        ";
        let config: ToolsConfig = toml::from_str(toml_str).unwrap();
        assert!(
            config
                .utility
                .exempt_tools
                .contains(&"invoke_skill".to_string()),
            "partial [tools.utility] TOML must populate exempt_tools with invoke_skill"
        );
        assert!(
            config
                .utility
                .exempt_tools
                .contains(&"load_skill".to_string()),
            "partial [tools.utility] TOML must populate exempt_tools with load_skill"
        );
    }
}
