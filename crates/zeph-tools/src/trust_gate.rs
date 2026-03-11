// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Trust-level enforcement layer for tool execution.

use std::sync::atomic::{AtomicU8, Ordering};

use crate::TrustLevel;

use crate::executor::{ToolCall, ToolError, ToolExecutor, ToolOutput};
use crate::permissions::{AutonomyLevel, PermissionAction, PermissionPolicy};
use crate::registry::ToolDef;

/// Tools denied when a Quarantined skill is active.
const QUARANTINE_DENIED: &[&str] = &["bash", "file_write", "web_scrape"];

fn trust_to_u8(level: TrustLevel) -> u8 {
    match level {
        TrustLevel::Trusted => 0,
        TrustLevel::Verified => 1,
        TrustLevel::Quarantined => 2,
        TrustLevel::Blocked => 3,
    }
}

fn u8_to_trust(v: u8) -> TrustLevel {
    match v {
        0 => TrustLevel::Trusted,
        1 => TrustLevel::Verified,
        2 => TrustLevel::Quarantined,
        _ => TrustLevel::Blocked,
    }
}

/// Wraps an inner `ToolExecutor` and applies trust-level permission overlays.
pub struct TrustGateExecutor<T: ToolExecutor> {
    inner: T,
    policy: PermissionPolicy,
    effective_trust: AtomicU8,
}

impl<T: ToolExecutor + std::fmt::Debug> std::fmt::Debug for TrustGateExecutor<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TrustGateExecutor")
            .field("inner", &self.inner)
            .field("policy", &self.policy)
            .field("effective_trust", &self.effective_trust())
            .finish()
    }
}

impl<T: ToolExecutor> TrustGateExecutor<T> {
    #[must_use]
    pub fn new(inner: T, policy: PermissionPolicy) -> Self {
        Self {
            inner,
            policy,
            effective_trust: AtomicU8::new(trust_to_u8(TrustLevel::Trusted)),
        }
    }

    pub fn set_effective_trust(&self, level: TrustLevel) {
        self.effective_trust
            .store(trust_to_u8(level), Ordering::Relaxed);
    }

    #[must_use]
    pub fn effective_trust(&self) -> TrustLevel {
        u8_to_trust(self.effective_trust.load(Ordering::Relaxed))
    }

    fn check_trust(&self, tool_id: &str, input: &str) -> Result<(), ToolError> {
        match self.effective_trust() {
            TrustLevel::Blocked => {
                return Err(ToolError::Blocked {
                    command: "all tools blocked (trust=blocked)".to_owned(),
                });
            }
            TrustLevel::Quarantined => {
                if QUARANTINE_DENIED.contains(&tool_id) {
                    return Err(ToolError::Blocked {
                        command: format!("{tool_id} denied (trust=quarantined)"),
                    });
                }
            }
            TrustLevel::Trusted | TrustLevel::Verified => {}
        }

        // PermissionPolicy was designed for the bash tool. In Supervised mode, tools
        // without explicit rules default to Ask, which incorrectly blocks MCP/LSP tools.
        // Skip the policy check for such tools — trust-level enforcement above is sufficient.
        // ReadOnly mode is excluded: its allowlist is enforced inside policy.check().
        if self.policy.autonomy_level() == AutonomyLevel::Supervised
            && self.policy.rules().get(tool_id).is_none()
        {
            return Ok(());
        }

        match self.policy.check(tool_id, input) {
            PermissionAction::Allow => Ok(()),
            PermissionAction::Ask => Err(ToolError::ConfirmationRequired {
                command: input.to_owned(),
            }),
            PermissionAction::Deny => Err(ToolError::Blocked {
                command: input.to_owned(),
            }),
        }
    }
}

