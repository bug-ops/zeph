// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! `ShadowProbeExecutor`: wraps an inner `ToolExecutor` and runs an LLM safety probe
//! before delegating high-risk tool calls.
//!
//! Wiring position (outermost first):
//!   `ScopedToolExecutor` → `ShadowProbeExecutor` → `PolicyGateExecutor` → ...
//!
//! The probe is skipped for low-risk tools, so the common path has zero latency overhead.
//! On `ProbeVerdict::Deny`, returns `ToolError::SafetyDenied` immediately without running
//! `PolicyGateExecutor` — the policy gate remains as a second defence-in-depth layer for
//! calls that pass the probe.
//!
//! # Legacy path
//!
//! `execute()` and `execute_confirmed()` bypass the probe (no structured tool id available).
//! This is intentional — the structured `execute_tool_call*` path is the active dispatch
//! path in the agent loop.

use std::sync::Arc;

use tracing::info_span;

use crate::executor::{ToolCall, ToolError, ToolExecutor, ToolOutput};
use crate::registry::ToolDef;

/// Probe interface required by `ShadowProbeExecutor`.
///
/// Decoupled from `zeph-core` to avoid a reverse crate dependency. The agent builder
/// wires in a concrete `Arc<zeph_core::agent::shadow_sentinel::ShadowSentinel>` at
/// construction time.
///
/// Uses `Pin<Box<dyn Future>>` returns for dyn-compatibility (same pattern as `ErasedToolExecutor`).
pub trait ProbeGate: Send + Sync {
    /// Evaluate whether the tool call at `qualified_tool_id` with `args` is safe.
    fn probe<'a>(
        &'a self,
        qualified_tool_id: &'a str,
        args: &'a serde_json::Value,
        turn_number: u64,
        risk_level: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ProbeOutcome> + Send + 'a>>;
}

/// Result of a probe gate evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// Tool execution may proceed.
    Allow,
    /// Tool execution is denied. The reason is returned to the caller as `ToolError::SafetyDenied`.
    Deny {
        /// Human-readable explanation from the safety probe.
        reason: String,
    },
    /// Probe was skipped (tool not high-risk, or feature disabled).
    Skip,
}

/// Wraps an inner `ToolExecutor` and applies an LLM safety probe before high-risk calls.
///
/// `ShadowProbeExecutor<T>` is `Clone` when `T: Clone` (not required for operation).
/// All methods delegate to `inner` after a probe verdict of `Allow` or `Skip`.
///
/// # Concurrency
///
/// The `probe` field is `Arc<dyn ProbeGate>`, so multiple `ShadowProbeExecutor` instances
/// sharing the same underlying `ShadowSentinel` (e.g., during parallel tool dispatch) are safe.
pub struct ShadowProbeExecutor<T: ToolExecutor> {
    inner: T,
    probe: Arc<dyn ProbeGate>,
    /// Current turn number, used for probe context and event recording.
    /// Updated by the agent loop before each turn.
    turn_number: Arc<std::sync::atomic::AtomicU64>,
    /// Current risk level string for shadow event recording.
    risk_level: Arc<parking_lot::RwLock<String>>,
}

impl<T: ToolExecutor + std::fmt::Debug> std::fmt::Debug for ShadowProbeExecutor<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShadowProbeExecutor")
            .field("inner", &self.inner)
            .finish_non_exhaustive()
    }
}

impl<T: ToolExecutor> ShadowProbeExecutor<T> {
    /// Create a new `ShadowProbeExecutor` wrapping `inner`.
    ///
    /// # Arguments
    ///
    /// * `inner` — the next executor in the chain (typically `PolicyGateExecutor`).
    /// * `probe` — the safety probe gate backed by `ShadowSentinel`.
    /// * `turn_number` — shared atomic counter updated by the agent loop.
    /// * `risk_level` — shared risk level string updated by the agent loop.
    #[must_use]
    pub fn new(
        inner: T,
        probe: Arc<dyn ProbeGate>,
        turn_number: Arc<std::sync::atomic::AtomicU64>,
        risk_level: Arc<parking_lot::RwLock<String>>,
    ) -> Self {
        Self {
            inner,
            probe,
            turn_number,
            risk_level,
        }
    }

