// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `/policy` command handler (requires `policy-enforcer` feature).

use zeph_tools::{DefaultEffect, PolicyContext, PolicyDecision, PolicyEnforcer, TrustLevel};

use super::Agent;
use super::error::AgentError;
use crate::channel::Channel;

impl<C: Channel> Agent<C> {
    /// Handle `/policy [status|check <tool> [args_json]]` command.
    pub(super) async fn handle_policy_command(&mut self, args: &str) -> Result<(), AgentError> {
        let Some(ref policy_config) = self.policy_config else {
            return self
                .channel
                .send("Policy enforcer: not configured (use --policy-file or set [tools.policy] in config)")
                .await
                .map_err(Into::into);
        };

        let parts: Vec<&str> = args.split_whitespace().collect();

        match parts.first().copied().unwrap_or("status") {
            "status" => {
                let rule_count = policy_config.rules.len();
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
                self.channel
                    .send(&format!(
                        "Policy: {status_str}, default: {default_str}, rules: {rule_count}{file_str}"
                    ))
                    .await
                    .map_err(Into::into)
            }
            "check" => {
                let tool = parts.get(1).copied().unwrap_or("");
                if tool.is_empty() {
                    return self
                        .channel
                        .send("Usage: /policy check <tool> [args_json]")
                        .await
                        .map_err(Into::into);
                }
                let args_json = parts.get(2..).map(|s| s.join(" ")).unwrap_or_default();
                let params: serde_json::Map<String, serde_json::Value> = if args_json.is_empty() {
                    serde_json::Map::new()
                } else {
                    match serde_json::from_str(&args_json) {
                        Ok(serde_json::Value::Object(m)) => m,
                        Ok(_) => {
                            return self
                                .channel
                                .send("args_json must be a JSON object")
                                .await
                                .map_err(Into::into);
                        }
                        Err(e) => {
                            return self
                                .channel
                                .send(&format!("invalid args_json: {e}"))
                                .await
                                .map_err(Into::into);
                        }
                    }
                };

                match PolicyEnforcer::compile(policy_config) {
                    Ok(enforcer) => {
                        let ctx = PolicyContext {
                            trust_level: TrustLevel::Trusted,
                            env: std::env::vars().collect(),
                        };
                        match enforcer.evaluate(tool, &params, &ctx) {
                            PolicyDecision::Allow { trace } => self
                                .channel
                                .send(&format!("Allow: {trace}"))
                                .await
                                .map_err(Into::into),
                            PolicyDecision::Deny { trace } => self
                                .channel
                                .send(&format!("Deny: {trace}"))
                                .await
                                .map_err(Into::into),
                        }
                    }
                    Err(e) => self
                        .channel
                        .send(&format!("policy compile error: {e}"))
                        .await
                        .map_err(Into::into),
                }
            }
            other => self
                .channel
                .send(&format!(
                    "Unknown /policy subcommand: {other}. Use: status, check <tool> [args_json]"
                ))
                .await
                .map_err(Into::into),
        }
    }
}
