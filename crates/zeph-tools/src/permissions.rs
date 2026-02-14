use std::collections::HashMap;

use glob::Pattern;
use serde::Deserialize;

/// Action a permission rule resolves to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionAction {
    Allow,
    Ask,
    Deny,
}

/// Single permission rule: glob `pattern` + action.
#[derive(Debug, Clone, Deserialize)]
pub struct PermissionRule {
    pub pattern: String,
    pub action: PermissionAction,
}

/// Tool permission policy: maps `tool_id` → ordered list of rules.
/// First matching rule wins; default is `Ask`.
///
/// Runtime enforcement is currently implemented for `bash` (`ShellExecutor`).
/// Other tools rely on prompt filtering via `ToolRegistry::format_for_prompt_filtered`.
#[derive(Debug, Clone, Default)]
pub struct PermissionPolicy {
    rules: HashMap<String, Vec<PermissionRule>>,
}

impl PermissionPolicy {
    #[must_use]
    pub fn new(rules: HashMap<String, Vec<PermissionRule>>) -> Self {
        Self { rules }
    }

    /// Check permission for a tool invocation. First matching glob wins.
    #[must_use]
    pub fn check(&self, tool_id: &str, input: &str) -> PermissionAction {
        let Some(rules) = self.rules.get(tool_id) else {
            return PermissionAction::Ask;
        };
        let normalized = input.to_lowercase();
        for rule in rules {
            if let Ok(pat) = Pattern::new(&rule.pattern.to_lowercase())
                && pat.matches(&normalized)
            {
                return rule.action;
            }
        }
        PermissionAction::Ask
    }

    /// Build policy from legacy `blocked_commands` / `confirm_patterns` for "bash" tool.
    #[must_use]
    pub fn from_legacy(blocked: &[String], confirm: &[String]) -> Self {
        let mut rules = Vec::with_capacity(blocked.len() + confirm.len());
        for cmd in blocked {
            rules.push(PermissionRule {
                pattern: format!("*{cmd}*"),
                action: PermissionAction::Deny,
            });
        }
        for pat in confirm {
            rules.push(PermissionRule {
                pattern: format!("*{pat}*"),
                action: PermissionAction::Ask,
            });
        }
        let mut map = HashMap::new();
        if !rules.is_empty() {
            map.insert("bash".to_owned(), rules);
        }
        Self { rules: map }
    }

    /// Returns true if all rules for a `tool_id` are Deny.
    #[must_use]
    pub fn is_fully_denied(&self, tool_id: &str) -> bool {
        self.rules.get(tool_id).is_some_and(|rules| {
            !rules.is_empty() && rules.iter().all(|r| r.action == PermissionAction::Deny)
        })
    }

    /// Returns a reference to the internal rules map.
    #[must_use]
    pub fn rules(&self) -> &HashMap<String, Vec<PermissionRule>> {
        &self.rules
    }
}

/// TOML-deserializable permissions config section.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct PermissionsConfig {
    #[serde(flatten)]
    pub tools: HashMap<String, Vec<PermissionRule>>,
}