    fn current_turn(&self) -> u64 {
        self.turn_number.load(std::sync::atomic::Ordering::Acquire)
    }

    fn current_risk_level(&self) -> String {
        self.risk_level.read().clone()
    }
}

impl<T: ToolExecutor> ToolExecutor for ShadowProbeExecutor<T> {
    /// Legacy fenced-block path: probe not applied (no structured tool id).
    async fn execute(&self, response: &str) -> Result<Option<ToolOutput>, ToolError> {
        self.inner.execute(response).await
    }

    /// Legacy confirmed path: probe not applied.
    async fn execute_confirmed(&self, response: &str) -> Result<Option<ToolOutput>, ToolError> {
        self.inner.execute_confirmed(response).await
    }

    fn tool_definitions(&self) -> Vec<ToolDef> {
        self.inner.tool_definitions()
    }

    /// Structured tool call path: probe is applied before delegation.
    ///
    /// Returns `ToolError::SafetyDenied` if the probe returns `Deny`.
    /// Delegates to `inner` on `Allow` or `Skip`.
    async fn execute_tool_call(&self, call: &ToolCall) -> Result<Option<ToolOutput>, ToolError> {
        let span = info_span!(
            "security.shadow.probe_executor",
            tool_id = %call.tool_id
        );
        let _enter = span.enter();

        let args = serde_json::Value::Object(call.params.clone());
        let turn = self.current_turn();
        let risk = self.current_risk_level();

        let outcome = self
            .probe
            .probe(call.tool_id.as_str(), &args, turn, &risk)
            .await;

        match outcome {
            ProbeOutcome::Allow | ProbeOutcome::Skip => self.inner.execute_tool_call(call).await,
            ProbeOutcome::Deny { reason } => {
                tracing::warn!(
                    tool_id = %call.tool_id,
                    reason = %reason,
                    "ShadowProbeExecutor: safety probe denied tool call"
                );
                Err(ToolError::SafetyDenied { reason })
            }
        }
    }

