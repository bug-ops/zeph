// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! MAGE trajectory-risk confirmation and escalation gates.
//!
//! Covers the human-in-the-loop confirmation phase (`ConfirmationRequired` tool errors) and
//! the MAGE trajectory risk hard-block/soft-escalation gates (spec 004-19 FR-004–FR-006).
//! Split out of `tier_loop.rs` — see that module for the orchestration entry point that calls
//! into these gates.

use zeph_tools::executor::ToolCall;

use crate::agent::Agent;
use crate::channel::Channel;

impl<C: Channel> Agent<C> {
    /// Single batch-level human confirmation for the MAGE soft-escalation tier (spec 004-19
    /// FR-006).
    ///
    /// Returns `Ok(true)` if the user declined — the tombstone and `[Cancelled]` notice are
    /// already persisted via `cancel_tool_batch`, matching every other cancellation checkpoint
    /// in this file; the caller must return `Ok(false)` without running the tier loop. Returns
    /// `Ok(false)` if the user approved — the caller must then run the *normal*
    /// `run_tier_execution_loop` so `check_trust`/`PermissionPolicy`/shadow-probe still apply
    /// per call. MAGE escalation gates *whether* execution proceeds at all; it must never
    /// substitute for those per-call gates — an earlier version of this wiring synthesized
    /// `ToolError::ConfirmationRequired` and dispatched approved calls through
    /// `execute_tool_call_confirmed_erased`, which explicitly skips `check_trust`, letting a
    /// policy-`Deny` tool execute under escalation (critic finding F1).
    pub(super) async fn confirm_mage_escalation(
        &mut self,
        tool_calls: &[zeph_llm::provider::ToolUseRequest],
    ) -> Result<bool, crate::agent::error::AgentError> {
        let score = self.services.security.mage_accumulator.current_risk();
        let prompt = format!(
            "Elevated trajectory risk detected (score {score:.3}) — allow tool execution to proceed?"
        );
        if self.channel.confirm(&prompt).await? {
            return Ok(false);
        }
        self.cancel_tool_batch(
            tool_calls,
            "tool execution cancelled: MAGE trajectory risk escalation declined",
        )
        .await?;
        Ok(true)
    }

    #[tracing::instrument(
        name = "core.tool.handle_confirmation_phase",
        skip_all,
        level = "debug",
        err
    )]
    /// Returns `Ok(true)` if the user cancelled the turn during this phase.
    pub(super) async fn handle_confirmation_phase(
        &mut self,
        tool_calls: &[zeph_llm::provider::ToolUseRequest],
        calls: &[ToolCall],
        tool_results: &mut [Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError>],
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<bool, crate::agent::error::AgentError> {
        for idx in 0..tool_results.len() {
            if cancel.is_cancelled() {
                self.cancel_tool_batch(tool_calls, "tool execution cancelled by user")
                    .await?;
                return Ok(true);
            }
            let new_result =
                if let Err(zeph_tools::ToolError::ConfirmationRequired { ref command }) =
                    tool_results[idx]
                {
                    let tc = &tool_calls[idx];
                    let prompt = if command.is_empty() {
                        format!("Allow tool: {}?", tc.name)
                    } else {
                        format!("Allow command: {command}?")
                    };
                    Some(if self.channel.confirm(&prompt).await? {
                        // execute_tool_call_confirmed_erased bypasses check_trust; a second
                        // ConfirmationRequired here indicates a misconfigured executor stack.
                        self.tool_executor
                            .execute_tool_call_confirmed_erased(&calls[idx])
                            .await
                    } else {
                        Ok(Some(zeph_tools::ToolOutput {
                            tool_name: tc.name.clone(),
                            summary: "[cancelled by user]".to_owned(),
                            blocks_executed: 0,
                            filter_stats: None,
                            diff: None,
                            streamed: false,
                            terminal_id: None,
                            locations: None,
                            raw_response: None,
                            claim_source: None,
                            ..Default::default()
                        }))
                    })
                } else {
                    None
                };
            if let Some(result) = new_result {
                if let Err(ref e) = result
                    && let Some(ref d) = self.runtime.debug.debug_dumper
                {
                    d.dump_tool_error(tool_calls[idx].name.as_str(), e);
                }
                tool_results[idx] = result;
            }
        }
        Ok(false)
    }

    /// Check MAGE trajectory risk gate (spec 004-19 FR-004, FR-005).
    ///
    /// Returns `Some((score, top_signals))` when the accumulator is blocked. Emits a security
    /// event, increments `pre_execution_blocks`, and calls `record_block()` on the accumulator.
    pub(super) fn check_mage_block(&mut self) -> Option<(f64, Vec<String>)> {
        if !self.services.security.mage_accumulator.is_blocked() {
            return None;
        }
        let score = self.services.security.mage_accumulator.current_risk();
        let top: Vec<String> = self
            .services
            .security
            .mage_accumulator
            .top_signals(3)
            .iter()
            .map(|s| format!("{:?}({:?})", s.signal_type, s.severity))
            .collect();
        tracing::warn!(
            score,
            signals = ?top,
            "MAGE trajectory risk accumulator blocked tool dispatch"
        );
        self.update_metrics(|m| m.pre_execution_blocks += 1);
        self.push_security_event(
            zeph_common::SecurityEventCategory::PreExecutionBlock,
            "<mage>",
            format!("trajectory risk {score:.3} exceeds threshold"),
        );
        self.services.security.mage_accumulator.record_block();
        Some((score, top))
    }

    /// Check MAGE trajectory risk soft-escalation gate (spec 004-19 FR-006).
    ///
    /// Returns `true` when the accumulator's risk is in `[escalation_threshold,
    /// risk_threshold)`. Emits a security event, increments `pre_execution_warnings`, and
    /// calls `record_escalation()` on the accumulator so the caller gates the batch behind
    /// a single `Agent::confirm_mage_escalation` confirmation before falling through to the
    /// normal tier execution loop (see that method's doc comment for why this must not
    /// bypass `check_trust`/`PermissionPolicy`).
    pub(super) fn check_mage_escalation(&mut self) -> bool {
        if !self.services.security.mage_accumulator.should_escalate() {
            return false;
        }
        let score = self.services.security.mage_accumulator.current_risk();
        tracing::warn!(
            score,
            "MAGE trajectory risk accumulator escalating tool dispatch to human confirmation"
        );
        self.update_metrics(|m| m.pre_execution_warnings += 1);
        self.push_security_event(
            zeph_common::SecurityEventCategory::PreExecutionWarn,
            "<mage>",
            format!("trajectory risk {score:.3} in escalation band, requiring confirmation"),
        );
        self.services.security.mage_accumulator.record_escalation();
        true
    }
}
