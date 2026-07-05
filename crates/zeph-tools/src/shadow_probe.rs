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
//! # Quarantine short-circuit (#5740)
//!
//! Because `ShadowProbeExecutor` sits outside `TrustGateExecutor` (deep in the `PolicyGateExecutor`
//! chain), a quarantine-denied call would otherwise reach the LLM probe first, which frequently
//! denies it with a generic reason instead of `TrustGateExecutor`'s named, deterministic
//! `quarantine_denial_message`. To avoid this, `execute_tool_call`/`execute_tool_call_confirmed`
//! check the turn's effective trust and the tool id against the same `QUARANTINE_DENIED` set
//! `TrustGateExecutor` uses, and short-circuit to the identical denial message before invoking
//! the probe. The outcome is still recorded via `ProbeGate::record` (as `"quarantine
//! short-circuit: {reason}"`), so cross-session shadow-event detection (#5494/#5449) keeps
//! seeing these denials even though the LLM probe itself never ran. All other trust levels and
//! non-quarantine-denied tools still go through the LLM probe exactly as before.
//!
//! # Legacy path
//!
//! `execute()` and `execute_confirmed()` bypass the probe (no structured tool id available).
//! This is intentional — the structured `execute_tool_call*` path is the active dispatch
//! path in the agent loop.

use std::sync::Arc;

use tracing::{Instrument as _, info_span};

use crate::SkillTrustLevel;
use crate::executor::{ToolCall, ToolError, ToolExecutor, ToolOutput};
use crate::registry::ToolDef;
use crate::trust_gate::{
    is_quarantine_denied, quarantine_denial_message, trust_to_u8, u8_to_trust,
};

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

    /// Record a completed tool call in the persistent safety event stream.
    ///
    /// Called by [`ShadowProbeExecutor`] after a probe outcome of `Allow` or `Deny` (never
    /// `Skip` — recording every low-risk/disabled-feature call would flood the store with
    /// noise and defeat the purpose of cross-session pattern detection). Best-effort: no
    /// error is surfaced to the tool-dispatch path.
    ///
    /// Default implementation is a no-op, so gates that don't back a persistent store
    /// (e.g. test doubles) don't need to implement it.
    fn record<'a>(
        &'a self,
        qualified_tool_id: &'a str,
        turn_number: u64,
        risk_level: &'a str,
        context_summary: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        let _ = (qualified_tool_id, turn_number, risk_level, context_summary);
        Box::pin(async {})
    }
}

