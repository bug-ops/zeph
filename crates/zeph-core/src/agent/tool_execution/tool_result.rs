// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use zeph_llm::provider::{LlmProvider, MessagePart};
use zeph_sanitizer::{ContentSource, ContentSourceKind};
use zeph_skills::evolution::FailureKind;

use super::{AnomalyOutcome, ToolResultClassification, truncate_utf8};
use crate::agent::Agent;
use crate::channel::{Channel, ToolOutputEvent};
use crate::overflow_tools::OverflowToolExecutor;

impl<C: Channel> Agent<C> {
    fn fire_vigil_audit_entry(
        &mut self,
        tool_name: &str,
        vigil_outcome: Option<&super::VigilOutcome>,
    ) {
        let (Some(vo), Some(logger)) = (
            vigil_outcome.filter(|v| !matches!(v, super::VigilOutcome::Clean)),
            self.tool_orchestrator.audit_logger.as_ref(),
        ) else {
            return;
        };
        let (vigil_risk, audit_result, err_cat) = if vo.is_blocked() {
            (
                Some(zeph_tools::VigilRiskLevel::High),
                zeph_tools::AuditResult::Blocked {
                    reason: "vigil_blocked".into(),
                },
                "vigil_blocked",
            )
        } else {
            (
                Some(zeph_tools::VigilRiskLevel::Medium),
                zeph_tools::AuditResult::Success,
                "vigil_sanitized",
            )
        };
        let entry = zeph_tools::AuditEntry {
            timestamp: zeph_tools::chrono_now(),
            tool: tool_name.to_owned().into(),
            command: String::new(),
            result: audit_result,
            duration_ms: 0,
            error_category: Some(err_cat.to_owned()),
            error_domain: Some("security".to_owned()),
            error_phase: None,
            claim_source: None,
            mcp_server_id: None,
            injection_flagged: false,
            embedding_anomalous: false,
            cross_boundary_mcp_to_acp: false,
            adversarial_policy_decision: None,
            exit_code: None,
            truncated: false,
            caller_id: None,
            policy_match: None,
            correlation_id: None,
            vigil_risk,
            execution_env: None,
            resolved_cwd: None,
            scope_at_definition: None,
            scope_at_dispatch: None,
        };
        let logger = std::sync::Arc::clone(logger);
        self.runtime.lifecycle.supervisor.spawn(
            crate::agent::agent_supervisor::TaskClass::Telemetry,
            "vigil-audit-log",
            async move { logger.log(&entry).await },
        );
    }

