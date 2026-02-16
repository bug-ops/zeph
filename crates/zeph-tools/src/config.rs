use serde::Deserialize;

use crate::permissions::{AutonomyLevel, PermissionPolicy, PermissionsConfig};

fn default_true() -> bool {
    true
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
    ]
}

fn default_audit_destination() -> String {
    "stdout".into()
}

/// Top-level configuration for tool execution.
#[derive(Debug, Deserialize)]
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
#[derive(Debug, Deserialize)]
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
}

/// Configuration for audit logging of tool executions.
#[derive(Debug, Deserialize)]
pub struct AuditConfig {
    #[serde(default)]
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
        }
    }
}

impl Default for AuditConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            destination: default_audit_destination(),
        }
    }
}

fn default_scrape_timeout() -> u64 {
    15
}

fn default_max_body_bytes() -> usize {
    1_048_576
}

/// Configuration for the web scrape tool.
#[derive(Debug, Deserialize)]
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
        let toml_str = r#"
            [shell]
            timeout = 30
        "#;

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
        assert!(!config.audit.enabled);
    }

    #[test]
    fn tools_summarize_output_default_true() {
        let config = ToolsConfig::default();
        assert!(config.summarize_output);
    }

    #[test]
    fn tools_summarize_output_parsing() {
        let toml_str = r#"
            summarize_output = true
        "#;
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
        assert_eq!(config.scrape.max_body_bytes, 1_048_576);
        assert!(!config.audit.enabled);
        assert_eq!(config.audit.destination, "stdout");
        assert!(config.summarize_output);
    }

    #[test]
    fn default_scrape_config() {
        let config = ScrapeConfig::default();
        assert_eq!(config.timeout, 15);
        assert_eq!(config.max_body_bytes, 1_048_576);
    }

    #[test]
    fn deserialize_scrape_config() {
        let toml_str = r#"
            [scrape]
            timeout = 30
            max_body_bytes = 2097152
        "#;

        let config: ToolsConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.scrape.timeout, 30);
        assert_eq!(config.scrape.max_body_bytes, 2_097_152);
    }

    #[test]
    fn tools_config_default_includes_scrape() {
        let config = ToolsConfig::default();
        assert_eq!(config.scrape.timeout, 15);
        assert_eq!(config.scrape.max_body_bytes, 1_048_576);
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
        assert!(!config.enabled);
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
}
