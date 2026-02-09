use serde::Deserialize;

fn default_true() -> bool {
    true
}

fn default_timeout() -> u64 {
    30
}

/// Top-level configuration for tool execution.
#[derive(Debug, Deserialize)]
pub struct ToolsConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub shell: ShellConfig,
    #[serde(default)]
    pub scrape: ScrapeConfig,
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
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            shell: ShellConfig::default(),
            scrape: ScrapeConfig::default(),
        }
    }
}

impl Default for ShellConfig {
    fn default() -> Self {
        Self {
            timeout: default_timeout(),
            blocked_commands: Vec::new(),
            allowed_commands: Vec::new(),
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
        assert_eq!(config.shell.timeout, 30);
        assert!(config.shell.blocked_commands.is_empty());
    }

    #[test]
    fn default_shell_config() {
        let config = ShellConfig::default();
        assert_eq!(config.timeout, 30);
        assert!(config.blocked_commands.is_empty());
    }

    #[test]
    fn deserialize_omitted_fields_use_defaults() {
        let toml_str = "";
        let config: ToolsConfig = toml::from_str(toml_str).unwrap();
        assert!(config.enabled);
        assert_eq!(config.shell.timeout, 30);
        assert!(config.shell.blocked_commands.is_empty());
        assert_eq!(config.scrape.timeout, 15);
        assert_eq!(config.scrape.max_body_bytes, 1_048_576);
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
}
