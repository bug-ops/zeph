// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Pre-dispatch policy gates shared by the tier loop and the inline orchestration loop.
//!
//! [`Agent::apply_policy_gates`] runs the batch-level gates (exfiltration URL flagging,
//! pre-execution verifiers, channel tool allowlist) in one fixed order, and [`GateRefusal`]
//! owns the text and reason of every refusal so both loops answer a refused call identically.

#[cfg(feature = "scheduler")]
use std::collections::HashSet;

use zeph_tools::executor::ToolCall;

use crate::agent::Agent;
#[cfg(feature = "scheduler")]
use crate::agent::tool_orchestrator::RepeatWindow;
use crate::channel::Channel;

/// Why a gate refused a tool call before dispatch.
///
/// Each variant owns the model-facing text and the denial reason, so the tier loop and the
/// inline loop cannot drift apart. [`GateRefusal::into_output`] builds the synthetic result,
/// which carries `ToolStatus::Synthetic(GateRefused)`: it is a failure that skips dependants
/// and never enters the result cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::agent) enum GateRefusal {
    /// A prerequisite tool failed or requires confirmation.
    FailedDependency,
    /// The session tool-call quota is exhausted.
    QuotaExceeded { max: u32 },
    /// A pre-execution verifier or the channel allowlist blocked the call.
    PreExecutionBlocked,
    /// The dispatch gate rejected the call.
    DispatchRejected { reason: String },
    /// The same call repeated within the repeat window.
    Repeated,
    /// The tool is outside the orchestration task's own allowlist.
    #[cfg(feature = "scheduler")]
    NotInTaskAllowlist,
}

impl GateRefusal {
    fn message(&self, tool: &str) -> String {
        match self {
            Self::FailedDependency => {
                "[error] Skipped: a prerequisite tool failed or requires confirmation".to_owned()
            }
            Self::QuotaExceeded { max } => format!(
                "[error] Tool call quota exceeded (session limit: {max} calls). \
                 No further tool calls are allowed this session."
            ),
            Self::PreExecutionBlocked => format!(
                "[error] Tool call to {tool} was blocked by pre-execution verifier. \
                 The requested operation is not permitted."
            ),
            Self::DispatchRejected { reason } => {
                format!("[error] Tool call to {tool} was rejected: {reason}.")
            }
            Self::Repeated => format!(
                "[error] Repeated identical call to {tool} detected. \
                 Use different arguments or a different approach."
            ),
            #[cfg(feature = "scheduler")]
            Self::NotInTaskAllowlist => format!(
                "[error] Tool call to {tool} was rejected: the tool is not in this task's \
                 tool allowlist."
            ),
        }
    }

    /// Short reason handed to `permission_denied` hooks and logs.
    pub(in crate::agent) fn reason(&self, tool: &str) -> String {
        match self {
            Self::FailedDependency => {
                "prerequisite tool failed or requires confirmation".to_owned()
            }
            Self::QuotaExceeded { max } => {
                format!("session tool call quota exceeded (limit: {max} calls)")
            }
            Self::PreExecutionBlocked => {
                format!("blocked by pre-execution verifier: {tool} is not permitted")
            }
            Self::DispatchRejected { reason } => reason.clone(),
            Self::Repeated => format!("repeated identical call to {tool} detected"),
            #[cfg(feature = "scheduler")]
            Self::NotInTaskAllowlist => format!("{tool} is not in the task tool allowlist"),
        }
    }

    /// The synthetic result answering the refused call.
    pub(in crate::agent) fn into_output(
        self,
        tool: zeph_common::ToolName,
    ) -> zeph_tools::ToolOutput {
        let summary = self.message(tool.as_str());
        zeph_tools::ToolOutput::synthetic(tool, summary, zeph_tools::SyntheticKind::GateRefused)
    }
}

/// Records pre-dispatch verifier blocks into the agent's metrics, security events and audit log.
///
/// The one recorder behind both the parent loop's gates and the
/// [`PreDispatchBlockSink`](zeph_tools::PreDispatchBlockSink) handed to sub-agents, so a block
/// is recorded identically wherever it is raised. It owns clones of the handles it needs and
/// does not borrow the agent, which lets a long-lived sub-agent loop hold it.
#[derive(Clone)]
pub(crate) struct PreDispatchBlockRecorder {
    metrics_tx: Option<tokio::sync::watch::Sender<crate::metrics::MetricsSnapshot>>,
    start_time: std::time::Instant,
    audit_logger: Option<std::sync::Arc<zeph_tools::AuditLogger>>,
}

impl PreDispatchBlockRecorder {
    /// Count the block and push a `PreExecutionBlock` security event.
    fn note(&self, block: &zeph_tools::PreDispatchBlock) {
        let Some(tx) = &self.metrics_tx else {
            return;
        };
        let event = crate::metrics::SecurityEvent::new(
            zeph_common::SecurityEventCategory::PreExecutionBlock,
            block.tool.as_str(),
            block.detail(),
        );
        let elapsed = self.start_time.elapsed().as_secs();
        tx.send_modify(|m| {
            m.uptime_seconds = elapsed;
            m.pre_execution_blocks += 1;
            m.push_security_event(event);
        });
    }