    fn record_tool_experience(
        &mut self,
        tool_name: &str,
        vigil_blocked: bool,
        is_error: bool,
        tool_succeeded: bool,
        tool_err_category: Option<&zeph_tools::error_taxonomy::ToolErrorCategory>,
        llm_content: &str,
    ) {
        let Some(memory) = self.services.memory.persistence.memory.as_ref() else {
            return;
        };
        let Some(experience) = memory.experience.as_ref() else {
            return;
        };
        let Some(conversation_id) = self.services.memory.persistence.conversation_id else {
            return;
        };
        let (outcome, detail, error_ctx): (&'static str, Option<String>, Option<String>) =
            if vigil_blocked {
                (
                    "blocked",
                    Some("vigil".to_owned()),
                    Some(truncate_utf8(llm_content, 256)),
                )
            } else if is_error {
                (
                    "error",
                    tool_err_category.map(|c| format!("{c:?}")),
                    Some(truncate_utf8(llm_content, 256)),
                )
            } else if tool_succeeded {
                ("success", None, None)
            } else {
                ("unknown", None, None)
            };
        let exp = std::sync::Arc::clone(experience);
        let session_id = conversation_id.0.to_string();
        let turn = i64::try_from(self.services.sidequest.turn_counter).unwrap_or(i64::MAX);
        let tool_name_owned = tool_name.to_owned();
        let accepted = self.runtime.lifecycle.supervisor.spawn(
            crate::agent::agent_supervisor::TaskClass::Telemetry,
            "experience-record",
            async move {
                if let Err(e) = exp
                    .record_tool_outcome(
                        &session_id,
                        turn,
                        &tool_name_owned,
                        outcome,
                        detail.as_deref(),
                        error_ctx.as_deref(),
                    )
                    .await
                {
                    tracing::warn!(
                        tool = %tool_name_owned, outcome = %outcome, error = %e,
                        "experience: record_tool_outcome failed",
                    );
                }
            },
        );
        if !accepted {
            tracing::warn!(
                tool = %tool_name, outcome = %outcome,
                "experience-record dropped (telemetry class at capacity)",
            );
        }
    }

    fn record_tool_execution_telemetry(
        &mut self,
        tool_name: &str,
        started_at: &std::time::Instant,
        is_error: bool,
        output: &str,
    ) {
        if let Some(ref recorder) = self.runtime.metrics.histogram_recorder {
            recorder.observe_tool_execution(started_at.elapsed());
        }
        if let Some(ref mut trace_coll) = self.runtime.debug.trace_collector
            && let Some(iter_span_id) = self.runtime.debug.current_iteration_span_id
        {
            let latency = u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
            let guard = trace_coll.begin_tool_call_at(tool_name, iter_span_id, started_at);
            let error_kind = is_error.then(|| output.chars().take(200).collect::<String>());
            trace_coll.end_tool_call(
                guard,
                tool_name,
                crate::debug_dump::trace::ToolAttributes {
                    latency_ms: latency,
                    is_error,
                    error_kind,
                },
            );
        }
    }

    fn handle_tool_failure_outcomes(
        &mut self,
        output: &str,
        tool_err_category: &mut Option<zeph_tools::error_taxonomy::ToolErrorCategory>,
        is_quality_failure: bool,
        pending_outcomes: &mut Vec<crate::agent::learning::PendingSkillOutcome>,
        pending_reflection: &mut Option<String>,
    ) -> bool {
        if output.contains("[error]") || output.contains("[exit code") {
            let kind = tool_err_category
                .take()
                .map_or_else(|| FailureKind::from_error(output), FailureKind::from);
            pending_outcomes.push(crate::agent::learning::PendingSkillOutcome {
                outcome: "tool_failure".into(),
                error_context: Some(output.to_owned()),
                outcome_detail: Some(kind.as_str().into()),
            });
            if is_quality_failure {
                self.provider
                    .record_quality_outcome(self.provider.name(), false);
            }
            if pending_reflection.is_none()
                && !self.services.learning_engine.was_reflection_used()
                && is_quality_failure
            {
                let sanitized_out = self
                    .services
                    .security
                    .sanitizer
                    .sanitize(output, ContentSource::new(ContentSourceKind::ToolResult))
                    .body;
                *pending_reflection = Some(sanitized_out);
            }
            false
        } else {
            true
        }
    }

    fn classify_tool_result(
        &mut self,
        tc: &zeph_llm::provider::ToolUseRequest,
        tool_result: Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError>,
    ) -> ToolResultClassification {
        match tool_result {
            Ok(Some(out)) => {
                let anomaly_outcome =
                    if out.summary.contains("[error]") || out.summary.contains("[stderr]") {
                        AnomalyOutcome::Error
                    } else {
                        AnomalyOutcome::Success
                    };
                if let Some(ref fs) = out.filter_stats {
                    self.record_filter_metrics(fs);
                }
                let inline_stats = out.filter_stats.as_ref().and_then(|fs| {
                    (fs.filtered_chars < fs.raw_chars).then(|| fs.format_inline(tc.name.as_str()))
                });
                let kept_lines = out
                    .filter_stats
                    .as_ref()
                    .and_then(|fs| (!fs.kept_lines.is_empty()).then(|| fs.kept_lines.clone()));
                ToolResultClassification {
                    output: out.summary,
                    is_error: false,
                    diff: out.diff,
                    inline_stats,
                    kept_lines,
                    locations: out.locations,
                    anomaly_outcome,
                    is_quality_failure: false,
                    tool_err_category: None,
                }
            }
            Ok(None) => ToolResultClassification {
                output: "(no output)".to_owned(),
                is_error: false,
                diff: None,
                inline_stats: None,
                kept_lines: None,
                locations: None,
                anomaly_outcome: AnomalyOutcome::Success,
                is_quality_failure: false,
                tool_err_category: None,
            },
            Err(ref e) => {
                let category = e.category();
                let is_quality_failure = category.is_quality_failure();
                let anomaly_outcome = if matches!(e, zeph_tools::ToolError::Blocked { .. }) {
                    AnomalyOutcome::Blocked
                } else if is_quality_failure && zeph_tools::is_reasoning_model(self.provider.name())
                {
                    AnomalyOutcome::ReasoningQualityFailure {
                        model: self.provider.name().to_owned(),
                        tool: tc.name.to_string(),
                    }
                } else {
                    AnomalyOutcome::Error
                };
                if let Some(ref d) = self.runtime.debug.debug_dumper {
                    d.dump_tool_error(tc.name.as_str(), e);
                }
                if tc.name == "memory_save"
                    && matches!(e, zeph_tools::ToolError::InvalidParams { .. })
                    && e.to_string().contains("memory write rejected")
                {
                    self.update_metrics(|m| m.memory_validation_failures += 1);
                    self.push_security_event(
                        zeph_common::SecurityEventCategory::MemoryValidation,
                        "memory_save",
                        e.to_string(),
                    );
                }
                let feedback = zeph_tools::ToolErrorFeedback {
                    category,
                    message: e.to_string(),
                    retryable: category.is_retryable(),
                };
                ToolResultClassification {
                    output: feedback.format_for_llm(),
                    is_error: true,
                    diff: None,
                    inline_stats: None,
                    kept_lines: None,
                    locations: None,
                    anomaly_outcome,
                    is_quality_failure,
                    tool_err_category: Some(category),
                }
            }
        }
    }

    #[tracing::instrument(
        name = "core.tool.process_one_result",
        skip_all,
        level = "debug",
        fields(tool_name = %tc.name),
        err
    )]
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn process_one_tool_result(
        &mut self,
        tc: &zeph_llm::provider::ToolUseRequest,
        tool_call_id: &str,
        started_at: &std::time::Instant,
        tool_result: Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError>,
        result_parts: &mut Vec<MessagePart>,
        lsp_tool_calls: &mut Vec<(String, serde_json::Value, String)>,
        has_any_injection_flags: &mut bool,
        pending_reflection: &mut Option<String>,
        pending_outcomes: &mut Vec<crate::agent::learning::PendingSkillOutcome>,
    ) -> Result<(), crate::agent::error::AgentError> {
        let ToolResultClassification {
            output,
            mut is_error,
            diff,
            inline_stats,
            kept_lines,
            locations,
            anomaly_outcome,
            is_quality_failure,
            mut tool_err_category,
        } = self.classify_tool_result(tc, tool_result);

        self.record_tool_execution_telemetry(tc.name.as_str(), started_at, is_error, &output);

        let tool_succeeded = self.handle_tool_failure_outcomes(
            &output,
            &mut tool_err_category,
            is_quality_failure,
            pending_outcomes,
            pending_reflection,
        );
        let _ = self.record_anomaly_outcome(anomaly_outcome).await;

        let processed = if tc.name == OverflowToolExecutor::TOOL_NAME {
            output.clone()
        } else {
            self.maybe_summarize_tool_output(&output).await
        };
        let body = if let Some(ref stats) = inline_stats {
            format!("{stats}\n{processed}")
        } else {
            processed.clone()
        };
        let body_display = self.maybe_redact(&body);
        self.channel
            .send_tool_output(ToolOutputEvent {
                tool_name: tc.name.clone(),
                display: body_display.into_owned(),
                diff,
                filter_stats: inline_stats,
                kept_lines,
                locations,
                tool_call_id: tool_call_id.to_owned(),
                is_error,
                terminal_id: None,
                parent_tool_use_id: self.services.session.parent_tool_use_id.clone(),
                raw_response: None,
                started_at: Some(*started_at),
            })
            .await?;

        let (processed, vigil_outcome) = self.run_vigil_gate(tc.name.as_str(), processed);
        self.fire_vigil_audit_entry(tc.name.as_str(), vigil_outcome.as_ref());

        let (llm_content, tool_had_injection_flags) = match &vigil_outcome {
            Some(super::VigilOutcome::Blocked { sentinel, .. }) => {
                is_error = true;
                (sentinel.clone(), false)
            }
            _ => {
                self.sanitize_tool_output(&processed, tc.name.as_str())
                    .await
            }
        };
        *has_any_injection_flags |= tool_had_injection_flags;

        let vigil_blocked = vigil_outcome
            .as_ref()
            .is_some_and(super::VigilOutcome::is_blocked);
        if !is_error && !vigil_blocked {
            lsp_tool_calls.push((tc.name.to_string(), tc.input.clone(), llm_content.clone()));
        }

        if vigil_blocked {
            pending_outcomes.push(crate::agent::learning::PendingSkillOutcome {
                outcome: FailureKind::SecurityBlocked.as_str().into(),
                error_context: Some("VIGIL blocked tool output".into()),
                outcome_detail: None,
            });
        } else if tool_succeeded {
            pending_outcomes.push(crate::agent::learning::PendingSkillOutcome {
                outcome: "success".into(),
                error_context: None,
                outcome_detail: None,
            });
            self.provider
                .record_quality_outcome(self.provider.name(), true);
        }

        self.record_tool_experience(
            tc.name.as_str(),
            vigil_blocked,
            is_error,
            tool_succeeded,
            tool_err_category.as_ref(),
            &llm_content,
        );

        // PASTE: record tool transition for pattern learning (#3642).
        self.observe_paste_transition(tc, started_at, tool_succeeded, vigil_blocked)
            .await;

        result_parts.push(MessagePart::ToolResult {
            tool_use_id: tc.id.clone(),
            content: llm_content,
            is_error,
        });
        Ok(())
    }

    async fn observe_paste_transition(
        &mut self,
        tc: &zeph_llm::provider::ToolUseRequest,
        started_at: &std::time::Instant,
        tool_succeeded: bool,
        vigil_blocked: bool,
    ) {
        let Some(ref store) = self.services.tool_state.pattern_store.clone() else {
            return;
        };

        let tool_name = tc.name.as_str();

        let Some((skill_name, skill_hash)) = self
            .services
            .tool_state
            .tool_to_skill
            .get(tool_name)
            .cloned()
        else {
            return;
        };

        let prev_tool = self
            .services
            .tool_state
            .last_tool_per_skill
            .get(&skill_name)
            .cloned();

        let outcome = if tool_succeeded && !vigil_blocked {
            crate::agent::speculative::paste::ToolOutcome::Success
        } else {
            crate::agent::speculative::paste::ToolOutcome::Failure
        };

        let latency_ms = u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX);

        let args_json = serde_json::to_string(&tc.input).unwrap_or_default();

        let _ = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            store.observe(
                &skill_name,
                &skill_hash,
                prev_tool.as_deref(),
                tool_name,
                &args_json,
                outcome,
                latency_ms,
            ),
        )
        .await;

        // Update last_tool_per_skill for this skill so the next tool in the same turn
        // uses the correct prev_tool value.
        self.services
            .tool_state
            .last_tool_per_skill
            .insert(skill_name, tool_name.to_owned());
    }

    #[cfg(test)]
    pub(super) async fn handle_tool_result(
        &mut self,
        response: &str,
        result: Result<Option<zeph_tools::executor::ToolOutput>, zeph_tools::executor::ToolError>,
    ) -> Result<bool, crate::agent::error::AgentError> {
        use zeph_sanitizer::{ContentSource, ContentSourceKind};
        use zeph_tools::executor::ToolError;
        match result {
            Ok(Some(output)) => self.process_successful_tool_output(output).await,
            Ok(None) => {
                self.record_skill_outcomes("success", None, None).await;
                self.record_anomaly_outcome(AnomalyOutcome::Success).await?;
                Ok(false)
            }
            Err(ToolError::Blocked { command }) => {
                tracing::warn!("blocked command: {command}");
                self.channel
                    .send("This command is blocked by security policy.")
                    .await?;
                self.record_anomaly_outcome(AnomalyOutcome::Blocked).await?;
                Ok(false)
            }
            Err(ToolError::ConfirmationRequired { command }) => {
                self.handle_confirmation_required(response, &command).await
            }
            Err(ToolError::Cancelled) => {
                tracing::info!("tool execution cancelled");
                self.update_metrics(|m| m.cancellations += 1);
                self.channel.send("[Cancelled]").await?;
                Ok(false)
            }
            Err(ToolError::SandboxViolation { path }) => {
                tracing::warn!("sandbox violation: {path}");
                self.channel
                    .send("Command targets a path outside the sandbox.")
                    .await?;
                self.record_anomaly_outcome(AnomalyOutcome::Error).await?;
                Ok(false)
            }
            Err(e) => {
                let category = e.category();
                let err_str = format!("{e:#}");
                tracing::error!("tool execution error: {err_str}");
                if let Some(ref d) = self.runtime.debug.debug_dumper {
                    d.dump_tool_error("legacy", &e);
                }
                let kind = FailureKind::from(category);
                let sanitized_err = self
                    .services
                    .security
                    .sanitizer
                    .sanitize(&err_str, ContentSource::new(ContentSourceKind::McpResponse))
                    .body;
                self.record_skill_outcomes("tool_failure", Some(&err_str), Some(kind.as_str()))
                    .await;
                self.record_anomaly_outcome(AnomalyOutcome::Error).await?;

                if !self.services.learning_engine.was_reflection_used()
                    && self.attempt_self_reflection(&sanitized_err, "").await?
                {
                    return Ok(false);
                }

                self.channel
                    .send("Tool execution failed. Please try a different approach.")
                    .await?;
                Ok(false)
            }
        }
    }

    /// Record skill learning outcomes for a tool output and optionally trigger self-reflection.
    ///
    /// Returns `Ok(true)` when the caller should return early (reflection consumed the turn),
    /// `Ok(false)` to continue, or `Err` on a hard error.
    #[cfg(test)]
    async fn record_tool_output_outcome(
        &mut self,
        output: &zeph_tools::executor::ToolOutput,
    ) -> Result<bool, crate::agent::error::AgentError> {
        if let Some(ref fs) = output.filter_stats {
            self.record_filter_metrics(fs);
        }
        if output.summary.trim().is_empty() {
            tracing::warn!("tool execution returned empty output");
            self.record_skill_outcomes("success", None, None).await;
            return Ok(true);
        }
        if output.summary.contains("[error]") || output.summary.contains("[exit code") {
            let kind = FailureKind::from_error(&output.summary);
            self.record_skill_outcomes("tool_failure", Some(&output.summary), Some(kind.as_str()))
                .await;
            if !self.services.learning_engine.was_reflection_used()
                && self
                    .attempt_self_reflection(&output.summary, &output.summary)
                    .await?
            {
                return Ok(true);
            }
        } else {
            self.record_skill_outcomes("success", None, None).await;
        }
        Ok(false)
    }

    #[cfg(test)]
    async fn process_successful_tool_output(
        &mut self,
        output: zeph_tools::executor::ToolOutput,
    ) -> Result<bool, crate::agent::error::AgentError> {
        use crate::agent::format_tool_output;
        use crate::channel::{ToolOutputEvent, ToolStartEvent};
        use zeph_llm::provider::{Message, MessagePart, Role};

        if self.record_tool_output_outcome(&output).await? {
            return Ok(false);
        }

        let tool_call_id = uuid::Uuid::new_v4().to_string();
        let tool_started_at = std::time::Instant::now();
        self.channel
            .send_tool_start(ToolStartEvent {
                tool_name: output.tool_name.clone(),
                tool_call_id: tool_call_id.clone(),
                params: None,
                parent_tool_use_id: self.services.session.parent_tool_use_id.clone(),
                started_at: std::time::Instant::now(),
                speculative: false,
                sandbox_profile: None,
            })
            .await?;
        if let Some(ref d) = self.runtime.debug.debug_dumper {
            let dump_content = if self.services.security.pii_filter.is_enabled() {
                self.services
                    .security
                    .pii_filter
                    .scrub(&output.summary)
                    .into_owned()
            } else {
                output.summary.clone()
            };
            d.dump_tool_output(output.tool_name.as_str(), &dump_content);
        }
        let processed = self.maybe_summarize_tool_output(&output.summary).await;
        let body = if let Some(ref fs) = output.filter_stats
            && fs.filtered_chars < fs.raw_chars
        {
            format!(
                "{}\n{processed}",
                fs.format_inline(output.tool_name.as_str())
            )
        } else {
            processed.clone()
        };
        let filter_stats_inline = output.filter_stats.as_ref().and_then(|fs| {
            (fs.filtered_chars < fs.raw_chars).then(|| fs.format_inline(output.tool_name.as_str()))
        });
        let formatted_output = format_tool_output(output.tool_name.as_str(), &body);
        self.channel
            .send_tool_output(ToolOutputEvent {
                tool_name: output.tool_name.clone(),
                display: self.maybe_redact(&body).to_string(),
                diff: None,
                filter_stats: filter_stats_inline,
                kept_lines: None,
                locations: output.locations,
                tool_call_id: tool_call_id.clone(),
                terminal_id: None,
                is_error: false,
                parent_tool_use_id: self.services.session.parent_tool_use_id.clone(),
                raw_response: None,
                started_at: Some(tool_started_at),
            })
            .await?;

        let (llm_body, has_injection_flags) = self
            .sanitize_tool_output(&processed, output.tool_name.as_str())
            .await;
        let user_msg = Message::from_parts(
            Role::User,
            vec![MessagePart::ToolOutput {
                tool_name: output.tool_name.clone(),
                body: llm_body,
                compacted_at: None,
            }],
        );
        self.persist_message(
            Role::User,
            &formatted_output,
            &user_msg.parts,
            has_injection_flags || !self.services.security.flagged_urls.is_empty(),
        )
        .await;
        self.push_message(user_msg);
        let outcome = if output.summary.contains("[error]") || output.summary.contains("[stderr]") {
            AnomalyOutcome::Error
        } else {
            AnomalyOutcome::Success
        };
        self.record_anomaly_outcome(outcome).await?;
        Ok(true)
    }

    #[cfg(test)]
    async fn handle_confirmation_required(
        &mut self,
        response: &str,
        command: &str,
    ) -> Result<bool, crate::agent::error::AgentError> {
        use crate::agent::format_tool_output;
        use crate::channel::{ToolOutputEvent, ToolStartEvent};
        use zeph_llm::provider::{Message, MessagePart, Role};
        let prompt = format!("Allow command: {command}?");
        if self.channel.confirm(&prompt).await? {
            if let Ok(Some(out)) = self.tool_executor.execute_confirmed_erased(response).await {
                let confirmed_tool_call_id = uuid::Uuid::new_v4().to_string();
                let confirmed_started_at = std::time::Instant::now();
                self.channel
                    .send_tool_start(ToolStartEvent {
                        tool_name: out.tool_name.clone(),
                        tool_call_id: confirmed_tool_call_id.clone(),
                        params: None,
                        parent_tool_use_id: self.services.session.parent_tool_use_id.clone(),
                        started_at: std::time::Instant::now(),
                        speculative: false,
                        sandbox_profile: None,
                    })
                    .await?;
                if let Some(ref d) = self.runtime.debug.debug_dumper {
                    let dump_content = if self.services.security.pii_filter.is_enabled() {
                        self.services
                            .security
                            .pii_filter
                            .scrub(&out.summary)
                            .into_owned()
                    } else {
                        out.summary.clone()
                    };
                    d.dump_tool_output(out.tool_name.as_str(), &dump_content);
                }
                let processed = self.maybe_summarize_tool_output(&out.summary).await;
                let formatted = format_tool_output(out.tool_name.as_str(), &processed);
                self.channel
                    .send_tool_output(ToolOutputEvent {
                        tool_name: out.tool_name.clone(),
                        display: self.maybe_redact(&processed).to_string(),
                        diff: None,
                        filter_stats: None,
                        kept_lines: None,
                        locations: out.locations,
                        tool_call_id: confirmed_tool_call_id.clone(),
                        terminal_id: None,
                        is_error: false,
                        parent_tool_use_id: self.services.session.parent_tool_use_id.clone(),
                        raw_response: None,
                        started_at: Some(confirmed_started_at),
                    })
                    .await?;
                let (llm_body, has_injection_flags) = self
                    .sanitize_tool_output(&processed, out.tool_name.as_str())
                    .await;
                let confirmed_msg = Message::from_parts(
                    Role::User,
                    vec![MessagePart::ToolOutput {
                        tool_name: out.tool_name.clone(),
                        body: llm_body,
                        compacted_at: None,
                    }],
                );
                self.persist_message(
                    Role::User,
                    &formatted,
                    &confirmed_msg.parts,
                    has_injection_flags || !self.services.security.flagged_urls.is_empty(),
                )
                .await;
                self.push_message(confirmed_msg);
            }
        } else {
            self.channel.send("Command cancelled.").await?;
        }
        Ok(false)
    }
}
