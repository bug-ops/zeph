// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `AdversarialPolicyGateExecutor`: wraps an inner `ToolExecutor` and runs an LLM-based
//! policy check before delegating any structured tool call.
//!
//! Wiring order (outermost first):
//!   `PolicyGateExecutor` → `AdversarialPolicyGateExecutor` → `TrustGateExecutor` → ...
//!
//! Per CRIT-04 recommendation: declarative `PolicyGateExecutor` is outermost.
//! Adversarial gate only fires for calls that pass declarative policy — no duplication.
//!
//! Per CRIT-06: ALL `ToolExecutor` trait methods are delegated to `self.inner`.
//! Per CRIT-01: fail behavior (allow/deny on LLM error) is controlled by `fail_open` config.
//! Per CRIT-11: params are sanitized and wrapped in code fences before LLM call.

use std::sync::Arc;

use crate::adversarial_policy::{PolicyDecision, PolicyLlmClient, PolicyValidator};
use crate::audit::{AuditEntry, AuditLogger, AuditResult, chrono_now};
use crate::executor::{ClaimSource, ToolCall, ToolError, ToolExecutor, ToolOutput};
use crate::registry::ToolDef;

/// Wraps an inner `ToolExecutor`, running an LLM-based adversarial policy check
/// before delegating structured tool calls.
///
/// Only `execute_tool_call` and `execute_tool_call_confirmed` are intercepted.
/// Legacy `execute` / `execute_confirmed` bypass the check (no structured `tool_id`).
pub struct AdversarialPolicyGateExecutor<T: ToolExecutor> {
    inner: T,
    validator: Arc<PolicyValidator>,
    llm: Arc<dyn PolicyLlmClient>,
    audit: Option<Arc<AuditLogger>>,
}

impl<T: ToolExecutor + std::fmt::Debug> std::fmt::Debug for AdversarialPolicyGateExecutor<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdversarialPolicyGateExecutor")
            .field("inner", &self.inner)
            .finish_non_exhaustive()
    }
}

impl<T: ToolExecutor> AdversarialPolicyGateExecutor<T> {
    /// Create a new `AdversarialPolicyGateExecutor`.
    #[must_use]
    pub fn new(inner: T, validator: Arc<PolicyValidator>, llm: Arc<dyn PolicyLlmClient>) -> Self {
        Self {
            inner,
            validator,
            llm,
            audit: None,
        }
    }

    /// Attach an audit logger.
    #[must_use]
    pub fn with_audit(mut self, audit: Arc<AuditLogger>) -> Self {
        self.audit = Some(audit);
        self
    }

    async fn check_policy(&self, call: &ToolCall) -> Result<(), ToolError> {
        tracing::info!(
            tool = %call.tool_id,
            status_spinner = true,
            "Validating tool policy\u{2026}"
        );

        let decision = self
            .validator
            .validate(&call.tool_id, &call.params, self.llm.as_ref())
            .await;

        match decision {
            PolicyDecision::Allow => {
                tracing::debug!(tool = %call.tool_id, "adversarial policy: allow");
                self.write_audit(call, "allow", AuditResult::Success, None)
                    .await;
                Ok(())
            }
            PolicyDecision::Deny { reason } => {
                tracing::warn!(
                    tool = %call.tool_id,
                    reason = %reason,
                    "adversarial policy: deny"
                );
                self.write_audit(
                    call,
                    &format!("deny:{reason}"),
                    AuditResult::Blocked {
                        reason: reason.clone(),
                    },
                    None,
                )
                .await;
                // MED-03: do NOT surface the LLM reason to the main LLM.
                Err(ToolError::Blocked {
                    command: "[adversarial] Tool call denied by policy".to_owned(),
                })
            }
            PolicyDecision::Error { message } => {
                tracing::warn!(
                    tool = %call.tool_id,
                    error = %message,
                    fail_open = self.validator.fail_open(),
                    "adversarial policy: LLM error"
                );
                if self.validator.fail_open() {
                    self.write_audit(
                        call,
                        &format!("error:{message}"),
                        AuditResult::Success,
                        None,
                    )
                    .await;
                    Ok(())
                } else {
                    self.write_audit(
                        call,
                        &format!("error:{message}"),
                        AuditResult::Blocked {
                            reason: "adversarial policy LLM error (fail-closed)".to_owned(),
                        },
                        None,
                    )
                    .await;
                    Err(ToolError::Blocked {
                        command: "[adversarial] Tool call denied: policy check failed".to_owned(),
                    })
                }
            }
        }
    }

