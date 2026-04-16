// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `/policy` command handler (requires `policy-enforcer` feature).

use std::collections::HashMap;
use std::str::FromStr;

use zeph_tools::{DefaultEffect, PolicyContext, PolicyDecision, PolicyEnforcer, SkillTrustLevel};

use super::Agent;
use crate::channel::Channel;

impl<C: Channel> Agent<C> {
    /// Channel-free version of [`Self::handle_policy_command`] for use via
    /// [`zeph_commands::traits::agent::AgentAccess`].
    pub(super) fn handle_policy_command_as_string(&mut self, args: &str) -> String {
        let Some(ref policy_config) = self.session.policy_config else {
            return "Policy enforcer: not configured (use --policy-file or set [tools.policy] in config)"
                .to_owned();
        };

        let parts: Vec<&str> = args.split_whitespace().collect();

        match parts.first().copied().unwrap_or("status") {
            "status" => {
                let rule_count = PolicyEnforcer::compile(policy_config)
                    .map_or(policy_config.rules.len(), |e| e.rule_count());
                let default_str = match policy_config.default_effect {
                    DefaultEffect::Allow => "allow",
                    DefaultEffect::Deny => "deny",
                };
                let status_str = if policy_config.enabled {
                    "enabled"
                } else {
                    "disabled"
                };
                let file_str = policy_config
                    .policy_file
                    .as_deref()
                    .map(|f| format!(", file: {f}"))
                    .unwrap_or_default();
                format!(
                    "Policy: {status_str}, default: {default_str}, rules: {rule_count}{file_str}"
                )
            }
            "check" => self.handle_policy_check_as_string(parts.get(1..).unwrap_or(&[])),
            other => format!(
                "Unknown /policy subcommand: {other}. Use: status, check <tool> [args_json]"
            ),
        }
    }

    fn handle_policy_check_as_string(&mut self, raw: &[&str]) -> String {
        let Some(ref policy_config) = self.session.policy_config else {
            return String::new();
        };

        let mut remaining = raw.to_vec();
        let mut trust_level = SkillTrustLevel::Trusted;
        if let Some(pos) = remaining.iter().position(|&s| s == "--trust-level") {
            remaining.remove(pos);
            if pos < remaining.len() {
                let level_str = remaining.remove(pos);
                match SkillTrustLevel::from_str(level_str) {
                    Ok(level) => trust_level = level,
                    Err(e) => return format!("invalid --trust-level: {e}"),
                }
            } else {
                return "--trust-level requires a value: trusted, verified, quarantined, blocked"
                    .to_owned();
            }
        }

        let tool = remaining.first().copied().unwrap_or("");
        if tool.is_empty() {
            return "Usage: /policy check [--trust-level <level>] <tool> [args_json]".to_owned();
        }

        let args_json = remaining.get(1..).map(|s| s.join(" ")).unwrap_or_default();
        let params: serde_json::Map<String, serde_json::Value> = if args_json.is_empty() {
            serde_json::Map::new()
        } else {
            match serde_json::from_str(&args_json) {
                Ok(serde_json::Value::Object(m)) => m,
                Ok(_) => return "args_json must be a JSON object".to_owned(),
                Err(e) => return format!("invalid args_json: {e}"),
            }
        };

        match PolicyEnforcer::compile(policy_config) {
            Ok(enforcer) => {
                let ctx = PolicyContext {
                    trust_level,
                    env: HashMap::new(),
                };
                match enforcer.evaluate(tool, &params, &ctx) {
                    PolicyDecision::Allow { trace } => format!("Allow: {trace}"),
                    PolicyDecision::Deny { trace } => format!("Deny: {trace}"),
                }
            }
            Err(e) => format!("policy compile error: {e}"),
        }
    }
}