    /// The logger and audit row for `block`, or `None` when auditing is disabled.
    fn audit_entry(
        &self,
        block: &zeph_tools::PreDispatchBlock,
    ) -> Option<(
        std::sync::Arc<zeph_tools::AuditLogger>,
        zeph_tools::AuditEntry,
    )> {
        self.audit_logger.as_ref().map(|logger| {
            (
                std::sync::Arc::clone(logger),
                zeph_tools::AuditEntry::pre_execution_block(block),
            )
        })
    }
}

impl zeph_tools::PreDispatchBlockSink for PreDispatchBlockRecorder {
    fn record(
        &self,
        block: zeph_tools::PreDispatchBlock,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            self.note(&block);
            if let Some((logger, entry)) = self.audit_entry(&block) {
                logger.log(&entry).await;
            }
        })
    }
}

impl<C: Channel> Agent<C> {
    #[cfg(feature = "scheduler")]
    /// Whether `tool` may be offered to and called by an inline orchestration task: it must pass
    /// the channel allowlist and, when the planner emitted one, the task's own allowlist.
    pub(in crate::agent) fn inline_tool_permitted(
        &self,
        tool: &str,
        task_allowlist: Option<&HashSet<String>>,
    ) -> bool {
        let channel_ok = self
            .runtime
            .config
            .channel_tool_allowlist
            .as_ref()
            .is_none_or(|allowed| allowed.iter().any(|t| t == tool));
        channel_ok
            && task_allowlist
                .is_none_or(|allowed| allowed.contains(&zeph_subagent::normalize_tool_id(tool)))
    }

    #[cfg(feature = "scheduler")]
    /// Gate one batch of inline-loop tool calls and return the refusal, if any, per call.
    ///
    /// Applies, in order of precedence: the task allowlist, the session quota (charged once per
    /// batch), the batch policy gates, and the loop-local `repeat` window. Dependency, dispatch
    /// and utility gates belong to the tier loop and do not apply to inline tasks.
    pub(in crate::agent) fn inline_gate_refusals(
        &mut self,
        tool_calls: &[zeph_llm::provider::ToolUseRequest],
        calls: &[ToolCall],
        task_allowlist: Option<&HashSet<String>>,
        repeat: &mut RepeatWindow,
    ) -> Vec<Option<GateRefusal>> {
        let policy_blocked = self.apply_policy_gates(tool_calls, calls);
        let quota_blocked = self.check_and_update_quota(calls.len());
        let max = self
            .tool_orchestrator
            .max_tool_calls_per_session
            .unwrap_or(0);
        let hashes: Vec<u64> = calls
            .iter()
            .map(|c| super::tool_args_hash(&c.params))
            .collect();
        let entries: Vec<(&str, u64)> = calls
            .iter()
            .zip(&hashes)
            .map(|(c, &h)| (c.tool_id.as_str(), h))
            .collect();
        let exempt: Vec<bool> = calls
            .iter()
            .map(|c| self.is_gate_exempt(c.tool_id.as_str()))
            .collect();
        let repeated = repeat.check_and_push(&entries, &exempt);

        calls
            .iter()
            .enumerate()
            .map(|(idx, call)| {
                if task_allowlist.is_some_and(|allowed| {
                    !allowed.contains(&zeph_subagent::normalize_tool_id(call.tool_id.as_str()))
                }) {
                    Some(GateRefusal::NotInTaskAllowlist)
                } else if quota_blocked {
                    Some(GateRefusal::QuotaExceeded { max })
                } else if policy_blocked[idx] {
                    Some(GateRefusal::PreExecutionBlocked)
                } else if repeated[idx] {
                    Some(GateRefusal::Repeated)
                } else {
                    None
                }
            })
            .collect()
    }

    /// A recorder bound to this agent's metrics channel, start time and audit logger.
    pub(in crate::agent) fn pre_dispatch_block_recorder(&self) -> PreDispatchBlockRecorder {
        PreDispatchBlockRecorder {
            metrics_tx: self.runtime.metrics.metrics_tx.clone(),
            start_time: self.runtime.lifecycle.start_time,
            audit_logger: self.tool_orchestrator.audit_logger.clone(),
        }
    }

    /// Run the batch-level policy gates in order — exfiltration URL flagging (flag-only),
    /// pre-execution verifiers, channel tool allowlist — and return one `blocked` flag per call.
    #[tracing::instrument(name = "core.tool.policy_gates", skip_all, level = "debug")]
    pub(in crate::agent) fn apply_policy_gates(
        &mut self,
        tool_calls: &[zeph_llm::provider::ToolUseRequest],
        calls: &[ToolCall],
    ) -> Vec<bool> {
        self.check_exfiltration_urls(tool_calls);
        let mut blocked = self.run_pre_execution_verifiers(calls);
        self.apply_channel_tool_allowlist(calls, &mut blocked);
        blocked
    }