/// Result of a probe gate evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
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
    /// Effective trust level mirrored from `set_effective_trust`, used to short-circuit
    /// quarantine-denied tool calls before the LLM probe runs (#5740) — see
    /// `quarantine_denial_reason`.
    effective_trust: std::sync::atomic::AtomicU8,
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
            effective_trust: std::sync::atomic::AtomicU8::new(trust_to_u8(
                SkillTrustLevel::Trusted,
            )),
        }
    }

    fn current_turn(&self) -> u64 {
        self.turn_number.load(std::sync::atomic::Ordering::Acquire)
    }

    fn current_risk_level(&self) -> String {
        self.risk_level.read().clone()
    }

    fn effective_trust(&self) -> SkillTrustLevel {
        u8_to_trust(
            self.effective_trust
                .load(std::sync::atomic::Ordering::Relaxed),
        )
    }

    /// Returns the quarantine denial reason (deterministic, no LLM call) for `call` if the
    /// turn's effective trust is Quarantined and `call.tool_id` is in the quarantine-denied set.
    ///
    /// Mirrors `TrustGateExecutor::check_trust`'s Quarantined branch so the caller never
    /// reaches this executor's LLM safety probe for a call that `TrustGateExecutor` would
    /// deny anyway — the probe would otherwise frequently deny it first with a generic,
    /// unnamed reason, hiding the informative `quarantine_denial_message` (#5740).
    ///
    /// Returns only the reason string (not a `ToolError`) because the caller must still
    /// `record()` the outcome in the shadow event stream before returning the error — the
    /// same as every other denial path in this executor.
    fn quarantine_denial_reason(&self, call: &ToolCall) -> Option<String> {
        if self.effective_trust() == SkillTrustLevel::Quarantined
            && is_quarantine_denied(call.tool_id.as_str())
        {
            let active_skills = call.skill_name.as_deref().unwrap_or(&[]);
            return Some(quarantine_denial_message(
                call.tool_id.as_str(),
                active_skills,
            ));
        }
        None
    }

    /// Summarise a tool execution result for the shadow event stream's `context_summary`.
    fn context_summary_for_result(result: &Result<Option<ToolOutput>, ToolError>) -> String {
        match result {
            Ok(Some(output)) => output.summary.clone(),
            Ok(None) => "tool call completed with no output".to_owned(),
            Err(e) => format!("tool call failed: {e}"),
        }
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
        let turn = self.current_turn();
        let risk = self.current_risk_level();

        if let Some(reason) = self.quarantine_denial_reason(call) {
            tracing::warn!(
                tool_id = %call.tool_id,
                reason = %reason,
                "ShadowProbeExecutor: quarantine short-circuit denied tool call"
            );
            self.probe
                .record(
                    call.tool_id.as_str(),
                    turn,
                    &risk,
                    &format!("quarantine short-circuit: {reason}"),
                )
                .await;
            return Err(ToolError::SafetyDenied { reason });
        }

        let span = info_span!(
            "security.shadow.probe_executor",
            tool_id = %call.tool_id
        );

        let args = serde_json::Value::Object(call.params.clone());

        let outcome = self
            .probe
            .probe(call.tool_id.as_str(), &args, turn, &risk)
            .instrument(span)
            .await;

        match outcome {
            ProbeOutcome::Allow => {
                let result = self.inner.execute_tool_call(call).await;
                // `ConfirmationRequired` is not a terminal outcome — the same call will run
                // again via `execute_tool_call_confirmed` once the user approves, which records
                // its own (correct) event. Recording here too would double-record every
                // confirmation-gated call with a spurious "tool call failed" entry.
                if !matches!(result, Err(ToolError::ConfirmationRequired { .. })) {
                    let summary = Self::context_summary_for_result(&result);
                    self.probe
                        .record(call.tool_id.as_str(), turn, &risk, &summary)
                        .await;
                }
                result
            }
            ProbeOutcome::Skip => self.inner.execute_tool_call(call).await,
            ProbeOutcome::Deny { reason } => {
                tracing::warn!(
                    tool_id = %call.tool_id,
                    reason = %reason,
                    "ShadowProbeExecutor: safety probe denied tool call"
                );
                self.probe
                    .record(
                        call.tool_id.as_str(),
                        turn,
                        &risk,
                        &format!("probe denied: {reason}"),
                    )
                    .await;
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
        let turn = self.current_turn();
        let risk = self.current_risk_level();

        if let Some(reason) = self.quarantine_denial_reason(call) {
            tracing::warn!(
                tool_id = %call.tool_id,
                reason = %reason,
                "ShadowProbeExecutor: quarantine short-circuit denied confirmed tool call"
            );
            self.probe
                .record(
                    call.tool_id.as_str(),
                    turn,
                    &risk,
                    &format!("quarantine short-circuit: {reason}"),
                )
                .await;
            return Err(ToolError::SafetyDenied { reason });
        }

        let span = info_span!(
            "security.shadow.probe_executor_confirmed",
            tool_id = %call.tool_id
        );

        let args = serde_json::Value::Object(call.params.clone());

        let outcome = self
            .probe
            .probe(call.tool_id.as_str(), &args, turn, &risk)
            .instrument(span)
            .await;

        match outcome {
            ProbeOutcome::Allow => {
                let result = self.inner.execute_tool_call_confirmed(call).await;
                // Defense-in-depth/symmetry with `execute_tool_call`: `TrustGateExecutor`
                // itself never reissues `ConfirmationRequired` on the confirmed path, but a
                // future inner layer could, and the same double-recording rationale applies.
                if !matches!(result, Err(ToolError::ConfirmationRequired { .. })) {
                    let summary = Self::context_summary_for_result(&result);
                    self.probe
                        .record(call.tool_id.as_str(), turn, &risk, &summary)
                        .await;
                }
                result
            }
            ProbeOutcome::Skip => self.inner.execute_tool_call_confirmed(call).await,
            ProbeOutcome::Deny { reason } => {
                tracing::warn!(
                    tool_id = %call.tool_id,
                    reason = %reason,
                    "ShadowProbeExecutor: safety probe denied confirmed tool call"
                );
                self.probe
                    .record(
                        call.tool_id.as_str(),
                        turn,
                        &risk,
                        &format!("probe denied: {reason}"),
                    )
                    .await;
                Err(ToolError::SafetyDenied { reason })
            }
        }
    }

    fn set_skill_env(&self, env: Option<std::collections::HashMap<String, String>>) {
        self.inner.set_skill_env(env);
    }

    fn set_effective_trust(&self, level: crate::SkillTrustLevel) {
        self.effective_trust
            .store(trust_to_u8(level), std::sync::atomic::Ordering::Relaxed);
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

    /// Test double whose `probe()` panics if invoked. Used to prove the quarantine
    /// short-circuit never reaches the LLM probe, rather than merely returning the right
    /// message (which `DenyProbe` alone cannot distinguish from "probe ran and happened to
    /// deny with a different reason").
    struct PanicProbe;
    impl ProbeGate for PanicProbe {
        fn probe<'a>(
            &'a self,
            _: &'a str,
            _: &'a serde_json::Value,
            _: u64,
            _: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ProbeOutcome> + Send + 'a>>
        {
            panic!("probe() must not be invoked when the quarantine short-circuit applies")
        }
    }

    /// Test double that returns a fixed `probe()` outcome and captures every `record()` call,
    /// so tests can assert whether recording happened without a real `ShadowSentinel`.
    struct RecordingProbe {
        outcome: ProbeOutcome,
        recorded: std::sync::Mutex<Vec<(String, u64, String, String)>>,
    }

    impl RecordingProbe {
        fn new(outcome: ProbeOutcome) -> Self {
            Self {
                outcome,
                recorded: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    impl ProbeGate for RecordingProbe {
        fn probe<'a>(
            &'a self,
            _: &'a str,
            _: &'a serde_json::Value,
            _: u64,
            _: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ProbeOutcome> + Send + 'a>>
        {
            let outcome = self.outcome.clone();
            Box::pin(async move { outcome })
        }

        fn record<'a>(
            &'a self,
            qualified_tool_id: &'a str,
            turn_number: u64,
            risk_level: &'a str,
            context_summary: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
            Box::pin(async move {
                self.recorded.lock().unwrap().push((
                    qualified_tool_id.to_owned(),
                    turn_number,
                    risk_level.to_owned(),
                    context_summary.to_owned(),
                ));
            })
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

    /// Inner executor that always returns `ConfirmationRequired`, simulating
    /// `TrustGateExecutor::execute_tool_call` for a `PermissionAction::Ask`-gated tool.
    struct ConfirmationRequiredInner;
    impl ToolExecutor for ConfirmationRequiredInner {
        async fn execute(&self, _: &str) -> Result<Option<ToolOutput>, ToolError> {
            Ok(None)
        }

        async fn execute_tool_call(
            &self,
            call: &ToolCall,
        ) -> Result<Option<ToolOutput>, ToolError> {
            Err(ToolError::ConfirmationRequired {
                command: call.tool_id.to_string(),
            })
        }
    }

    fn make_call(tool: &str) -> ToolCall {
        ToolCall {
            tool_id: ToolName::new(tool),
            params: serde_json::Map::new(),
            caller_id: None,
            context: None,
            tool_call_id: String::new(),
            skill_name: None,
        }
    }

    fn make_call_with_skills(tool: &str, skills: &[&str]) -> ToolCall {
        ToolCall {
            tool_id: ToolName::new(tool),
            params: serde_json::Map::new(),
            caller_id: None,
            context: None,
            tool_call_id: String::new(),
            skill_name: Some(skills.iter().map(ToString::to_string).collect()),
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

    // ── quarantine short-circuit (#5740) ──────────────────────────────────────

    /// Regression for #5740: when the turn's effective trust is Quarantined and the tool is
    /// in `QUARANTINE_DENIED`, the deterministic message must win over the probe's own denial
    /// reason — proving the probe was never invoked (its reason would otherwise leak through).
    #[tokio::test]
    async fn quarantined_short_circuits_before_probe_runs() {
        // PanicProbe proves the LLM probe is never invoked — a DenyProbe could only prove
        // the returned message differs, not that probe() was skipped entirely.
        let exec = make_executor(PanicProbe);
        exec.set_effective_trust(SkillTrustLevel::Quarantined);

        let call = make_call_with_skills("bash", &["disk-usage"]);
        let result = exec.execute_tool_call(&call).await;
        match result {
            Err(ToolError::SafetyDenied { reason }) => {
                assert!(
                    reason.contains("disk-usage"),
                    "expected quarantine_denial_message naming active skills, got: {reason}"
                );
            }
            other => panic!("expected SafetyDenied, got {other:?}"),
        }
    }

    /// Same short-circuit must apply on the confirmed path — user confirmation does not
    /// bypass the quarantine trust floor any more than it bypasses the probe.
    #[tokio::test]
    async fn quarantined_short_circuits_confirmed_path() {
        let exec = make_executor(PanicProbe);
        exec.set_effective_trust(SkillTrustLevel::Quarantined);

        let call = make_call_with_skills("bash", &["disk-usage"]);
        let result = exec.execute_tool_call_confirmed(&call).await;
        match result {
            Err(ToolError::SafetyDenied { reason }) => {
                assert!(reason.contains("disk-usage"));
            }
            other => panic!("expected SafetyDenied on confirmed call, got {other:?}"),
        }
    }

    /// A tool outside `QUARANTINE_DENIED` (e.g. a read) must still go through the probe
    /// even when the turn is Quarantined — the short-circuit is scoped to denied tools only.
    #[tokio::test]
    async fn quarantined_non_denied_tool_still_runs_probe() {
        let exec = make_executor(AllowProbe);
        exec.set_effective_trust(SkillTrustLevel::Quarantined);

        let result = exec.execute_tool_call(&make_call("read")).await;
        assert!(result.unwrap().is_some());
    }

    /// When trust is not Quarantined, a `QUARANTINE_DENIED`-listed tool (e.g. "bash") must
    /// still go through the probe as before — the short-circuit must not fire at other trust
    /// levels.
    #[tokio::test]
    async fn non_quarantined_trust_still_runs_probe_for_denied_tool_name() {
        let exec = make_executor(DenyProbe);
        exec.set_effective_trust(SkillTrustLevel::Trusted);

        let result = exec.execute_tool_call(&make_call("bash")).await;
        match result {
            Err(ToolError::SafetyDenied { reason }) => {
                assert_eq!(
                    reason, "test denial",
                    "probe must still run at Trusted level"
                );
            }
            other => panic!("expected SafetyDenied from probe, got {other:?}"),
        }
    }

    /// Confirmed-path counterpart of `quarantined_non_denied_tool_still_runs_probe`.
    #[tokio::test]
    async fn quarantined_non_denied_tool_still_runs_probe_confirmed_path() {
        let exec = make_executor(AllowProbe);
        exec.set_effective_trust(SkillTrustLevel::Quarantined);

        let result = exec.execute_tool_call_confirmed(&make_call("read")).await;
        assert!(result.unwrap().is_some());
    }

    /// Confirmed-path counterpart of `non_quarantined_trust_still_runs_probe_for_denied_tool_name`.
    #[tokio::test]
    async fn non_quarantined_trust_still_runs_probe_for_denied_tool_name_confirmed_path() {
        let exec = make_executor(DenyProbe);
        exec.set_effective_trust(SkillTrustLevel::Trusted);

        let result = exec.execute_tool_call_confirmed(&make_call("bash")).await;
        match result {
            Err(ToolError::SafetyDenied { reason }) => {
                assert_eq!(
                    reason, "test denial",
                    "probe must still run at Trusted level"
                );
            }
            other => panic!("expected SafetyDenied from probe, got {other:?}"),
        }
    }

    /// Regression for the S1 review finding on #5740: the quarantine short-circuit must still
    /// record a shadow event, otherwise cross-session detection (#5494/#5449) silently loses
    /// visibility into every quarantine denial that used to flow through `ProbeOutcome::Deny`.
    #[tokio::test]
    async fn quarantine_short_circuit_still_records_event() {
        let probe = Arc::new(RecordingProbe::new(ProbeOutcome::Allow));
        let gate: Arc<dyn ProbeGate> = probe.clone();
        let exec = ShadowProbeExecutor::new(
            OkInner,
            gate,
            Arc::new(std::sync::atomic::AtomicU64::new(7)),
            Arc::new(parking_lot::RwLock::new("elevated".to_owned())),
        );
        exec.set_effective_trust(SkillTrustLevel::Quarantined);

        let call = make_call_with_skills("bash", &["disk-usage"]);
        let result = exec.execute_tool_call(&call).await;
        assert!(matches!(result, Err(ToolError::SafetyDenied { .. })));

        let recorded = probe.recorded.lock().unwrap();
        assert_eq!(
            recorded.len(),
            1,
            "quarantine short-circuit must record exactly one event"
        );
        let (tool_id, turn, risk, summary) = &recorded[0];
        assert_eq!(tool_id, "bash");
        assert_eq!(*turn, 7);
        assert_eq!(risk, "elevated");
        assert!(summary.starts_with("quarantine short-circuit:"));
        assert!(summary.contains("disk-usage"));
    }

    /// Same recording contract on the confirmed path.
    #[tokio::test]
    async fn quarantine_short_circuit_confirmed_path_still_records_event() {
        let probe = Arc::new(RecordingProbe::new(ProbeOutcome::Allow));
        let gate: Arc<dyn ProbeGate> = probe.clone();
        let exec = ShadowProbeExecutor::new(
            OkInner,
            gate,
            Arc::new(std::sync::atomic::AtomicU64::new(1)),
            Arc::new(parking_lot::RwLock::new("calm".to_owned())),
        );
        exec.set_effective_trust(SkillTrustLevel::Quarantined);

        let call = make_call_with_skills("bash", &["disk-usage"]);
        let result = exec.execute_tool_call_confirmed(&call).await;
        assert!(matches!(result, Err(ToolError::SafetyDenied { .. })));
        assert_eq!(probe.recorded.lock().unwrap().len(), 1);
    }

    #[test]
    fn is_tool_speculatable_always_false() {
        let exec = make_executor(AllowProbe);
        assert!(!exec.is_tool_speculatable("builtin:read"));
        assert!(!exec.is_tool_speculatable("builtin:shell"));
    }

    // ── record() wiring (#5449 follow-up) ─────────────────────────────────────

    #[tokio::test]
    async fn allow_outcome_records_after_execution() {
        let probe = Arc::new(RecordingProbe::new(ProbeOutcome::Allow));
        let gate: Arc<dyn ProbeGate> = probe.clone();
        let exec = ShadowProbeExecutor::new(
            OkInner,
            gate,
            Arc::new(std::sync::atomic::AtomicU64::new(3)),
            Arc::new(parking_lot::RwLock::new("elevated".to_owned())),
        );

        let result = exec.execute_tool_call(&make_call("builtin:shell")).await;
        assert!(result.unwrap().is_some());

        let recorded = probe.recorded.lock().unwrap();
        assert_eq!(
            recorded.len(),
            1,
            "Allow outcome must record exactly one event"
        );
        let (tool_id, turn, risk, summary) = &recorded[0];
        assert_eq!(tool_id, "builtin:shell");
        assert_eq!(*turn, 3);
        assert_eq!(risk, "elevated");
        assert_eq!(summary, "ok");
    }

    /// Regression: `ConfirmationRequired` is not terminal — the confirmed re-run records the
    /// real outcome, so recording here too would double-record every confirmation-gated call
    /// with a spurious "tool call failed" entry (found in code review of the initial fix).
    #[tokio::test]
    async fn allow_outcome_does_not_record_on_confirmation_required() {
        let probe = Arc::new(RecordingProbe::new(ProbeOutcome::Allow));
        let gate: Arc<dyn ProbeGate> = probe.clone();
        let exec = ShadowProbeExecutor::new(
            ConfirmationRequiredInner,
            gate,
            Arc::new(std::sync::atomic::AtomicU64::new(1)),
            Arc::new(parking_lot::RwLock::new("calm".to_owned())),
        );

        let result = exec.execute_tool_call(&make_call("builtin:shell")).await;
        assert!(matches!(
            result,
            Err(ToolError::ConfirmationRequired { .. })
        ));
        assert!(
            probe.recorded.lock().unwrap().is_empty(),
            "ConfirmationRequired must not be recorded — the confirmed re-run records instead"
        );
    }

    #[tokio::test]
    async fn deny_outcome_records_denial_reason() {
        let probe = Arc::new(RecordingProbe::new(ProbeOutcome::Deny {
            reason: "risky pattern".to_owned(),
        }));
        let gate: Arc<dyn ProbeGate> = probe.clone();
        let exec = ShadowProbeExecutor::new(
            OkInner,
            gate,
            Arc::new(std::sync::atomic::AtomicU64::new(1)),
            Arc::new(parking_lot::RwLock::new("calm".to_owned())),
        );

        let result = exec.execute_tool_call(&make_call("builtin:shell")).await;
        assert!(result.is_err(), "Deny outcome must still return an error");

        let recorded = probe.recorded.lock().unwrap();
        assert_eq!(
            recorded.len(),
            1,
            "Deny outcome must be recorded even though the tool never executed"
        );
        assert!(recorded[0].3.contains("risky pattern"));
    }

    #[tokio::test]
    async fn skip_outcome_does_not_record() {
        let probe = Arc::new(RecordingProbe::new(ProbeOutcome::Skip));
        let gate: Arc<dyn ProbeGate> = probe.clone();
        let exec = ShadowProbeExecutor::new(
            OkInner,
            gate,
            Arc::new(std::sync::atomic::AtomicU64::new(1)),
            Arc::new(parking_lot::RwLock::new("calm".to_owned())),
        );

        let _ = exec.execute_tool_call(&make_call("builtin:read")).await;
        assert!(
            probe.recorded.lock().unwrap().is_empty(),
            "Skip outcome must never record — it covers both disabled-feature and \
             low-risk-tool cases and would flood the store with noise"
        );
    }

    #[tokio::test]
    async fn allow_outcome_records_on_confirmed_path_too() {
        let probe = Arc::new(RecordingProbe::new(ProbeOutcome::Allow));
        let gate: Arc<dyn ProbeGate> = probe.clone();
        let exec = ShadowProbeExecutor::new(
            OkInner,
            gate,
            Arc::new(std::sync::atomic::AtomicU64::new(1)),
            Arc::new(parking_lot::RwLock::new("calm".to_owned())),
        );

        let _ = exec
            .execute_tool_call_confirmed(&make_call("builtin:shell"))
            .await;
        assert_eq!(
            probe.recorded.lock().unwrap().len(),
            1,
            "confirmed path must also record on Allow"
        );
    }
}
