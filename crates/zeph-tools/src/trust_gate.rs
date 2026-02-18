//! Trust-level enforcement layer for tool execution.

use zeph_skills::TrustLevel;

use crate::executor::{ToolCall, ToolError, ToolExecutor, ToolOutput};
use crate::permissions::{PermissionAction, PermissionPolicy};
use crate::registry::ToolDef;

/// Tools denied when a Quarantined skill is active.
const QUARANTINE_DENIED: &[&str] = &["bash", "file_write", "web_scrape"];

/// Wraps an inner `ToolExecutor` and applies trust-level permission overlays.
#[derive(Debug)]
pub struct TrustGateExecutor<T: ToolExecutor> {
    inner: T,
    policy: PermissionPolicy,
    effective_trust: TrustLevel,
}

impl<T: ToolExecutor> TrustGateExecutor<T> {
    #[must_use]
    pub fn new(inner: T, policy: PermissionPolicy) -> Self {
        Self {
            inner,
            policy,
            effective_trust: TrustLevel::Trusted,
        }
    }

    pub fn set_effective_trust(&mut self, level: TrustLevel) {
        self.effective_trust = level;
    }

    #[must_use]
    pub fn effective_trust(&self) -> TrustLevel {
        self.effective_trust
    }

    fn check_trust(&self, tool_id: &str, input: &str) -> Result<(), ToolError> {
        match self.effective_trust {
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
        if self.effective_trust == TrustLevel::Blocked {
            return Err(ToolError::Blocked {
                command: "all tools blocked (trust=blocked)".to_owned(),
            });
        }
        self.inner.execute(response).await
    }

    async fn execute_confirmed(&self, response: &str) -> Result<Option<ToolOutput>, ToolError> {
        if self.effective_trust == TrustLevel::Blocked {
            return Err(ToolError::Blocked {
                command: "all tools blocked (trust=blocked)".to_owned(),
            });
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
            .and_then(|v| v.as_str())
            .unwrap_or("");
        self.check_trust(&call.tool_id, input)?;
        self.inner.execute_tool_call(call).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

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
            }))
        }
    }

    fn make_call(tool_id: &str) -> ToolCall {
        ToolCall {
            tool_id: tool_id.into(),
            params: HashMap::new(),
        }
    }

    fn make_call_with_cmd(tool_id: &str, cmd: &str) -> ToolCall {
        let mut params = HashMap::new();
        params.insert("command".into(), serde_json::Value::String(cmd.into()));
        ToolCall {
            tool_id: tool_id.into(),
            params,
        }
    }

    #[tokio::test]
    async fn trusted_allows_all() {
        let mut gate = TrustGateExecutor::new(MockExecutor, PermissionPolicy::default());
        gate.set_effective_trust(TrustLevel::Trusted);

        let result = gate.execute_tool_call(&make_call("bash")).await;
        // Default policy returns Ask for unknown tools
        assert!(matches!(
            result,
            Err(ToolError::ConfirmationRequired { .. })
        ));
    }

    #[tokio::test]
    async fn quarantined_denies_bash() {
        let mut gate = TrustGateExecutor::new(MockExecutor, PermissionPolicy::default());
        gate.set_effective_trust(TrustLevel::Quarantined);

        let result = gate.execute_tool_call(&make_call("bash")).await;
        assert!(matches!(result, Err(ToolError::Blocked { .. })));
    }

    #[tokio::test]
    async fn quarantined_denies_file_write() {
        let mut gate = TrustGateExecutor::new(MockExecutor, PermissionPolicy::default());
        gate.set_effective_trust(TrustLevel::Quarantined);

        let result = gate.execute_tool_call(&make_call("file_write")).await;
        assert!(matches!(result, Err(ToolError::Blocked { .. })));
    }

    #[tokio::test]
    async fn quarantined_allows_file_read() {
        let policy = crate::permissions::PermissionPolicy::from_legacy(&[], &[]);
        let mut gate = TrustGateExecutor::new(MockExecutor, policy);
        gate.set_effective_trust(TrustLevel::Quarantined);

        let result = gate.execute_tool_call(&make_call("file_read")).await;
        // file_read is not in quarantine denied list, and policy has no rules for file_read => Ask
        assert!(matches!(
            result,
            Err(ToolError::ConfirmationRequired { .. })
        ));
    }

    #[tokio::test]
    async fn blocked_denies_everything() {
        let mut gate = TrustGateExecutor::new(MockExecutor, PermissionPolicy::default());
        gate.set_effective_trust(TrustLevel::Blocked);

        let result = gate.execute_tool_call(&make_call("file_read")).await;
        assert!(matches!(result, Err(ToolError::Blocked { .. })));
    }

    #[tokio::test]
    async fn policy_deny_overrides_trust() {
        let policy = crate::permissions::PermissionPolicy::from_legacy(&["sudo".into()], &[]);
        let mut gate = TrustGateExecutor::new(MockExecutor, policy);
        gate.set_effective_trust(TrustLevel::Trusted);

        let result = gate
            .execute_tool_call(&make_call_with_cmd("bash", "sudo rm"))
            .await;
        assert!(matches!(result, Err(ToolError::Blocked { .. })));
    }

    #[tokio::test]
    async fn blocked_denies_execute() {
        let mut gate = TrustGateExecutor::new(MockExecutor, PermissionPolicy::default());
        gate.set_effective_trust(TrustLevel::Blocked);

        let result = gate.execute("some response").await;
        assert!(matches!(result, Err(ToolError::Blocked { .. })));
    }

    #[tokio::test]
    async fn blocked_denies_execute_confirmed() {
        let mut gate = TrustGateExecutor::new(MockExecutor, PermissionPolicy::default());
        gate.set_effective_trust(TrustLevel::Blocked);

        let result = gate.execute_confirmed("some response").await;
        assert!(matches!(result, Err(ToolError::Blocked { .. })));
    }

    #[tokio::test]
    async fn trusted_allows_execute() {
        let mut gate = TrustGateExecutor::new(MockExecutor, PermissionPolicy::default());
        gate.set_effective_trust(TrustLevel::Trusted);

        let result = gate.execute("some response").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn verified_with_allow_policy_succeeds() {
        let policy = crate::permissions::PermissionPolicy::from_legacy(&[], &[]);
        let mut gate = TrustGateExecutor::new(MockExecutor, policy);
        gate.set_effective_trust(TrustLevel::Verified);

        let result = gate
            .execute_tool_call(&make_call_with_cmd("bash", "echo hi"))
            .await
            .unwrap();
        assert!(result.is_some());
    }
}