    fn run_pre_execution_verifiers(&mut self, calls: &[ToolCall]) -> Vec<bool> {
        let mut pre_exec_blocked = vec![false; calls.len()];
        if self.tool_orchestrator.pre_execution_verifiers.is_empty() {
            return pre_exec_blocked;
        }
        let recorder = self.pre_dispatch_block_recorder();
        for (idx, call) in calls.iter().enumerate() {
            if self.is_gate_exempt(call.tool_id.as_str()) {
                continue;
            }
            let args_value = serde_json::Value::Object(call.params.clone());
            for verifier in self.tool_orchestrator.pre_execution_verifiers.iter() {
                match verifier.verify(call.tool_id.as_str(), &args_value) {
                    zeph_tools::VerificationResult::Block { reason } => {
                        tracing::warn!(
                            tool = %call.tool_id,
                            verifier = verifier.name(),
                            %reason,
                            "pre-execution verifier blocked tool call"
                        );
                        let block = zeph_tools::PreDispatchBlock {
                            tool: call.tool_id.clone(),
                            verifier: verifier.name(),
                            reason,
                            args_json: serde_json::to_string(&args_value).unwrap_or_default(),
                            caller_id: call.caller_id.clone(),
                            skill_name: call.skill_name.clone(),
                        };
                        recorder.note(&block);
                        if let Some((logger, entry)) = recorder.audit_entry(&block) {
                            self.runtime.lifecycle.supervisor.spawn(
                                crate::agent::agent_supervisor::TaskClass::Telemetry,
                                "audit-log",
                                async move { logger.log(&entry).await },
                            );
                        }
                        pre_exec_blocked[idx] = true;
                        break;
                    }
                    zeph_tools::VerificationResult::Warn { message } => {
                        tracing::warn!(
                            tool = %call.tool_id,
                            verifier = verifier.name(),
                            %message,
                            "pre-execution verifier warning (not blocked)"
                        );
                        self.update_metrics(|m| m.pre_execution_warnings += 1);
                        self.push_security_event(
                            zeph_common::SecurityEventCategory::PreExecutionWarn,
                            call.tool_id.as_str(),
                            format!("{}: {}", verifier.name(), message),
                        );
                    }
                    _ => {}
                }
            }
        }
        pre_exec_blocked
    }

    /// Block tool calls whose names are absent from the channel-level allowlist (#3879).
    ///
    /// No-op when no allowlist is configured (`None`). Skips already-blocked indices.
    /// Comparison is case-sensitive; channel configs must use canonical lowercase names.
    fn apply_channel_tool_allowlist(&mut self, calls: &[ToolCall], pre_exec_blocked: &mut [bool]) {
        let Some(ref allowlist) = self.runtime.config.channel_tool_allowlist else {
            return;
        };
        for (idx, call) in calls.iter().enumerate() {
            if pre_exec_blocked[idx] || self.is_gate_exempt(call.tool_id.as_str()) {
                continue;
            }
            if !allowlist.iter().any(|t| t == call.tool_id.as_str()) {
                tracing::warn!(tool = %call.tool_id, "tool blocked by channel allowlist");
                self.update_metrics(|m| m.pre_execution_blocks += 1);
                self.push_security_event(
                    zeph_common::SecurityEventCategory::PreExecutionBlock,
                    call.tool_id.as_str(),
                    format!(
                        "channel allowlist: '{}' is not permitted on this channel",
                        call.tool_id
                    ),
                );
                pre_exec_blocked[idx] = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeph_tools::{SyntheticKind, ToolStatus};

    #[test]
    fn refusal_output_is_synthetic_gate_refused_failure() {
        let refusals = [
            GateRefusal::FailedDependency,
            GateRefusal::QuotaExceeded { max: 3 },
            GateRefusal::PreExecutionBlocked,
            GateRefusal::DispatchRejected {
                reason: "disabled".to_owned(),
            },
            GateRefusal::Repeated,
            #[cfg(feature = "scheduler")]
            GateRefusal::NotInTaskAllowlist,
        ];
        for refusal in refusals {
            let out = refusal
                .clone()
                .into_output(zeph_common::ToolName::new("bash"));
            assert_eq!(
                out.status,
                ToolStatus::Synthetic(SyntheticKind::GateRefused),
                "{refusal:?}"
            );
            assert!(out.status.is_failure() && out.status.aborts_dependants());
            assert!(out.summary.starts_with("[error]"), "{}", out.summary);
            assert!(out.claim_source.is_none());
        }
    }

    #[test]
    fn refusal_reason_names_the_tool_where_relevant() {
        assert!(GateRefusal::Repeated.reason("bash").contains("bash"));
        assert_eq!(
            GateRefusal::QuotaExceeded { max: 5 }.reason("bash"),
            "session tool call quota exceeded (limit: 5 calls)"
        );
    }
}