impl<T: ToolExecutor> ToolExecutor for TrustGateExecutor<T> {
    async fn execute(&self, response: &str) -> Result<Option<ToolOutput>, ToolError> {
        match self.effective_trust() {
            TrustLevel::Blocked | TrustLevel::Quarantined => {
                return Err(ToolError::Blocked {
                    command: format!(
                        "tool execution denied (trust={})",
                        format!("{:?}", self.effective_trust()).to_lowercase()
                    ),
                });
            }
            TrustLevel::Trusted | TrustLevel::Verified => {}
        }
        self.inner.execute(response).await
    }

    async fn execute_confirmed(&self, response: &str) -> Result<Option<ToolOutput>, ToolError> {
        match self.effective_trust() {
            TrustLevel::Blocked | TrustLevel::Quarantined => {
                return Err(ToolError::Blocked {
                    command: format!(
                        "tool execution denied (trust={})",
                        format!("{:?}", self.effective_trust()).to_lowercase()
                    ),
                });
            }
            TrustLevel::Trusted | TrustLevel::Verified => {}
        }
        self.inner.execute_confirmed(response).await
    }

    fn tool_definitions(&self) -> Vec<ToolDef> {
        self.inner.tool_definitions()
    }

    async fn execute_tool_call(&self, call: &ToolCall) -> Result<Option<ToolOutput>, ToolError> {
        let input = call
            .params
            .get("command")
            .or_else(|| call.params.get("file_path"))
            .or_else(|| call.params.get("query"))
            .or_else(|| call.params.get("url"))
            .or_else(|| call.params.get("uri"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        self.check_trust(&call.tool_id, input)?;
        self.inner.execute_tool_call(call).await
    }

    fn set_skill_env(&self, env: Option<std::collections::HashMap<String, String>>) {
        self.inner.set_skill_env(env);
    }

    fn set_effective_trust(&self, level: crate::TrustLevel) {
        self.effective_trust
            .store(trust_to_u8(level), Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct MockExecutor;
    impl ToolExecutor for MockExecutor {
        async fn execute(&self, _: &str) -> Result<Option<ToolOutput>, ToolError> {
            Ok(None)
        }
        async fn execute_tool_call(
            &self,
            call: &ToolCall,
        ) -> Result<Option<ToolOutput>, ToolError> {
            Ok(Some(ToolOutput {
                tool_name: call.tool_id.clone(),
                summary: "ok".into(),
                blocks_executed: 1,
                filter_stats: None,
                diff: None,
                streamed: false,
                terminal_id: None,
                locations: None,
                raw_response: None,
            }))
        }
    }

    fn make_call(tool_id: &str) -> ToolCall {
        ToolCall {
            tool_id: tool_id.into(),
            params: serde_json::Map::new(),
        }
    }

    fn make_call_with_cmd(tool_id: &str, cmd: &str) -> ToolCall {
        let mut params = serde_json::Map::new();
        params.insert("command".into(), serde_json::Value::String(cmd.into()));
        ToolCall {
            tool_id: tool_id.into(),
            params,
        }
    }

    #[tokio::test]
    async fn trusted_allows_all() {
        let gate = TrustGateExecutor::new(MockExecutor, PermissionPolicy::default());
        gate.set_effective_trust(TrustLevel::Trusted);

        let result = gate.execute_tool_call(&make_call("bash")).await;
        // Default policy has no rules for bash => skip policy check => Ok
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn quarantined_denies_bash() {
        let gate = TrustGateExecutor::new(MockExecutor, PermissionPolicy::default());
        gate.set_effective_trust(TrustLevel::Quarantined);

        let result = gate.execute_tool_call(&make_call("bash")).await;
        assert!(matches!(result, Err(ToolError::Blocked { .. })));
    }

    #[tokio::test]
    async fn quarantined_denies_file_write() {
        let gate = TrustGateExecutor::new(MockExecutor, PermissionPolicy::default());
        gate.set_effective_trust(TrustLevel::Quarantined);

        let result = gate.execute_tool_call(&make_call("file_write")).await;
        assert!(matches!(result, Err(ToolError::Blocked { .. })));
    }

    #[tokio::test]
    async fn quarantined_allows_file_read() {
        let policy = crate::permissions::PermissionPolicy::from_legacy(&[], &[]);
        let gate = TrustGateExecutor::new(MockExecutor, policy);
        gate.set_effective_trust(TrustLevel::Quarantined);

        let result = gate.execute_tool_call(&make_call("file_read")).await;
        // file_read is not in quarantine denied list, and policy has no rules for file_read => Ok
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn blocked_denies_everything() {
        let gate = TrustGateExecutor::new(MockExecutor, PermissionPolicy::default());
        gate.set_effective_trust(TrustLevel::Blocked);

        let result = gate.execute_tool_call(&make_call("file_read")).await;
        assert!(matches!(result, Err(ToolError::Blocked { .. })));
    }

    #[tokio::test]
    async fn policy_deny_overrides_trust() {
        let policy = crate::permissions::PermissionPolicy::from_legacy(&["sudo".into()], &[]);
        let gate = TrustGateExecutor::new(MockExecutor, policy);
        gate.set_effective_trust(TrustLevel::Trusted);

        let result = gate
            .execute_tool_call(&make_call_with_cmd("bash", "sudo rm"))
            .await;
        assert!(matches!(result, Err(ToolError::Blocked { .. })));
    }

    #[tokio::test]
    async fn blocked_denies_execute() {
        let gate = TrustGateExecutor::new(MockExecutor, PermissionPolicy::default());
        gate.set_effective_trust(TrustLevel::Blocked);

        let result = gate.execute("some response").await;
        assert!(matches!(result, Err(ToolError::Blocked { .. })));
    }

    #[tokio::test]
    async fn blocked_denies_execute_confirmed() {
        let gate = TrustGateExecutor::new(MockExecutor, PermissionPolicy::default());
        gate.set_effective_trust(TrustLevel::Blocked);

        let result = gate.execute_confirmed("some response").await;
        assert!(matches!(result, Err(ToolError::Blocked { .. })));
    }

    #[tokio::test]
    async fn trusted_allows_execute() {
        let gate = TrustGateExecutor::new(MockExecutor, PermissionPolicy::default());
        gate.set_effective_trust(TrustLevel::Trusted);

        let result = gate.execute("some response").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn verified_with_allow_policy_succeeds() {
        let policy = crate::permissions::PermissionPolicy::from_legacy(&[], &[]);
        let gate = TrustGateExecutor::new(MockExecutor, policy);
        gate.set_effective_trust(TrustLevel::Verified);

        let result = gate
            .execute_tool_call(&make_call_with_cmd("bash", "echo hi"))
            .await
            .unwrap();
        assert!(result.is_some());
    }

    #[tokio::test]
    async fn quarantined_denies_web_scrape() {
        let gate = TrustGateExecutor::new(MockExecutor, PermissionPolicy::default());
        gate.set_effective_trust(TrustLevel::Quarantined);

        let result = gate.execute_tool_call(&make_call("web_scrape")).await;
        assert!(matches!(result, Err(ToolError::Blocked { .. })));
    }

    #[derive(Debug)]
    struct EnvCapture {
        captured: std::sync::Mutex<Option<std::collections::HashMap<String, String>>>,
    }
    impl EnvCapture {
        fn new() -> Self {
            Self {
                captured: std::sync::Mutex::new(None),
            }
        }
    }
    impl ToolExecutor for EnvCapture {
        async fn execute(&self, _: &str) -> Result<Option<ToolOutput>, ToolError> {
            Ok(None)
        }
        async fn execute_tool_call(&self, _: &ToolCall) -> Result<Option<ToolOutput>, ToolError> {
            Ok(None)
        }
        fn set_skill_env(&self, env: Option<std::collections::HashMap<String, String>>) {
            *self.captured.lock().unwrap() = env;
        }
    }

    #[test]
    fn set_skill_env_forwarded_to_inner() {
        let inner = EnvCapture::new();
        let gate = TrustGateExecutor::new(inner, PermissionPolicy::default());

        let mut env = std::collections::HashMap::new();
        env.insert("MY_VAR".to_owned(), "42".to_owned());
        gate.set_skill_env(Some(env.clone()));

        let captured = gate.inner.captured.lock().unwrap();
        assert_eq!(*captured, Some(env));
    }

    #[tokio::test]
    async fn mcp_tool_supervised_no_rules_allows() {
        // MCP tool with Supervised mode + from_legacy policy (no rules for MCP tool) => Ok
        let policy = crate::permissions::PermissionPolicy::from_legacy(&[], &[]);
        let gate = TrustGateExecutor::new(MockExecutor, policy);
        gate.set_effective_trust(TrustLevel::Trusted);

        let mut params = serde_json::Map::new();
        params.insert(
            "file_path".into(),
            serde_json::Value::String("/tmp/test.txt".into()),
        );
        let call = ToolCall {
            tool_id: "mcp_filesystem__read_file".into(),
            params,
        };
        let result = gate.execute_tool_call(&call).await;
        assert!(
            result.is_ok(),
            "MCP tool should be allowed when no rules exist"
        );
    }

    #[tokio::test]
    async fn bash_with_explicit_deny_rule_blocked() {
        // Bash with explicit Deny rule => Err(ToolCallBlocked)
        let policy = crate::permissions::PermissionPolicy::from_legacy(&["sudo".into()], &[]);
        let gate = TrustGateExecutor::new(MockExecutor, policy);
        gate.set_effective_trust(TrustLevel::Trusted);

        let result = gate
            .execute_tool_call(&make_call_with_cmd("bash", "sudo apt install vim"))
            .await;
        assert!(
            matches!(result, Err(ToolError::Blocked { .. })),
            "bash with explicit deny rule should be blocked"
        );
    }

    #[tokio::test]
    async fn bash_with_explicit_allow_rule_succeeds() {
        // Tool with explicit Allow rules => Ok
        let policy = crate::permissions::PermissionPolicy::from_legacy(&[], &[]);
        let gate = TrustGateExecutor::new(MockExecutor, policy);
        gate.set_effective_trust(TrustLevel::Trusted);

        let result = gate
            .execute_tool_call(&make_call_with_cmd("bash", "echo hello"))
            .await;
        assert!(
            result.is_ok(),
            "bash with explicit allow rule should succeed"
        );
    }

    #[tokio::test]
    async fn readonly_denies_mcp_tool_not_in_allowlist() {
        // ReadOnly mode must deny tools not in READONLY_TOOLS, even MCP ones.
        let policy =
            crate::permissions::PermissionPolicy::default().with_autonomy(AutonomyLevel::ReadOnly);
        let gate = TrustGateExecutor::new(MockExecutor, policy);
        gate.set_effective_trust(TrustLevel::Trusted);

        let result = gate
            .execute_tool_call(&make_call("mcpls_get_diagnostics"))
            .await;
        assert!(
            matches!(result, Err(ToolError::Blocked { .. })),
            "ReadOnly mode must deny non-allowlisted tools"
        );
    }

    #[test]
    fn set_effective_trust_interior_mutability() {
        let gate = TrustGateExecutor::new(MockExecutor, PermissionPolicy::default());
        assert_eq!(gate.effective_trust(), TrustLevel::Trusted);

        gate.set_effective_trust(TrustLevel::Quarantined);
        assert_eq!(gate.effective_trust(), TrustLevel::Quarantined);

        gate.set_effective_trust(TrustLevel::Blocked);
        assert_eq!(gate.effective_trust(), TrustLevel::Blocked);

        gate.set_effective_trust(TrustLevel::Trusted);
        assert_eq!(gate.effective_trust(), TrustLevel::Trusted);
    }
}
