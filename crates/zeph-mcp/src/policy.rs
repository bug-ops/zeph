// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! MCP declarative policy layer.
//!
//! Provides per-server tool allowlists, denylists, and optional per-server
//! rate limiting. Policy enforcement runs synchronously before each
//! `call_tool()` invocation in `McpManager`.
//!
//! Policy changes require an agent restart (hot-reload is a follow-up task).

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Instant;

use dashmap::DashMap;
use serde::{Deserialize, Serialize};

use zeph_common::ToolName;

use crate::manager::McpTrustLevel;
use crate::tool::{DataSensitivity, McpTool};

// ── Data-flow policy ─────────────────────────────────────────────────────────

/// Data-flow policy violation.
#[derive(Debug, thiserror::Error)]
pub enum DataFlowViolation {
    #[error(
        "tool '{tool_name}' (sensitivity={sensitivity:?}) on server '{server_id}' \
         (trust={trust:?}) violates data-flow policy: \
         high-sensitivity tools require trusted servers"
    )]
    SensitivityTrustMismatch {
        server_id: String,
        tool_name: ToolName,
        sensitivity: DataSensitivity,
        trust: McpTrustLevel,
    },
}

/// Check data-flow constraints at tool registration time.
///
/// High-sensitivity tools (shell, database write) must not be registered on
/// untrusted or sandboxed servers. Medium-sensitivity tools on sandboxed servers
/// emit a warning but are allowed.
///
/// # Errors
///
/// Returns `DataFlowViolation::SensitivityTrustMismatch` when a high-sensitivity
/// tool is registered on an untrusted or sandboxed server.
pub fn check_data_flow(
    tool: &McpTool,
    server_trust: McpTrustLevel,
) -> Result<(), DataFlowViolation> {
    match (tool.security_meta.data_sensitivity, server_trust) {
        (DataSensitivity::High, McpTrustLevel::Untrusted | McpTrustLevel::Sandboxed) => {
            Err(DataFlowViolation::SensitivityTrustMismatch {
                server_id: tool.server_id.clone(),
                tool_name: tool.name.as_str().into(),
                sensitivity: tool.security_meta.data_sensitivity,
                trust: server_trust,
            })
        }
        (DataSensitivity::Medium, McpTrustLevel::Sandboxed) => {
            tracing::warn!(
                server_id = %tool.server_id,
                tool_name = %tool.name,
                "medium-sensitivity tool on sandboxed server — use with caution"
            );
            Ok(())
        }
        _ => Ok(()),
    }
}

// ── Public types ──────────────────────────────────────────────────────────────

/// Rate limit configuration for a single MCP server.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RateLimit {
    /// Maximum number of tool calls allowed per minute across all tools on this server.
    pub max_calls_per_minute: u32,
}

/// Per-server MCP policy.
///
/// No policy present = allow all (backward compatible default).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct McpPolicy {
    /// Allowlist of tool names. `None` means all tools are allowed (subject to `denied_tools`).
    pub allowed_tools: Option<Vec<String>>,
    /// Denylist of tool names. Takes precedence over `allowed_tools`.
    pub denied_tools: Vec<String>,
    /// Optional rate limit for this server.
    pub rate_limit: Option<RateLimit>,
}

/// Reason a policy check blocked a tool call.
///
/// Returned by [`PolicyEnforcer::check`]. The outer [`McpError`](crate::error::McpError)
/// wraps this as `McpError::PolicyViolation`.
#[derive(Debug, thiserror::Error)]
pub enum PolicyViolation {
    #[error("tool '{tool_name}' is denied on server '{server_id}'")]
    ToolDenied {
        server_id: String,
        tool_name: ToolName,
    },

    #[error("tool '{tool_name}' is not in the allowlist for server '{server_id}'")]
    ToolNotAllowed {
        server_id: String,
        tool_name: ToolName,
    },

    #[error("rate limit exceeded for server '{server_id}' (max {max_calls_per_minute}/min)")]
    RateLimitExceeded {
        server_id: String,
        max_calls_per_minute: u32,
    },
}

/// Enforces MCP policies for all configured servers.
///
/// Uses a `DashMap` of per-server `Mutex<VecDeque<Instant>>` for sliding-window
/// rate limiting, avoiding a global lock across servers.
pub struct PolicyEnforcer {
    /// Map from `server_id` → policy.
    policies: DashMap<String, McpPolicy>,
    /// Map from `server_id` → sliding window of call timestamps.
    call_windows: DashMap<String, Mutex<VecDeque<Instant>>>,
}

impl PolicyEnforcer {
    /// Create an enforcer from a list of `(server_id, policy)` pairs.
    #[must_use]
    pub fn new(entries: Vec<(String, McpPolicy)>) -> Self {
        let policies = DashMap::new();
        let call_windows = DashMap::new();
        for (id, policy) in entries {
            if policy.rate_limit.is_some() {
                call_windows.insert(id.clone(), Mutex::new(VecDeque::new()));
            }
            policies.insert(id, policy);
        }
        Self {
            policies,
            call_windows,
        }
    }