    async fn write_audit(
        &self,
        call: &ToolCall,
        decision: &str,
        result: AuditResult,
        claim_source: Option<ClaimSource>,
    ) {
        let Some(audit) = &self.audit else { return };
        let entry = AuditEntry {
            timestamp: chrono_now(),
            tool: call.tool_id.clone(),
            command: params_summary(&call.params),
            result,
            duration_ms: 0,
            error_category: None,
            error_domain: None,
            error_phase: None,
            claim_source,
            mcp_server_id: None,
            injection_flagged: false,
            embedding_anomalous: false,
            cross_boundary_mcp_to_acp: false,
            adversarial_policy_decision: Some(decision.to_owned()),
            exit_code: None,
            truncated: false,
            caller_id: call.caller_id.clone(),
            policy_match: None,
        };
        audit.log(&entry).await;
    }
}

impl<T: ToolExecutor> ToolExecutor for AdversarialPolicyGateExecutor<T> {
    // Legacy dispatch bypasses adversarial check — no structured tool_id available.
    async fn execute(&self, response: &str) -> Result<Option<ToolOutput>, ToolError> {
        self.inner.execute(response).await
    }

    async fn execute_confirmed(&self, response: &str) -> Result<Option<ToolOutput>, ToolError> {
        self.inner.execute_confirmed(response).await
    }

    // CRIT-06: delegate all pass-through methods to inner executor.
    fn tool_definitions(&self) -> Vec<ToolDef> {
        self.inner.tool_definitions()
    }

    async fn execute_tool_call(&self, call: &ToolCall) -> Result<Option<ToolOutput>, ToolError> {
        self.check_policy(call).await?;
        let output = self.inner.execute_tool_call(call).await?;
        if let Some(ref out) = output {
            self.write_audit(
                call,
                "allow:executed",
                AuditResult::Success,
                out.claim_source,
            )
            .await;
        }
        Ok(output)
    }

    // MED-04: policy also enforced on confirmed calls.
    async fn execute_tool_call_confirmed(
        &self,
        call: &ToolCall,
    ) -> Result<Option<ToolOutput>, ToolError> {
        self.check_policy(call).await?;
        let output = self.inner.execute_tool_call_confirmed(call).await?;
        if let Some(ref out) = output {
            self.write_audit(
                call,
                "allow:executed",
                AuditResult::Success,
                out.claim_source,
            )
            .await;
        }
        Ok(output)
    }

    fn set_skill_env(&self, env: Option<std::collections::HashMap<String, String>>) {
        self.inner.set_skill_env(env);
    }

    fn set_effective_trust(&self, level: crate::SkillTrustLevel) {
        self.inner.set_effective_trust(level);
    }

    fn is_tool_retryable(&self, tool_id: &str) -> bool {
        self.inner.is_tool_retryable(tool_id)
    }
}

