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
            skill_name: None,
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

    /// Remembers the post-execution error category for the utility gate's `Retrieve`
    /// branch, so a subsequent `Retrieve` decision can detect a just-failed retryable
    /// dependency (e.g. `memory_search` hitting a down Qdrant) instead of mandating
    /// another doomed retry (#5584).
    ///
    /// This is called for every processed result, including gate-intercepted
    /// (`Respond`/`Retrieve`/`Verify`/`Stop`) synthetic outputs, which always classify as
    /// `is_error = false`. It deliberately does NOT touch
    /// `UtilityScorer`'s `consecutive_low` counter — that counter is owned exclusively by
    /// `note_action`'s pre-dispatch cross-iteration accounting, and mixing a second
    /// reset/increment source into it silently defeated the `utility_window` hard-break
    /// (every gate-intercepted result reset the counter before it could accumulate).
    ///
    /// Skips exempt tools (`invoke_skill`, `load_skill`, ...) — the pre-dispatch scoring
    /// pass never scores them either, so they must not affect gate state.
    fn record_gate_feedback(
        &mut self,
        tool_name: &str,
        tool_err_category: Option<zeph_tools::error_taxonomy::ToolErrorCategory>,
    ) {
        if self.tool_orchestrator.utility_scorer.is_exempt(tool_name) {
            return;
        }
        self.tool_orchestrator
            .record_tool_outcome_for_gate(tool_name, tool_err_category);
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

    /// Writes the raw, pre-summarization tool output to the debug dump directory, so a blob
    /// that later gets truncated or summarized away is still recoverable (#6364). No-op when
    /// debug dumps are disabled or the result was an error (errors go through
    /// `dump_tool_error` in `classify_tool_result` instead).
    fn dump_raw_tool_output(&self, tool_name: &str, is_error: bool, output: &str) {
        if is_error {
            return;
        }
        let Some(ref d) = self.runtime.debug.debug_dumper else {
            return;
        };
        let dump_content = if self.services.security.pii_filter.is_enabled() {
            self.services.security.pii_filter.scrub(output).into_owned()
        } else {
            output.to_owned()
        };
        d.dump_tool_output(tool_name, &dump_content);
    }

    fn handle_tool_failure_outcomes(
        &mut self,
        output: &str,
        is_error: bool,
        tool_err_category: &mut Option<zeph_tools::error_taxonomy::ToolErrorCategory>,
        is_quality_failure: bool,
        pending_outcomes: &mut Vec<crate::agent::learning::PendingSkillOutcome>,
        pending_reflection: &mut Option<String>,
    ) -> bool {
        if is_error || output.contains("[error]") || output.contains("[exit code") {
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

    /// Pushes the terminal skill outcome (`SecurityBlocked` or `success`) once vigil and
    /// tool-failure classification have both been resolved, and records the quality
    /// signal for successful dispatches.
    fn record_final_skill_outcome(
        &mut self,
        vigil_blocked: bool,
        tool_succeeded: bool,
        pending_outcomes: &mut Vec<crate::agent::learning::PendingSkillOutcome>,
    ) {
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
                    media: out.media,
                    max_result_size_chars: out.max_result_size_chars,
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
                media: Vec::new(),
                max_result_size_chars: None,
            },
            Err(ref e) => {
                let category = e.category();
                let is_quality_failure = category.is_quality_failure();
                let anomaly_outcome = if matches!(e, zeph_tools::ToolError::Blocked { .. }) {
                    AnomalyOutcome::Blocked
                } else if is_quality_failure
                    && zeph_tools::is_reasoning_model(self.provider.effective_model_identifier())
                {
                    AnomalyOutcome::ReasoningQualityFailure {
                        model: self.provider.effective_model_identifier().to_owned(),
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
                    media: Vec::new(),
                    max_result_size_chars: None,
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
        images_attached_this_turn: &mut usize,
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
            media,
            max_result_size_chars,
        } = self.classify_tool_result(tc, tool_result);

        self.record_tool_execution_telemetry(tc.name.as_str(), started_at, is_error, &output);
        self.record_gate_feedback(tc.name.as_str(), tool_err_category);

        let tool_succeeded = self.handle_tool_failure_outcomes(
            &output,
            is_error,
            &mut tool_err_category,
            is_quality_failure,
            pending_outcomes,
            pending_reflection,
        );
        let _ = self.record_anomaly_outcome(anomaly_outcome).await;

        self.dump_raw_tool_output(tc.name.as_str(), is_error, &output);

        let processed = if tc.name == OverflowToolExecutor::TOOL_NAME {
            output.clone()
        } else {
            self.maybe_summarize_tool_output(&output, max_result_size_chars)
                .await
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

        self.record_final_skill_outcome(vigil_blocked, tool_succeeded, pending_outcomes);

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

        if !is_error && !vigil_blocked && !media.is_empty() {
            self.emit_media_parts(
                tc.name.as_str(),
                media,
                result_parts,
                images_attached_this_turn,
            )
            .await;
        }

        Ok(())
    }

    /// Push sibling [`MessagePart::Image`] entries for validated MCP-sourced images
    /// (spec-072 §3.2-3.3), respecting `max_images_per_turn` as a running counter shared
    /// across the whole tool-call batch. Called only from the success path — the caller
    /// already excludes error and quarantined results (FR-006/FR-007).
    ///
    /// Gates on `self.provider.supports_vision()`, mirroring the existing user-upload
    /// image gate in `build_user_message` (`agent/mod.rs`). For a router/cascade provider
    /// this is a coarse, optimistic check (aggregates `.any()` across tiers) — the
    /// concrete per-request tier-selection seam (spec-072 §3.3, C3) is responsible for the
    /// final safety net so an unresolved-vision image never reaches an incapable tier as a
    /// 400/422. Both multi-provider implementations enforce this at dispatch time:
    /// `TriageRouter::chat_with_tools` (escalate-or-strip) and `RouterProvider::chat_with_tools`
    /// (per-provider strip on every dispatch branch — Cascade/Bandit/Ema/Thompson).
    async fn emit_media_parts(
        &mut self,
        tool_name: &str,
        media: Vec<zeph_llm::ImageData>,
        result_parts: &mut Vec<MessagePart>,
        images_attached_this_turn: &mut usize,
    ) {
        if !self.provider.supports_vision() {
            tracing::warn!(
                tool_name,
                count = media.len(),
                "MCP media: provider is not vision-capable, dropping image(s) (text placeholder remains)"
            );
            return;
        }
        let server_id = tool_name
            .split_once(':')
            .map_or(tool_name, |(server, _)| server);
        let max_images_per_turn = self.runtime.config.mcp_media.max_images_per_turn;
        let mut attached = 0usize;
        for img in media {
            if *images_attached_this_turn >= max_images_per_turn {
                tracing::warn!(
                    tool_name,
                    cap = max_images_per_turn,
                    "MCP media: per-turn image budget reached, remaining image(s) dropped"
                );
                break;
            }
            result_parts.push(MessagePart::Image(Box::new(img)));
            *images_attached_this_turn += 1;
            attached += 1;
        }
        if attached > 0 {
            // Mandatory TUI status indicator (CLAUDE.md "TUI Rules") — surfaces which MCP
            // server contributed an image attached to the outgoing LLM request. Deliberately
            // NOT self-cleared: with zero work between a set and a clear on the last-write-wins
            // `StatusTx` slot, an immediate `send_status_best_effort("")` would blank the label
            // before the render loop ever has a chance to pick it up, making it invisible. Left
            // set so the next natural status update (following tool call, LLM response) replaces
            // it once real work actually happens.
            self.channel
                .send_status_best_effort(&format!(
                    "Image attached from mcp:{server_id} ({attached})"
                ))
                .await;
        }
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

    // Legacy sequential-dispatch harness, superseded in production by
    // `process_one_tool_result`/`handle_confirmation_phase` (tier_loop.rs), which processes
    // every tool result — including confirmation-resolved ones — through the same batched path.
    // Retained `#[cfg(test)]`-only to keep exercising `sanitize_tool_output`/
    // `maybe_summarize_tool_output`/`dump_tool_output` via a simpler single-call harness; not
    // dead in the sense of untested, but unreachable from any production call site (#6364).
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

    // This function's `dump_tool_output` call below is one of the two legacy sites whose
    // green `#[cfg(test)]` coverage gave false confidence that `dump_tool_output` was reachable
    // in production — it wasn't (#6364). See the retention note above `handle_tool_result`.
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
                is_mcp: false,
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
        let processed = self
            .maybe_summarize_tool_output(&output.summary, output.max_result_size_chars)
            .await;
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

    // This function's `dump_tool_output` call below is the other legacy site whose green
    // `#[cfg(test)]` coverage gave false confidence that `dump_tool_output` was reachable in
    // production — it wasn't (#6364). See the retention note above `handle_tool_result`.
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
                        is_mcp: false,
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
                let processed = self
                    .maybe_summarize_tool_output(&out.summary, out.max_result_size_chars)
                    .await;
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