impl From<PermissionsConfig> for PermissionPolicy {
    fn from(config: PermissionsConfig) -> Self {
        Self::new(config.tools)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy_with_rules(tool_id: &str, rules: Vec<(&str, PermissionAction)>) -> PermissionPolicy {
        let rules = rules
            .into_iter()
            .map(|(pattern, action)| PermissionRule {
                pattern: pattern.to_owned(),
                action,
            })
            .collect();
        let mut map = HashMap::new();
        map.insert(tool_id.to_owned(), rules);
        PermissionPolicy::new(map)
    }

    #[test]
    fn allow_rule_matches_glob() {
        let policy = policy_with_rules("bash", vec![("echo *", PermissionAction::Allow)]);
        assert_eq!(policy.check("bash", "echo hello"), PermissionAction::Allow);
    }

    #[test]
    fn deny_rule_blocks() {
        let policy = policy_with_rules("bash", vec![("*rm -rf*", PermissionAction::Deny)]);
        assert_eq!(policy.check("bash", "rm -rf /tmp"), PermissionAction::Deny);
    }

    #[test]
    fn ask_rule_returns_ask() {
        let policy = policy_with_rules("bash", vec![("*git push*", PermissionAction::Ask)]);
        assert_eq!(
            policy.check("bash", "git push origin main"),
            PermissionAction::Ask
        );
    }

    #[test]
    fn first_matching_rule_wins() {
        let policy = policy_with_rules(
            "bash",
            vec![
                ("*safe*", PermissionAction::Allow),
                ("*", PermissionAction::Deny),
            ],
        );
        assert_eq!(
            policy.check("bash", "safe command"),
            PermissionAction::Allow
        );
        assert_eq!(
            policy.check("bash", "dangerous command"),
            PermissionAction::Deny
        );
    }

    #[test]
    fn no_rules_returns_default_ask() {
        let policy = PermissionPolicy::default();
        assert_eq!(policy.check("bash", "anything"), PermissionAction::Ask);
    }

    #[test]
    fn wildcard_pattern() {
        let policy = policy_with_rules("bash", vec![("*", PermissionAction::Allow)]);
        assert_eq!(policy.check("bash", "any command"), PermissionAction::Allow);
    }

    #[test]
    fn case_sensitive_tool_id() {
        let policy = policy_with_rules("bash", vec![("*", PermissionAction::Deny)]);
        assert_eq!(policy.check("BASH", "cmd"), PermissionAction::Ask);
        assert_eq!(policy.check("bash", "cmd"), PermissionAction::Deny);
    }

    #[test]
    fn no_matching_rule_falls_through_to_ask() {
        let policy = policy_with_rules("bash", vec![("echo *", PermissionAction::Allow)]);
        assert_eq!(policy.check("bash", "ls -la"), PermissionAction::Ask);
    }

    #[test]
    fn from_legacy_creates_deny_and_ask_rules() {
        let policy = PermissionPolicy::from_legacy(&["sudo".to_owned()], &["rm ".to_owned()]);
        assert_eq!(policy.check("bash", "sudo apt"), PermissionAction::Deny);
        assert_eq!(policy.check("bash", "rm file"), PermissionAction::Ask);
    }

    #[test]
    fn is_fully_denied_all_deny() {
        let policy = policy_with_rules("bash", vec![("*", PermissionAction::Deny)]);
        assert!(policy.is_fully_denied("bash"));
    }

    #[test]
    fn is_fully_denied_mixed() {
        let policy = policy_with_rules(
            "bash",
            vec![
                ("echo *", PermissionAction::Allow),
                ("*", PermissionAction::Deny),
            ],
        );
        assert!(!policy.is_fully_denied("bash"));
    }

    #[test]
    fn is_fully_denied_no_rules() {
        let policy = PermissionPolicy::default();
        assert!(!policy.is_fully_denied("bash"));
    }

    #[test]
    fn case_insensitive_input_matching() {
        let policy = policy_with_rules("bash", vec![("*sudo*", PermissionAction::Deny)]);
        assert_eq!(policy.check("bash", "SUDO apt"), PermissionAction::Deny);
        assert_eq!(policy.check("bash", "Sudo apt"), PermissionAction::Deny);
        assert_eq!(policy.check("bash", "sudo apt"), PermissionAction::Deny);
    }

    #[test]
    fn permissions_config_deserialize() {
        let toml_str = r#"
            [[bash]]
            pattern = "*sudo*"
            action = "deny"

            [[bash]]
            pattern = "*"
            action = "ask"
        "#;
        let config: PermissionsConfig = toml::from_str(toml_str).unwrap();
        let policy = PermissionPolicy::from(config);
        assert_eq!(policy.check("bash", "sudo rm"), PermissionAction::Deny);
        assert_eq!(policy.check("bash", "echo hi"), PermissionAction::Ask);
    }
}