fn params_summary(params: &serde_json::Map<String, serde_json::Value>) -> String {
    let s = serde_json::to_string(params).unwrap_or_default();
    if s.chars().count() > 500 {
        let truncated: String = s.chars().take(497).collect();
        format!("{truncated}\u{2026}")
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::*;
    use crate::adversarial_policy::{PolicyMessage, PolicyValidator};
    use crate::executor::{ToolCall, ToolOutput};

    // --- Mock LLM client ---

    struct MockLlm {
        response: String,
        call_count: Arc<AtomicUsize>,
    }

    impl MockLlm {
        fn new(response: impl Into<String>) -> (Arc<AtomicUsize>, Self) {
            let counter = Arc::new(AtomicUsize::new(0));
            let client = Self {
                response: response.into(),
                call_count: Arc::clone(&counter),
            };
            (counter, client)
        }
    }

    impl PolicyLlmClient for MockLlm {
        fn chat<'a>(
            &'a self,
            _messages: &'a [PolicyMessage],
        ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            let resp = self.response.clone();
            Box::pin(async move { Ok(resp) })
        }
    }

    // --- Mock inner executor ---

    #[derive(Debug)]
    struct MockInner {
        call_count: Arc<AtomicUsize>,
    }

    impl MockInner {
        fn new() -> (Arc<AtomicUsize>, Self) {
            let counter = Arc::new(AtomicUsize::new(0));
            let exec = Self {
                call_count: Arc::clone(&counter),
            };
            (counter, exec)
        }
    }

    impl ToolExecutor for MockInner {
        async fn execute(&self, _: &str) -> Result<Option<ToolOutput>, ToolError> {
            Ok(None)
        }

        async fn execute_tool_call(
            &self,
            call: &ToolCall,
        ) -> Result<Option<ToolOutput>, ToolError> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
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

    fn make_call(tool_id: &str) -> ToolCall {
        ToolCall {
            tool_id: tool_id.into(),
            params: serde_json::Map::new(),
            caller_id: None,
        }
    }

    fn make_validator(fail_open: bool) -> Arc<PolicyValidator> {
        Arc::new(PolicyValidator::new(
            vec!["test policy".to_owned()],
            Duration::from_millis(500),
            fail_open,
            Vec::new(),
        ))
    }

    #[tokio::test]
    async fn allow_path_delegates_to_inner() {
        let (llm_count, llm) = MockLlm::new("ALLOW");
        let (inner_count, inner) = MockInner::new();
        let gate = AdversarialPolicyGateExecutor::new(inner, make_validator(false), Arc::new(llm));
        let result = gate.execute_tool_call(&make_call("shell")).await;
        assert!(result.is_ok());
        assert_eq!(
            llm_count.load(Ordering::SeqCst),
            1,
            "LLM must be called once"
        );
        assert_eq!(
            inner_count.load(Ordering::SeqCst),
            1,
            "inner executor must be called on allow"
        );
    }

    #[tokio::test]
    async fn deny_path_blocks_and_does_not_call_inner() {
        let (llm_count, llm) = MockLlm::new("DENY: unsafe command");
        let (inner_count, inner) = MockInner::new();
        let gate = AdversarialPolicyGateExecutor::new(inner, make_validator(false), Arc::new(llm));
        let result = gate.execute_tool_call(&make_call("shell")).await;
        assert!(matches!(result, Err(ToolError::Blocked { .. })));
        assert_eq!(llm_count.load(Ordering::SeqCst), 1);
        assert_eq!(
            inner_count.load(Ordering::SeqCst),
            0,
            "inner must NOT be called on deny"
        );
    }

    #[tokio::test]
    async fn error_message_is_opaque() {
        // MED-03: error returned to main LLM must not contain the LLM denial reason.
        let (_, llm) = MockLlm::new("DENY: secret internal policy rule XYZ");
        let (_, inner) = MockInner::new();
        let gate = AdversarialPolicyGateExecutor::new(inner, make_validator(false), Arc::new(llm));
        let err = gate
            .execute_tool_call(&make_call("shell"))
            .await
            .unwrap_err();
        if let ToolError::Blocked { command } = err {
            assert!(
                !command.contains("secret internal policy rule XYZ"),
                "LLM denial reason must not leak to main LLM"
            );
        } else {
            panic!("expected Blocked error");
        }
    }

    #[tokio::test]
    async fn fail_closed_blocks_on_llm_error() {
        struct FailingLlm;
        impl PolicyLlmClient for FailingLlm {
            fn chat<'a>(
                &'a self,
                _: &'a [PolicyMessage],
            ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
                Box::pin(async { Err("network error".to_owned()) })
            }
        }

        let (_, inner) = MockInner::new();
        let gate = AdversarialPolicyGateExecutor::new(
            inner,
            make_validator(false), // fail_open = false
            Arc::new(FailingLlm),
        );
        let result = gate.execute_tool_call(&make_call("shell")).await;
        assert!(
            matches!(result, Err(ToolError::Blocked { .. })),
            "fail-closed must block on LLM error"
        );
    }

    #[tokio::test]
    async fn fail_open_allows_on_llm_error() {
        struct FailingLlm;
        impl PolicyLlmClient for FailingLlm {
            fn chat<'a>(
                &'a self,
                _: &'a [PolicyMessage],
            ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
                Box::pin(async { Err("network error".to_owned()) })
            }
        }

        let (inner_count, inner) = MockInner::new();
        let gate = AdversarialPolicyGateExecutor::new(
            inner,
            make_validator(true), // fail_open = true
            Arc::new(FailingLlm),
        );
        let result = gate.execute_tool_call(&make_call("shell")).await;
        assert!(result.is_ok(), "fail-open must allow on LLM error");
        assert_eq!(inner_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn confirmed_also_enforces_policy() {
        let (_, llm) = MockLlm::new("DENY: blocked");
        let (_, inner) = MockInner::new();
        let gate = AdversarialPolicyGateExecutor::new(inner, make_validator(false), Arc::new(llm));
        let result = gate.execute_tool_call_confirmed(&make_call("shell")).await;
        assert!(
            matches!(result, Err(ToolError::Blocked { .. })),
            "confirmed path must also enforce adversarial policy"
        );
    }

    #[tokio::test]
    async fn legacy_execute_bypasses_policy() {
        let (llm_count, llm) = MockLlm::new("DENY: anything");
        let (_, inner) = MockInner::new();
        let gate = AdversarialPolicyGateExecutor::new(inner, make_validator(false), Arc::new(llm));
        let result = gate.execute("```shell\necho hi\n```").await;
        assert!(
            result.is_ok(),
            "legacy execute must bypass adversarial policy"
        );
        assert_eq!(
            llm_count.load(Ordering::SeqCst),
            0,
            "LLM must NOT be called for legacy dispatch"
        );
    }

    #[tokio::test]
    async fn delegation_set_skill_env() {
        // Verify that set_skill_env reaches the inner executor without panic.
        let (_, llm) = MockLlm::new("ALLOW");
        let (_, inner) = MockInner::new();
        let gate = AdversarialPolicyGateExecutor::new(inner, make_validator(false), Arc::new(llm));
        gate.set_skill_env(None);
    }

    #[tokio::test]
    async fn delegation_set_effective_trust() {
        use crate::SkillTrustLevel;
        let (_, llm) = MockLlm::new("ALLOW");
        let (_, inner) = MockInner::new();
        let gate = AdversarialPolicyGateExecutor::new(inner, make_validator(false), Arc::new(llm));
        gate.set_effective_trust(SkillTrustLevel::Trusted);
    }

    #[tokio::test]
    async fn delegation_is_tool_retryable() {
        let (_, llm) = MockLlm::new("ALLOW");
        let (_, inner) = MockInner::new();
        let gate = AdversarialPolicyGateExecutor::new(inner, make_validator(false), Arc::new(llm));
        let retryable = gate.is_tool_retryable("shell");
        assert!(!retryable, "MockInner returns false for is_tool_retryable");
    }

    #[tokio::test]
    async fn delegation_tool_definitions() {
        let (_, llm) = MockLlm::new("ALLOW");
        let (_, inner) = MockInner::new();
        let gate = AdversarialPolicyGateExecutor::new(inner, make_validator(false), Arc::new(llm));
        let defs = gate.tool_definitions();
        assert!(defs.is_empty(), "MockInner returns empty tool definitions");
    }

    #[tokio::test]
    async fn audit_entry_contains_adversarial_decision() {
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let log_path = dir.path().join("audit.log");
        let audit_config = crate::config::AuditConfig {
            enabled: true,
            destination: log_path.display().to_string(),
            ..Default::default()
        };
        let audit_logger = Arc::new(
            crate::audit::AuditLogger::from_config(&audit_config)
                .await
                .unwrap(),
        );

        let (_, llm) = MockLlm::new("ALLOW");
        let (_, inner) = MockInner::new();
        let gate = AdversarialPolicyGateExecutor::new(inner, make_validator(false), Arc::new(llm))
            .with_audit(Arc::clone(&audit_logger));

        gate.execute_tool_call(&make_call("shell")).await.unwrap();

        let content = tokio::fs::read_to_string(&log_path).await.unwrap();
        assert!(
            content.contains("adversarial_policy_decision"),
            "audit entry must contain adversarial_policy_decision field"
        );
        assert!(
            content.contains("\"allow\""),
            "allow decision must be recorded"
        );
    }

    #[tokio::test]
    async fn audit_entry_deny_contains_decision() {
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let log_path = dir.path().join("audit.log");
        let audit_config = crate::config::AuditConfig {
            enabled: true,
            destination: log_path.display().to_string(),
            ..Default::default()
        };
        let audit_logger = Arc::new(
            crate::audit::AuditLogger::from_config(&audit_config)
                .await
                .unwrap(),
        );

        let (_, llm) = MockLlm::new("DENY: test denial");
        let (_, inner) = MockInner::new();
        let gate = AdversarialPolicyGateExecutor::new(inner, make_validator(false), Arc::new(llm))
            .with_audit(Arc::clone(&audit_logger));

        let _ = gate.execute_tool_call(&make_call("shell")).await;

        let content = tokio::fs::read_to_string(&log_path).await.unwrap();
        assert!(
            content.contains("deny:"),
            "deny decision must be recorded in audit"
        );
    }

    #[tokio::test]
    async fn audit_entry_propagates_claim_source() {
        use tempfile::TempDir;

        #[derive(Debug)]
        struct InnerWithClaimSource;

        impl ToolExecutor for InnerWithClaimSource {
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
                    claim_source: Some(crate::executor::ClaimSource::Shell),
                }))
            }
        }

        let dir = TempDir::new().unwrap();
        let log_path = dir.path().join("audit.log");
        let audit_config = crate::config::AuditConfig {
            enabled: true,
            destination: log_path.display().to_string(),
            ..Default::default()
        };
        let audit_logger = Arc::new(
            crate::audit::AuditLogger::from_config(&audit_config)
                .await
                .unwrap(),
        );

        let (_, llm) = MockLlm::new("ALLOW");
        let gate = AdversarialPolicyGateExecutor::new(
            InnerWithClaimSource,
            make_validator(false),
            Arc::new(llm),
        )
        .with_audit(Arc::clone(&audit_logger));

        gate.execute_tool_call(&make_call("shell")).await.unwrap();

        let content = tokio::fs::read_to_string(&log_path).await.unwrap();
        assert!(
            content.contains("\"shell\""),
            "claim_source must be propagated into the post-execution audit entry"
        );
    }
}