    /// Check whether `tool_name` may be called on `server_id`.
    ///
    /// Returns `Ok(())` if the call is permitted, or a `PolicyViolation` if not.
    ///
    /// # Errors
    ///
    /// Returns `PolicyViolation::ToolDenied` if the tool is in `denied_tools`,
    /// `PolicyViolation::ToolNotAllowed` if an allowlist exists and the tool is absent,
    /// or `PolicyViolation::RateLimitExceeded` if the sliding-window count is over the limit.
    pub fn check(&self, server_id: &str, tool_name: &str) -> Result<(), PolicyViolation> {
        let Some(policy) = self.policies.get(server_id) else {
            // No policy configured — allow all.
            return Ok(());
        };

        if policy.denied_tools.iter().any(|t| t == tool_name) {
            return Err(PolicyViolation::ToolDenied {
                server_id: server_id.into(),
                tool_name: tool_name.into(),
            });
        }

        if policy
            .allowed_tools
            .as_ref()
            .is_some_and(|allowlist| !allowlist.iter().any(|t| t == tool_name))
        {
            return Err(PolicyViolation::ToolNotAllowed {
                server_id: server_id.into(),
                tool_name: tool_name.into(),
            });
        }

        if let Some(rl) = &policy.rate_limit {
            self.check_rate_limit(server_id, rl.max_calls_per_minute)?;
        }

        Ok(())
    }

