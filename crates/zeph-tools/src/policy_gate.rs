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

use parking_lot::RwLock;
use tracing::debug;

use crate::audit::{AuditEntry, AuditLogger, AuditResult, chrono_now};
use crate::executor::{ToolCall, ToolError, ToolExecutor, ToolOutput};
use crate::policy::{PolicyContext, PolicyDecision, PolicyEnforcer};
use crate::registry::ToolDef;

/// Shared risk level from spec 050 `TrajectorySentinel`.
///
/// Stored as `u8` to avoid a direct dep on `zeph-core`; mapping:
/// `0` = Calm, `1` = Elevated, `2` = High, `3` = Critical.
/// Written by the agent loop after each `sentinel.current_risk()` call.
/// Read by `check_policy` — an `Allow` decision is downgraded to `Deny` at `3` (Critical).
pub type TrajectoryRiskSlot = Arc<parking_lot::RwLock<u8>>;

/// Callback invoked by executors in `zeph-tools` to record a risk signal into the sentinel
/// that lives in `zeph-core`, avoiding a reverse crate dependency.
///
/// The `u8` argument is a `RiskSignalCode` — see `crates/zeph-core/src/agent/trajectory.rs`.
pub type RiskSignalSink = Arc<dyn Fn(u8) + Send + Sync>;

/// Lock-free pending signal queue shared between executor layers and the agent loop.
///
/// Executors push `u8` signal codes; `begin_turn()` drains the queue and calls
/// `TrajectorySentinel::record()` for each entry. This avoids a reverse crate dependency
/// between `zeph-tools` and `zeph-core`.
pub type RiskSignalQueue = Arc<parking_lot::Mutex<Vec<u8>>>;