    /// Confirmed structured path: probe is still applied.
    ///
    /// User confirmation does not bypass the safety probe — they are orthogonal gates.
    async fn execute_tool_call_confirmed(
        &self,
        call: &ToolCall,
    ) -> Result<Option<ToolOutput>, ToolError> {
        let span = info_span!(
            "security.shadow.probe_executor_confirmed",
            tool_id = %call.tool_id
        );
        let _enter = span.enter();

        let args = serde_json::Value::Object(call.params.clone());
        let turn = self.current_turn();
        let risk = self.current_risk_level();

        let outcome = self
            .probe
            .probe(call.tool_id.as_str(), &args, turn, &risk)
            .await;

        match outcome {
            ProbeOutcome::Allow | ProbeOutcome::Skip => {
                self.inner.execute_tool_call_confirmed(call).await
            }
            ProbeOutcome::Deny { reason } => {
                tracing::warn!(
                    tool_id = %call.tool_id,
                    reason = %reason,
                    "ShadowProbeExecutor: safety probe denied confirmed tool call"
                );
                Err(ToolError::SafetyDenied { reason })
            }
        }
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

    fn is_tool_speculatable(&self, tool_id: &str) -> bool {
        // Never speculatable through the probe executor: probe adds latency and the
        // result depends on trajectory state at the time of execution.
        let _ = tool_id;
        false
    }

    fn requires_confirmation(&self, call: &ToolCall) -> bool {
        self.inner.requires_confirmation(call)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::{ToolError, ToolOutput};
    use crate::{ToolCall, ToolExecutor};
    use zeph_common::ToolName;

    struct AllowProbe;
    impl ProbeGate for AllowProbe {
        fn probe<'a>(
            &'a self,
            _: &'a str,
            _: &'a serde_json::Value,
            _: u64,
            _: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ProbeOutcome> + Send + 'a>>
        {
            Box::pin(async { ProbeOutcome::Allow })
        }
    }

    struct DenyProbe;
    impl ProbeGate for DenyProbe {
        fn probe<'a>(
            &'a self,
            _: &'a str,
            _: &'a serde_json::Value,
            _: u64,
            _: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ProbeOutcome> + Send + 'a>>
        {
            Box::pin(async {
                ProbeOutcome::Deny {
                    reason: "test denial".to_owned(),
                }
            })
        }
    }

    struct SkipProbe;
    impl ProbeGate for SkipProbe {
        fn probe<'a>(
            &'a self,
            _: &'a str,
            _: &'a serde_json::Value,
            _: u64,
            _: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ProbeOutcome> + Send + 'a>>
        {
            Box::pin(async { ProbeOutcome::Skip })
        }
    }

    struct OkInner;
    impl ToolExecutor for OkInner {
        async fn execute(&self, _: &str) -> Result<Option<ToolOutput>, ToolError> {
            Ok(None)
        }

        async fn execute_tool_call(
            &self,
            call: &ToolCall,
        ) -> Result<Option<ToolOutput>, ToolError> {
            Ok(Some(ToolOutput {
                tool_name: call.tool_id.clone(),
                summary: "ok".to_owned(),
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

    fn make_call(tool: &str) -> ToolCall {
        ToolCall {
            tool_id: ToolName::new(tool),
            params: serde_json::Map::new(),
            caller_id: None,
            context: None,
            tool_call_id: String::new(),
        }
    }

    fn make_executor<P: ProbeGate + 'static>(probe: P) -> ShadowProbeExecutor<OkInner> {
        ShadowProbeExecutor::new(
            OkInner,
            Arc::new(probe),
            Arc::new(std::sync::atomic::AtomicU64::new(1)),
            Arc::new(parking_lot::RwLock::new("calm".to_owned())),
        )
    }

    #[tokio::test]
    async fn allow_probe_delegates_to_inner() {
        let exec = make_executor(AllowProbe);
        let result = exec.execute_tool_call(&make_call("builtin:shell")).await;
        assert!(result.unwrap().is_some());
    }

    #[tokio::test]
    async fn deny_probe_returns_safety_denied() {
        let exec = make_executor(DenyProbe);
        let result = exec.execute_tool_call(&make_call("builtin:shell")).await;
        match result {
            Err(ToolError::SafetyDenied { reason }) => {
                assert_eq!(reason, "test denial");
            }
            other => panic!("expected SafetyDenied, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn skip_probe_delegates_to_inner() {
        let exec = make_executor(SkipProbe);
        let result = exec.execute_tool_call(&make_call("builtin:read")).await;
        assert!(result.unwrap().is_some());
    }

    #[tokio::test]
    async fn legacy_execute_bypasses_probe() {
        let exec = make_executor(DenyProbe);
        // Legacy path always delegates to inner, regardless of probe verdict.
        let result = exec.execute("some text").await;
        assert!(result.unwrap().is_none());
    }

    #[tokio::test]
    async fn deny_probe_blocks_confirmed_call() {
        // User confirmation must NOT bypass the safety probe.
        let exec = make_executor(DenyProbe);
        let result = exec
            .execute_tool_call_confirmed(&make_call("builtin:shell"))
            .await;
        match result {
            Err(ToolError::SafetyDenied { reason }) => {
                assert_eq!(reason, "test denial");
            }
            other => panic!("expected SafetyDenied on confirmed call, got {other:?}"),
        }
    }

    #[test]
    fn is_tool_speculatable_always_false() {
        let exec = make_executor(AllowProbe);
        assert!(!exec.is_tool_speculatable("builtin:read"));
        assert!(!exec.is_tool_speculatable("builtin:shell"));
    }
}
