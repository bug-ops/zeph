// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `PolicyGateExecutor`: wraps an inner `ToolExecutor` and enforces declarative policy
//! rules before delegating any tool call.
//!
//! Wiring order (outermost first):
//!   `PolicyGateExecutor` → `TrustGateExecutor` → `CompositeExecutor` → ...
//!
//! CRIT-03 note: legacy `execute()` / `execute_confirmed()` dispatch does NOT carry a
//! structured `tool_id`, so policy cannot be enforced there. These paths are preserved
//! for backward compat only; structured `execute_tool_call*` is the active dispatch path
//! in the agent loop.

use std::sync::Arc;

use tracing::debug;

use crate::audit::{AuditEntry, AuditLogger, AuditResult, chrono_now};
use crate::executor::{ToolCall, ToolError, ToolExecutor, ToolOutput};
use crate::policy::{PolicyContext, PolicyDecision, PolicyEnforcer};
use crate::registry::ToolDef;

/// Wraps an inner `ToolExecutor`, evaluating `PolicyEnforcer` before delegating.
///
/// Policy is only applied to `execute_tool_call` / `execute_tool_call_confirmed`.
/// Legacy `execute` / `execute_confirmed` bypass policy — see CRIT-03 note above.
pub struct PolicyGateExecutor<T: ToolExecutor> {
    inner: T,
    enforcer: Arc<PolicyEnforcer>,
    context: Arc<std::sync::RwLock<PolicyContext>>,
    audit: Option<Arc<AuditLogger>>,
}

impl<T: ToolExecutor + std::fmt::Debug> std::fmt::Debug for PolicyGateExecutor<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PolicyGateExecutor")
            .field("inner", &self.inner)
            .finish_non_exhaustive()
    }
}

impl<T: ToolExecutor> PolicyGateExecutor<T> {
    /// Create a new `PolicyGateExecutor`.
    #[must_use]
    pub fn new(
        inner: T,
        enforcer: Arc<PolicyEnforcer>,
        context: Arc<std::sync::RwLock<PolicyContext>>,
    ) -> Self {
        Self {
            inner,
            enforcer,
            context,
            audit: None,
        }
    }

    /// Attach an audit logger to record every policy decision.
    #[must_use]
    pub fn with_audit(mut self, audit: Arc<AuditLogger>) -> Self {
        self.audit = Some(audit);
        self
    }

    fn read_context(&self) -> PolicyContext {
        // parking_lot::RwLock would be preferable to avoid poisoning, but we handle
        // it gracefully here by falling back to a permissive default context.
        match self.context.read() {
            Ok(ctx) => ctx.clone(),
            Err(poisoned) => {
                tracing::warn!("PolicyContext RwLock poisoned; using poisoned value");
                poisoned.into_inner().clone()
            }
        }
    }

    /// Write the current context (called by the agent loop when trust level changes).
    pub fn update_context(&self, new_ctx: PolicyContext) {
        match self.context.write() {
            Ok(mut ctx) => *ctx = new_ctx,
            Err(poisoned) => {
                tracing::warn!("PolicyContext RwLock poisoned on write; overwriting");
                *poisoned.into_inner() = new_ctx;
            }
        }
    }

    async fn check_policy(&self, call: &ToolCall) -> Result<(), ToolError> {
        let ctx = self.read_context();
        let decision = self.enforcer.evaluate(&call.tool_id, &call.params, &ctx);

        match &decision {
            PolicyDecision::Allow { trace } => {
                debug!(tool = %call.tool_id, trace = %trace, "policy: allow");
                if let Some(audit) = &self.audit {
                    let entry = AuditEntry {
                        timestamp: chrono_now(),
                        tool: call.tool_id.clone(),
                        command: truncate_params(&call.params),
                        result: AuditResult::Success,
                        duration_ms: 0,
                    };
                    audit.log(&entry).await;
                }
                Ok(())
            }
            PolicyDecision::Deny { trace } => {
                debug!(tool = %call.tool_id, trace = %trace, "policy: deny");
                if let Some(audit) = &self.audit {
                    let entry = AuditEntry {
                        timestamp: chrono_now(),
                        tool: call.tool_id.clone(),
                        command: truncate_params(&call.params),
                        result: AuditResult::Blocked {
                            reason: trace.clone(),
                        },
                        duration_ms: 0,
                    };
                    audit.log(&entry).await;
                }
                // MED-03: return generic error to LLM; trace goes to audit only.
                Err(ToolError::Blocked {
                    command: "Tool call denied by policy".to_owned(),
                })
            }
        }
    }
}

impl<T: ToolExecutor> ToolExecutor for PolicyGateExecutor<T> {
    // CRIT-03: legacy dispatch bypasses policy — no structured tool_id available.
    async fn execute(&self, response: &str) -> Result<Option<ToolOutput>, ToolError> {
        self.inner.execute(response).await
    }

    async fn execute_confirmed(&self, response: &str) -> Result<Option<ToolOutput>, ToolError> {
        self.inner.execute_confirmed(response).await
    }

    fn tool_definitions(&self) -> Vec<ToolDef> {
        self.inner.tool_definitions()
    }

    async fn execute_tool_call(&self, call: &ToolCall) -> Result<Option<ToolOutput>, ToolError> {
        self.check_policy(call).await?;
        self.inner.execute_tool_call(call).await
    }

    // MED-04: policy is also enforced on confirmed calls — user confirmation does not
    // bypass declarative authorization.
    async fn execute_tool_call_confirmed(
        &self,
        call: &ToolCall,
    ) -> Result<Option<ToolOutput>, ToolError> {
        self.check_policy(call).await?;
        self.inner.execute_tool_call_confirmed(call).await
    }