    /// Sliding-window rate limit check. Records a new call timestamp on success.
    fn check_rate_limit(
        &self,
        server_id: &str,
        max_calls_per_minute: u32,
    ) -> Result<(), PolicyViolation> {
        let window_entry = self
            .call_windows
            .get(server_id)
            .expect("call_windows entry created alongside rate_limit policy");

        let mut window = window_entry.lock().expect("rate limit mutex not poisoned");
        let now = Instant::now();
        let cutoff = now
            .checked_sub(std::time::Duration::from_mins(1))
            .unwrap_or(now);

        // Drain calls older than 60 seconds.
        while window.front().is_some_and(|t| *t < cutoff) {
            window.pop_front();
        }

        if window.len() >= max_calls_per_minute as usize {
            return Err(PolicyViolation::RateLimitExceeded {
                server_id: server_id.into(),
                max_calls_per_minute,
            });
        }

        window.push_back(now);
        Ok(())
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn enforcer_with_policy(server_id: &str, policy: McpPolicy) -> PolicyEnforcer {
        PolicyEnforcer::new(vec![(server_id.into(), policy)])
    }

    #[test]
    fn no_policy_allows_any_tool() {
        let enforcer = PolicyEnforcer::new(vec![]);
        assert!(enforcer.check("any-server", "any-tool").is_ok());
    }

    #[test]
    fn denied_tool_blocked() {
        let policy = McpPolicy {
            denied_tools: vec!["rm".into()],
            ..Default::default()
        };
        let enforcer = enforcer_with_policy("srv", policy);
        let err = enforcer.check("srv", "rm").unwrap_err();
        assert!(matches!(err, PolicyViolation::ToolDenied { .. }));
    }

    #[test]
    fn deny_takes_precedence_over_allowlist() {
        let policy = McpPolicy {
            allowed_tools: Some(vec!["rm".into()]),
            denied_tools: vec!["rm".into()],
            ..Default::default()
        };
        let enforcer = enforcer_with_policy("srv", policy);
        let err = enforcer.check("srv", "rm").unwrap_err();
        assert!(matches!(err, PolicyViolation::ToolDenied { .. }));
    }

    #[test]
    fn allowlist_blocks_unlisted_tool() {
        let policy = McpPolicy {
            allowed_tools: Some(vec!["read_file".into()]),
            ..Default::default()
        };
        let enforcer = enforcer_with_policy("srv", policy);
        let err = enforcer.check("srv", "write_file").unwrap_err();
        assert!(matches!(err, PolicyViolation::ToolNotAllowed { .. }));
    }

    #[test]
    fn allowlist_permits_listed_tool() {
        let policy = McpPolicy {
            allowed_tools: Some(vec!["read_file".into()]),
            ..Default::default()
        };
        let enforcer = enforcer_with_policy("srv", policy);
        assert!(enforcer.check("srv", "read_file").is_ok());
    }

    #[test]
    fn rate_limit_blocks_after_threshold() {
        let policy = McpPolicy {
            rate_limit: Some(RateLimit {
                max_calls_per_minute: 2,
            }),
            ..Default::default()
        };
        let enforcer = enforcer_with_policy("srv", policy);
        assert!(enforcer.check("srv", "tool").is_ok());
        assert!(enforcer.check("srv", "tool").is_ok());
        let err = enforcer.check("srv", "tool").unwrap_err();
        assert!(matches!(err, PolicyViolation::RateLimitExceeded { .. }));
    }

    #[test]
    fn unknown_server_is_allowed() {
        let policy = McpPolicy {
            denied_tools: vec!["rm".into()],
            ..Default::default()
        };
        let enforcer = enforcer_with_policy("srv", policy);
        // different server — no policy → allowed
        assert!(enforcer.check("other-srv", "rm").is_ok());
    }

    // --- check_data_flow ---

    fn make_tool_with_meta(
        name: &str,
        sensitivity: crate::tool::DataSensitivity,
    ) -> crate::tool::McpTool {
        use crate::tool::ToolSecurityMeta;
        crate::tool::McpTool {
            server_id: "srv".into(),
            name: name.into(),
            description: "test".into(),
            input_schema: serde_json::json!({}),
            security_meta: ToolSecurityMeta {
                data_sensitivity: sensitivity,
                capabilities: vec![],
                flagged_parameters: Vec::new(),
            },
        }
    }

    #[test]
    fn data_flow_high_sensitivity_untrusted_blocked() {
        let tool = make_tool_with_meta("exec_shell", crate::tool::DataSensitivity::High);
        let result = check_data_flow(&tool, McpTrustLevel::Untrusted);
        assert!(matches!(
            result,
            Err(DataFlowViolation::SensitivityTrustMismatch { .. })
        ));
    }

    #[test]
    fn data_flow_high_sensitivity_sandboxed_blocked() {
        let tool = make_tool_with_meta("exec_shell", crate::tool::DataSensitivity::High);
        let result = check_data_flow(&tool, McpTrustLevel::Sandboxed);
        assert!(matches!(
            result,
            Err(DataFlowViolation::SensitivityTrustMismatch { .. })
        ));
    }

    #[test]
    fn data_flow_high_sensitivity_trusted_allowed() {
        let tool = make_tool_with_meta("exec_shell", crate::tool::DataSensitivity::High);
        assert!(check_data_flow(&tool, McpTrustLevel::Trusted).is_ok());
    }

    #[test]
    fn data_flow_medium_sensitivity_untrusted_allowed() {
        let tool = make_tool_with_meta("write_file", crate::tool::DataSensitivity::Medium);
        assert!(check_data_flow(&tool, McpTrustLevel::Untrusted).is_ok());
    }

    #[test]
    fn data_flow_medium_sensitivity_sandboxed_warns_but_allows() {
        let tool = make_tool_with_meta("write_file", crate::tool::DataSensitivity::Medium);
        // Medium on Sandboxed should warn but not block
        assert!(check_data_flow(&tool, McpTrustLevel::Sandboxed).is_ok());
    }

    #[test]
    fn data_flow_low_sensitivity_any_trust_allowed() {
        let tool = make_tool_with_meta("get_info", crate::tool::DataSensitivity::Low);
        assert!(check_data_flow(&tool, McpTrustLevel::Untrusted).is_ok());
        assert!(check_data_flow(&tool, McpTrustLevel::Sandboxed).is_ok());
        assert!(check_data_flow(&tool, McpTrustLevel::Trusted).is_ok());
    }

    #[test]
    fn data_flow_none_sensitivity_any_trust_allowed() {
        let tool = make_tool_with_meta("read_info", crate::tool::DataSensitivity::None);
        assert!(check_data_flow(&tool, McpTrustLevel::Untrusted).is_ok());
    }

    #[test]
    fn data_flow_violation_message_descriptive() {
        let tool = make_tool_with_meta("exec_shell", crate::tool::DataSensitivity::High);
        let err = check_data_flow(&tool, McpTrustLevel::Untrusted).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("exec_shell"));
        assert!(msg.contains("high-sensitivity"));
    }

    #[test]
    fn policy_violation_messages_are_descriptive() {
        let denied = PolicyViolation::ToolDenied {
            server_id: "s".into(),
            tool_name: "t".into(),
        };
        assert!(denied.to_string().contains("denied"));

        let not_allowed = PolicyViolation::ToolNotAllowed {
            server_id: "s".into(),
            tool_name: "t".into(),
        };
        assert!(not_allowed.to_string().contains("allowlist"));

        let rate = PolicyViolation::RateLimitExceeded {
            server_id: "s".into(),
            max_calls_per_minute: 10,
        };
        assert!(rate.to_string().contains("rate limit"));
    }

    #[test]
    fn data_flow_medium_sensitivity_trusted_allowed() {
        let tool = make_tool_with_meta("write_file", crate::tool::DataSensitivity::Medium);
        assert!(check_data_flow(&tool, McpTrustLevel::Trusted).is_ok());
    }
}