/// Wraps an inner `ToolExecutor`, evaluating `PolicyEnforcer` before delegating.
///
/// Policy is only applied to `execute_tool_call` / `execute_tool_call_confirmed`.
/// Legacy `execute` / `execute_confirmed` bypass policy — see CRIT-03 note above.
pub struct PolicyGateExecutor<T: ToolExecutor> {
    inner: T,
    enforcer: Arc<PolicyEnforcer>,
    context: Arc<RwLock<PolicyContext>>,
    audit: Option<Arc<AuditLogger>>,
    /// Optional trajectory risk level slot injected by the agent loop (spec 050).
    /// When `Some` and the value is `3` (Critical), all `Allow` decisions are downgraded.
    trajectory_risk: Option<TrajectoryRiskSlot>,
    /// Optional signal queue — `PolicyDeny` codes are pushed here; drained by `begin_turn()`.
    signal_queue: Option<RiskSignalQueue>,
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
        context: Arc<RwLock<PolicyContext>>,
    ) -> Self {
        Self {
            inner,
            enforcer,
            context,
            audit: None,
            trajectory_risk: None,
            signal_queue: None,
        }
    }

    /// Attach an audit logger to record every policy decision.
    #[must_use]
    pub fn with_audit(mut self, audit: Arc<AuditLogger>) -> Self {
        self.audit = Some(audit);
        self
    }

    /// Attach a trajectory risk slot (spec 050).
    ///
    /// When the slot value reaches `3` (Critical), any `Allow` decision from the policy
    /// enforcer is downgraded to `Deny` with `error_category = "trajectory_critical_downgrade"`.
    #[must_use]
    pub fn with_trajectory_risk(mut self, slot: TrajectoryRiskSlot) -> Self {
        self.trajectory_risk = Some(slot);
        self
    }

    /// Attach a shared signal queue so `PolicyDeny` decisions are recorded in the sentinel.
    ///
    /// The agent loop (`begin_turn`) drains the queue and feeds signals to the sentinel.
    #[must_use]
    pub fn with_signal_queue(mut self, queue: RiskSignalQueue) -> Self {
        self.signal_queue = Some(queue);
        self
    }

    fn push_signal(&self, code: u8) {
        if let Some(ref q) = self.signal_queue {
            q.lock().push(code);
        }
    }

    fn read_context(&self) -> PolicyContext {
        self.context.read().clone()
    }

    /// Write the current context (called by the agent loop when trust level changes).
    pub fn update_context(&self, new_ctx: PolicyContext) {
        *self.context.write() = new_ctx;
    }

    /// Return `true` when the trajectory sentinel is at Critical (spec 050).
    fn is_trajectory_critical(&self) -> bool {
        self.trajectory_risk
            .as_ref()
            .is_some_and(|slot| *slot.read() >= 3)
    }

    async fn log_audit(&self, call: &ToolCall, result: AuditResult, error_category: Option<&str>) {
        let Some(audit) = &self.audit else { return };
        let entry = AuditEntry {
            timestamp: chrono_now(),
            tool: call.tool_id.clone(),
            command: truncate_params(&call.params),
            result,
            duration_ms: 0,
            error_category: error_category.map(str::to_owned),
            error_domain: error_category.map(|_| "security".to_owned()),
            error_phase: None,
            claim_source: None,
            mcp_server_id: None,
            injection_flagged: false,
            embedding_anomalous: false,
            cross_boundary_mcp_to_acp: false,
            adversarial_policy_decision: None,
            exit_code: None,
            truncated: false,
            caller_id: call.caller_id.clone(),
            policy_match: None,
            correlation_id: None,
            vigil_risk: None,
            execution_env: None,
            resolved_cwd: None,
            scope_at_definition: None,
            scope_at_dispatch: None,
        };
        audit.log(&entry).await;
    }

    async fn check_policy(&self, call: &ToolCall) -> Result<(), ToolError> {
        // Spec 050: at Critical risk level, deny ALL tool calls before policy evaluation.
        if self.is_trajectory_critical() {
            tracing::warn!(tool = %call.tool_id, "trajectory sentinel at Critical: denied (spec 050)");
            self.log_audit(
                call,
                AuditResult::Blocked {
                    reason: "trajectory_critical_downgrade".to_owned(),
                },
                Some("trajectory_critical_downgrade"),
            )
            .await;
            return Err(ToolError::Blocked {
                command: "Tool call denied by policy".to_owned(),
            });
        }

        let ctx = self.read_context();
        let decision = self
            .enforcer
            .evaluate(call.tool_id.as_str(), &call.params, &ctx);

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
                        error_category: None,
                        error_domain: None,
                        error_phase: None,
                        claim_source: None,
                        mcp_server_id: None,
                        injection_flagged: false,
                        embedding_anomalous: false,
                        cross_boundary_mcp_to_acp: false,
                        adversarial_policy_decision: None,
                        exit_code: None,
                        truncated: false,
                        caller_id: call.caller_id.clone(),
                        policy_match: Some(trace.clone()),
                        correlation_id: None,
                        vigil_risk: None,
                        execution_env: None,
                        resolved_cwd: None,
                        scope_at_definition: None,
                        scope_at_dispatch: None,
                    };
                    audit.log(&entry).await;
                }
                Ok(())
            }
            PolicyDecision::Deny { trace } => {
                debug!(tool = %call.tool_id, trace = %trace, "policy: deny");
                // Signal code 1 = PolicyDeny (matches RiskSignal::PolicyDeny in zeph-core).
                self.push_signal(1);
                if let Some(audit) = &self.audit {
                    let entry = AuditEntry {
                        timestamp: chrono_now(),
                        tool: call.tool_id.clone(),
                        command: truncate_params(&call.params),
                        result: AuditResult::Blocked {
                            reason: trace.clone(),
                        },
                        duration_ms: 0,
                        error_category: Some("policy_blocked".to_owned()),
                        error_domain: Some("action".to_owned()),
                        error_phase: None,
                        claim_source: None,
                        mcp_server_id: None,
                        injection_flagged: false,
                        embedding_anomalous: false,
                        cross_boundary_mcp_to_acp: false,
                        adversarial_policy_decision: None,
                        exit_code: None,
                        truncated: false,
                        caller_id: call.caller_id.clone(),
                        policy_match: Some(trace.clone()),
                        correlation_id: None,
                        vigil_risk: None,
                        execution_env: None,
                        resolved_cwd: None,
                        scope_at_definition: None,
                        scope_at_dispatch: None,
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
    // CRIT-03: legacy unstructured dispatch has no tool_id; policy cannot be enforced.
    // PolicyGateExecutor is only constructed when policy is enabled, so reject unconditionally.
    async fn execute(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
        Err(ToolError::Blocked {
            command:
                "legacy unstructured dispatch is not supported when policy enforcement is enabled"
                    .into(),
        })
    }

    async fn execute_confirmed(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
        Err(ToolError::Blocked {
            command:
                "legacy unstructured dispatch is not supported when policy enforcement is enabled"
                    .into(),
        })
    }

    fn tool_definitions(&self) -> Vec<ToolDef> {
        self.inner.tool_definitions()
    }

    async fn execute_tool_call(&self, call: &ToolCall) -> Result<Option<ToolOutput>, ToolError> {
        self.check_policy(call).await?;
        let result = self.inner.execute_tool_call(call).await;
        // Populate mcp_server_id in audit when the inner executor produces MCP output.
        // MCP tool outputs use qualified_name() format: "server_id:tool_name".
        if let Ok(Some(ref output)) = result
            && let Some(colon) = output.tool_name.as_str().find(':')
        {
            let server_id = output.tool_name.as_str()[..colon].to_owned();
            if let Some(audit) = &self.audit {
                let entry = AuditEntry {
                    timestamp: chrono_now(),
                    tool: call.tool_id.clone(),
                    command: truncate_params(&call.params),
                    result: AuditResult::Success,
                    duration_ms: 0,
                    error_category: None,
                    error_domain: None,
                    error_phase: None,
                    claim_source: None,
                    mcp_server_id: Some(server_id),
                    injection_flagged: false,
                    embedding_anomalous: false,
                    cross_boundary_mcp_to_acp: false,
                    adversarial_policy_decision: None,
                    exit_code: None,
                    truncated: false,
                    caller_id: call.caller_id.clone(),
                    policy_match: None,
                    correlation_id: None,
                    vigil_risk: None,
                    execution_env: None,
                    resolved_cwd: None,
                    scope_at_definition: None,
                    scope_at_dispatch: None,
                };
                audit.log(&entry).await;
            }
        }
        result
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

    fn set_effective_trust(&self, level: crate::SkillTrustLevel) {
        self.context.write().trust_level = level;
        self.inner.set_effective_trust(level);
    }

    fn is_tool_retryable(&self, tool_id: &str) -> bool {
        self.inner.is_tool_retryable(tool_id)
    }

    fn is_tool_speculatable(&self, tool_id: &str) -> bool {
        self.inner.is_tool_speculatable(tool_id)
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
    use crate::SkillTrustLevel;
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
                claim_source: None,
            }))
        }
    }

    fn make_gate(config: &PolicyConfig) -> PolicyGateExecutor<MockExecutor> {
        let enforcer = Arc::new(PolicyEnforcer::compile(config).unwrap());
        let context = Arc::new(RwLock::new(PolicyContext {
            trust_level: SkillTrustLevel::Trusted,
            env: HashMap::new(),
        }));
        PolicyGateExecutor::new(MockExecutor, enforcer, context)
    }

    fn make_call(tool_id: &str) -> ToolCall {
        ToolCall {
            tool_id: tool_id.into(),
            params: serde_json::Map::new(),
            caller_id: None,
            context: None,
        }
    }

    fn make_call_with_path(tool_id: &str, path: &str) -> ToolCall {
        let mut params = serde_json::Map::new();
        params.insert("file_path".into(), serde_json::Value::String(path.into()));
        ToolCall {
            tool_id: tool_id.into(),
            params,
            caller_id: None,
            context: None,
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
        let gate = make_gate(&config);
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
        let gate = make_gate(&config);
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
                tool: "shell".into(),
                paths: vec!["/etc/*".to_owned()],
                env: vec![],
                trust_level: None,
                args_match: None,
                capabilities: vec![],
            }],
            policy_file: None,
        };
        let gate = make_gate(&config);
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
                tool: "shell".into(),
                paths: vec!["/tmp/*".to_owned()],
                env: vec![],
                trust_level: None,
                args_match: None,
                capabilities: vec![],
            }],
            policy_file: None,
        };
        let gate = make_gate(&config);
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
        let gate = make_gate(&config);
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
        let gate = make_gate(&config);
        let result = gate.execute_tool_call_confirmed(&make_call("bash")).await;
        assert!(matches!(result, Err(ToolError::Blocked { .. })));
    }

    // GAP-05: execute_tool_call_confirmed allow path must delegate to inner executor.
    #[tokio::test]
    async fn confirmed_allow_delegates_to_inner() {
        let config = PolicyConfig {
            enabled: true,
            default_effect: DefaultEffect::Allow,
            rules: vec![],
            policy_file: None,
        };
        let gate = make_gate(&config);
        let call = make_call("shell");
        let result = gate.execute_tool_call_confirmed(&call).await;
        assert!(result.is_ok(), "allow path must not return an error");
        let output = result.unwrap();
        assert!(
            output.is_some(),
            "inner executor must be invoked and return output on allow"
        );
        assert_eq!(
            output.unwrap().tool_name,
            "shell",
            "output tool_name must match the confirmed call"
        );
    }

    #[tokio::test]
    async fn legacy_execute_blocked_when_policy_enabled() {
        // CRIT-03: legacy dispatch has no tool_id; policy cannot be enforced.
        // PolicyGateExecutor must reject it unconditionally when policy is enabled.
        let config = PolicyConfig {
            enabled: true,
            default_effect: DefaultEffect::Deny,
            rules: vec![],
            policy_file: None,
        };
        let gate = make_gate(&config);
        let result = gate.execute("```bash\necho hi\n```").await;
        assert!(matches!(result, Err(ToolError::Blocked { .. })));
        let result_confirmed = gate.execute_confirmed("```bash\necho hi\n```").await;
        assert!(matches!(result_confirmed, Err(ToolError::Blocked { .. })));
    }

    // GAP-06: set_effective_trust must update PolicyContext.trust_level so trust_level rules
    // are evaluated against the actual invoking skill trust, not the hardcoded Trusted default.
    #[tokio::test]
    async fn set_effective_trust_quarantined_blocks_verified_threshold_rule() {
        // Rule: allow shell when trust_level = Verified (threshold severity=1).
        // Context set to Quarantined (severity=2) via set_effective_trust.
        // Expected: context.severity(2) > threshold.severity(1) → rule does not fire → Deny.
        let config = PolicyConfig {
            enabled: true,
            default_effect: DefaultEffect::Deny,
            rules: vec![PolicyRuleConfig {
                effect: PolicyEffect::Allow,
                tool: "shell".into(),
                paths: vec![],
                env: vec![],
                trust_level: Some(SkillTrustLevel::Verified),
                args_match: None,
                capabilities: vec![],
            }],
            policy_file: None,
        };
        let gate = make_gate(&config);
        gate.set_effective_trust(SkillTrustLevel::Quarantined);
        let result = gate.execute_tool_call(&make_call("shell")).await;
        assert!(
            matches!(result, Err(ToolError::Blocked { .. })),
            "Quarantined context must not satisfy a Verified trust threshold allow rule"
        );
    }

    #[tokio::test]
    async fn set_effective_trust_trusted_satisfies_verified_threshold_rule() {
        // Rule: allow shell when trust_level = Verified (threshold severity=1).
        // Context set to Trusted (severity=0) via set_effective_trust.
        // Expected: context.severity(0) <= threshold.severity(1) → rule fires → Allow.
        let config = PolicyConfig {
            enabled: true,
            default_effect: DefaultEffect::Deny,
            rules: vec![PolicyRuleConfig {
                effect: PolicyEffect::Allow,
                tool: "shell".into(),
                paths: vec![],
                env: vec![],
                trust_level: Some(SkillTrustLevel::Verified),
                args_match: None,
                capabilities: vec![],
            }],
            policy_file: None,
        };
        let gate = make_gate(&config);
        gate.set_effective_trust(SkillTrustLevel::Trusted);
        let result = gate.execute_tool_call(&make_call("shell")).await;
        assert!(
            result.is_ok(),
            "Trusted context must satisfy a Verified trust threshold allow rule"
        );
    }

    // GAP-1: trajectory_risk_slot at Critical (3) must downgrade Allow to Deny.
    #[tokio::test]
    async fn critical_trajectory_blocks_any_allow() {
        let config = PolicyConfig {
            enabled: true,
            default_effect: DefaultEffect::Allow,
            rules: vec![],
            policy_file: None,
        };
        let slot: TrajectoryRiskSlot = Arc::new(RwLock::new(3u8)); // Critical
        let gate = make_gate(&config).with_trajectory_risk(slot);
        let result = gate.execute_tool_call(&make_call("builtin:shell")).await;
        assert!(
            matches!(result, Err(ToolError::Blocked { .. })),
            "Critical trajectory must block even policy-allowed tool calls"
        );
        // LLM isolation: error message must not reveal risk level.
        if let Err(ToolError::Blocked { command }) = result {
            assert!(
                !command.contains("Critical") && !command.contains("trajectory"),
                "error message must not leak risk info to LLM: got '{command}'"
            );
        }
    }

    // Corollary: slot at High (2) must NOT downgrade (only Critical does).
    #[tokio::test]
    async fn high_trajectory_does_not_block_allowed_tool() {
        let config = PolicyConfig {
            enabled: true,
            default_effect: DefaultEffect::Allow,
            rules: vec![],
            policy_file: None,
        };
        let slot: TrajectoryRiskSlot = Arc::new(RwLock::new(2u8)); // High
        let gate = make_gate(&config).with_trajectory_risk(slot);
        let result = gate.execute_tool_call(&make_call("builtin:shell")).await;
        assert!(
            result.is_ok(),
            "High (not Critical) must not block allowed tool calls"
        );
    }
}