    fn set_skill_env(&self, env: Option<std::collections::HashMap<String, String>>) {
        self.inner.set_skill_env(env);
    }

    fn set_effective_trust(&self, level: crate::TrustLevel) {
        self.inner.set_effective_trust(level);
    }

    fn is_tool_retryable(&self, tool_id: &str) -> bool {
        self.inner.is_tool_retryable(tool_id)
    }
}

fn truncate_params(params: &serde_json::Map<String, serde_json::Value>) -> String {
    let s = serde_json::to_string(params).unwrap_or_default();
    if s.chars().count() > 500 {
        let truncated: String = s.chars().take(497).collect();
        format!("{truncated}…")
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use super::*;
    use crate::TrustLevel;
    use crate::policy::{
        DefaultEffect, PolicyConfig, PolicyEffect, PolicyEnforcer, PolicyRuleConfig,
    };

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

    fn make_gate(config: PolicyConfig) -> PolicyGateExecutor<MockExecutor> {
        let enforcer = Arc::new(PolicyEnforcer::compile(&config).unwrap());
        let context = Arc::new(std::sync::RwLock::new(PolicyContext {
            trust_level: TrustLevel::Trusted,
            env: HashMap::new(),
        }));
        PolicyGateExecutor::new(MockExecutor, enforcer, context)
    }

    fn make_call(tool_id: &str) -> ToolCall {
        ToolCall {
            tool_id: tool_id.into(),
            params: serde_json::Map::new(),
        }
    }

    fn make_call_with_path(tool_id: &str, path: &str) -> ToolCall {
        let mut params = serde_json::Map::new();
        params.insert("file_path".into(), serde_json::Value::String(path.into()));
        ToolCall {
            tool_id: tool_id.into(),
            params,
        }
    }

    #[tokio::test]
    async fn allow_by_default_when_default_allow() {
        let config = PolicyConfig {
            enabled: true,
            default_effect: DefaultEffect::Allow,
            rules: vec![],
            policy_file: None,
        };
        let gate = make_gate(config);
        let result = gate.execute_tool_call(&make_call("bash")).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn deny_by_default_when_default_deny() {
        let config = PolicyConfig {
            enabled: true,
            default_effect: DefaultEffect::Deny,
            rules: vec![],
            policy_file: None,
        };
        let gate = make_gate(config);
        let result = gate.execute_tool_call(&make_call("bash")).await;
        assert!(matches!(result, Err(ToolError::Blocked { .. })));
    }

    #[tokio::test]
    async fn deny_rule_blocks_tool() {
        let config = PolicyConfig {
            enabled: true,
            default_effect: DefaultEffect::Allow,
            rules: vec![PolicyRuleConfig {
                effect: PolicyEffect::Deny,
                tool: "shell".to_owned(),
                paths: vec!["/etc/*".to_owned()],
                env: vec![],
                trust_level: None,
                args_match: None,
            }],
            policy_file: None,
        };
        let gate = make_gate(config);
        let result = gate
            .execute_tool_call(&make_call_with_path("shell", "/etc/passwd"))
            .await;
        assert!(matches!(result, Err(ToolError::Blocked { .. })));
    }

    #[tokio::test]
    async fn allow_rule_permits_tool() {
        let config = PolicyConfig {
            enabled: true,
            default_effect: DefaultEffect::Deny,
            rules: vec![PolicyRuleConfig {
                effect: PolicyEffect::Allow,
                tool: "shell".to_owned(),
                paths: vec!["/tmp/*".to_owned()],
                env: vec![],
                trust_level: None,
                args_match: None,
            }],
            policy_file: None,
        };
        let gate = make_gate(config);
        let result = gate
            .execute_tool_call(&make_call_with_path("shell", "/tmp/foo.sh"))
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn error_message_is_generic() {
        // MED-03: LLM-facing error must not reveal rule details.
        let config = PolicyConfig {
            enabled: true,
            default_effect: DefaultEffect::Deny,
            rules: vec![],
            policy_file: None,
        };
        let gate = make_gate(config);
        let err = gate
            .execute_tool_call(&make_call("bash"))
            .await
            .unwrap_err();
        if let ToolError::Blocked { command } = err {
            assert!(!command.contains("rule["), "must not leak rule index");
            assert!(!command.contains("/etc/"), "must not leak path pattern");
        } else {
            panic!("expected Blocked error");
        }
    }

    #[tokio::test]
    async fn confirmed_also_enforces_policy() {
        // MED-04: execute_tool_call_confirmed must also check policy.
        let config = PolicyConfig {
            enabled: true,
            default_effect: DefaultEffect::Deny,
            rules: vec![],
            policy_file: None,
        };
        let gate = make_gate(config);
        let result = gate.execute_tool_call_confirmed(&make_call("bash")).await;
        assert!(matches!(result, Err(ToolError::Blocked { .. })));
    }

    #[tokio::test]
    async fn legacy_execute_bypasses_policy() {
        // CRIT-03: legacy dispatch cannot be policy-checked (no tool_id).
        let config = PolicyConfig {
            enabled: true,
            default_effect: DefaultEffect::Deny,
            rules: vec![],
            policy_file: None,
        };
        let gate = make_gate(config);
        let result = gate.execute("```bash\necho hi\n```").await;
        // MockExecutor always returns None for execute().
        assert!(result.is_ok());
    }
}
