// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use futures::FutureExt as _;
use tracing::Instrument;
use zeph_llm::provider::{Message, MessagePart, Role};
use zeph_tools::ExecutionContext;
use zeph_tools::executor::ToolCall;

use zeph_llm::provider::ToolDefinition;

use super::{
    CacheCheckResult, TierLoopData, TierLoopOutput, ToolDispatchContext, ToolExecFut,
    retry_backoff_ms, strip_tafc_fields, tool_args_hash,
};
use crate::agent::Agent;
use crate::channel::{Channel, StopHint, ToolStartEvent};

/// Maximum byte length of `ZEPH_TOOL_ARGS_JSON`. OS `ARG_MAX` is ~1 MB on macOS and ~2 MB on
/// Linux; staying well below that avoids `E2BIG` when spawning hook processes.
const TOOL_ARGS_JSON_LIMIT: usize = 64 * 1024;

/// Build the base env map for `pre_tool_use` / `post_tool_use` hook dispatch.
///
/// `ZEPH_TOOL_ARGS_JSON` is truncated to [`TOOL_ARGS_JSON_LIMIT`] bytes when the serialized
/// argument object would exceed the OS `ARG_MAX` limit.
fn make_tool_hook_env(
    tool_name: &str,
    tool_input: &serde_json::Value,
    session_id: Option<&str>,
) -> std::collections::HashMap<String, String> {
    let mut env = std::collections::HashMap::new();
    env.insert("ZEPH_TOOL_NAME".to_owned(), tool_name.to_owned());

    let raw = serde_json::to_string(tool_input).unwrap_or_default();
    let args_json = if raw.len() > TOOL_ARGS_JSON_LIMIT {
        tracing::warn!(
            tool = tool_name,
            len = raw.len(),
            limit = TOOL_ARGS_JSON_LIMIT,
            "ZEPH_TOOL_ARGS_JSON truncated for hook dispatch"
        );
        let limit = raw.floor_char_boundary(TOOL_ARGS_JSON_LIMIT);
        format!("{}…", &raw[..limit])
    } else {
        raw
    };
    env.insert("ZEPH_TOOL_ARGS_JSON".to_owned(), args_json);

    if let Some(sid) = session_id {
        env.insert("ZEPH_SESSION_ID".to_owned(), sid.to_owned());
    }
    env
}

impl<C: Channel> Agent<C> {
    #[tracing::instrument(
        name = "core.tool.run_post_dispatch_phases",
        skip_all,
        level = "debug",
        err
    )]
    async fn run_post_dispatch_phases(
        &mut self,
        tool_calls: &[zeph_llm::provider::ToolUseRequest],
        calls: &[ToolCall],
        tool_results: &mut [Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError>],
        max_retries: usize,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<(), crate::agent::error::AgentError> {
        self.handle_confirmation_phase(tool_calls, calls, tool_results, cancel)
            .await?;
        self.handle_retry_phase(tool_calls, calls, tool_results, max_retries, cancel)
            .await?;
        self.handle_reformat_phase(tool_calls, tool_results, cancel)
            .await?;
        Ok(())
    }

    #[tracing::instrument(
        name = "core.tool.handle_confirmation_phase",
        skip_all,
        level = "debug",
        err
    )]
    async fn handle_confirmation_phase(
        &mut self,
        tool_calls: &[zeph_llm::provider::ToolUseRequest],
        calls: &[ToolCall],
        tool_results: &mut [Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError>],
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<(), crate::agent::error::AgentError> {
        for idx in 0..tool_results.len() {
            if cancel.is_cancelled() {
                self.tool_executor.set_skill_env(None);
                tracing::info!("tool execution cancelled by user");
                self.update_metrics(|m| m.cancellations += 1);
                self.channel.send("[Cancelled]").await?;
                self.persist_cancelled_tool_results(tool_calls).await;
                return Ok(());
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
        Ok(())
    }

    #[tracing::instrument(name = "core.tool.handle_retry_phase", skip_all, level = "debug", err)]
    async fn handle_retry_phase(
        &mut self,
        tool_calls: &[zeph_llm::provider::ToolUseRequest],
        calls: &[ToolCall],
        tool_results: &mut [Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError>],
        max_retries: usize,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<(), crate::agent::error::AgentError> {
        if max_retries == 0 {
            return Ok(());
        }
        let max_retry_duration_secs = self.tool_orchestrator.max_retry_duration_secs;
        let retry_base_ms = self.tool_orchestrator.retry_base_ms;
        let retry_max_ms = self.tool_orchestrator.retry_max_ms;
        for idx in 0..tool_results.len() {
            if cancel.is_cancelled() {
                self.tool_executor.set_skill_env(None);
                tracing::info!("tool execution cancelled by user");
                self.update_metrics(|m| m.cancellations += 1);
                self.channel.send("[Cancelled]").await?;
                self.persist_cancelled_tool_results(tool_calls).await;
                return Ok(());
            }
            let is_transient = matches!(
                tool_results[idx],
                Err(ref e) if e.kind() == zeph_tools::ErrorKind::Transient
            );
            if !is_transient {
                continue;
            }
            let tc = &tool_calls[idx];
            if !self
                .tool_executor
                .is_tool_retryable_erased(tc.name.as_str())
            {
                continue;
            }
            let call = &calls[idx];
            let mut attempt = 0_usize;
            let retry_start = std::time::Instant::now();
            let result = loop {
                let exec_result = tokio::select! {
                    r = self.tool_executor.execute_tool_call_erased(call).instrument(
                        tracing::info_span!("tool_exec_retry", tool_name = %tc.name, idx = %tc.id)
                    ) => r,
                    () = cancel.cancelled() => {
                        self.tool_executor.set_skill_env(None);
                        tracing::info!("tool retry cancelled by user");
                        self.update_metrics(|m| m.cancellations += 1);
                        self.channel.send("[Cancelled]").await?;
                        self.persist_cancelled_tool_results(tool_calls).await;
                        return Ok(());
                    }
                };
                match exec_result {
                    Err(ref e)
                        if e.kind() == zeph_tools::ErrorKind::Transient
                            && attempt < max_retries =>
                    {
                        let elapsed_secs = retry_start.elapsed().as_secs();
                        if max_retry_duration_secs > 0 && elapsed_secs >= max_retry_duration_secs {
                            tracing::warn!(
                                tool = %tc.name, elapsed_secs, max_retry_duration_secs,
                                "tool retry budget exceeded, aborting retries"
                            );
                            break exec_result;
                        }
                        attempt += 1;
                        let delay_ms = retry_backoff_ms(attempt - 1, retry_base_ms, retry_max_ms);
                        tracing::warn!(
                            tool = %tc.name, attempt, delay_ms, error = %e,
                            "transient tool error, retrying with backoff"
                        );
                        let _ = self
                            .channel
                            .send_status(&format!("Retrying {}...", tc.name))
                            .await;
                        // Interruptible backoff sleep: cancelled if agent shuts down.
                        tokio::select! {
                            () = tokio::time::sleep(std::time::Duration::from_millis(delay_ms)) => {}
                            () = cancel.cancelled() => {
                                self.tool_executor.set_skill_env(None);
                                tracing::info!("retry backoff interrupted by cancellation");
                                self.update_metrics(|m| m.cancellations += 1);
                                self.channel.send("[Cancelled]").await?;
                                return Ok(());
                            }
                        }
                        let _ = self.channel.send_status("").await;
                        // NOTE: retry re-executions are NOT recorded in repeat-detection (CRIT-3).
                    }
                    result => break result,
                }
            };
            tool_results[idx] = result;
        }
        Ok(())
    }

    #[tracing::instrument(
        name = "core.tool.handle_reformat_phase",
        skip_all,
        level = "debug",
        err
    )]
    async fn handle_reformat_phase(
        &mut self,
        tool_calls: &[zeph_llm::provider::ToolUseRequest],
        tool_results: &mut [Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError>],
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<(), crate::agent::error::AgentError> {
        if self
            .tool_orchestrator
            .parameter_reformat_provider
            .is_empty()
        {
            return Ok(());
        }
        let budget_secs = self.tool_orchestrator.max_retry_duration_secs;
        for idx in 0..tool_results.len() {
            if cancel.is_cancelled() {
                self.tool_executor.set_skill_env(None);
                tracing::info!("parameter reformat phase cancelled by user");
                self.update_metrics(|m| m.cancellations += 1);
                self.channel.send("[Cancelled]").await?;
                self.persist_cancelled_tool_results(tool_calls).await;
                return Ok(());
            }
            let needs_reformat = matches!(
                tool_results[idx],
                Err(ref e) if e.category().needs_parameter_reformat()
            );
            if !needs_reformat {
                continue;
            }
            let tc = &tool_calls[idx];
            tracing::warn!(
                tool = %tc.name,
                "parameter error detected; parameter reformat path is reserved for future \
                 LLM-based reformat implementation"
            );
            // Budget check: a newly created instant always has ~0 elapsed, so this guard
            // is effectively a no-op today. Kept for structural parity with the planned
            // LLM-based reformat implementation that will run actual work here.
            let reformat_start = std::time::Instant::now();
            if budget_secs > 0 && reformat_start.elapsed().as_secs() >= budget_secs {
                tracing::warn!(tool = %tc.name, "parameter reformat budget exhausted, skipping");
                continue;
            }
            let _ = self
                .channel
                .send_status(&format!(
                    "Reformat for {} pending provider integration…",
                    tc.name
                ))
                .await;
            let _ = self.channel.send_status("").await;
        }
        Ok(())
    }

    fn run_pre_execution_verifiers(&mut self, calls: &[ToolCall]) -> Vec<bool> {
        let mut pre_exec_blocked = vec![false; calls.len()];
        if self.tool_orchestrator.pre_execution_verifiers.is_empty() {
            return pre_exec_blocked;
        }
        for (idx, call) in calls.iter().enumerate() {
            let args_value = serde_json::Value::Object(call.params.clone());
            for verifier in &self.tool_orchestrator.pre_execution_verifiers {
                match verifier.verify(call.tool_id.as_str(), &args_value) {
                    zeph_tools::VerificationResult::Allow => {}
                    zeph_tools::VerificationResult::Block { reason } => {
                        tracing::warn!(
                            tool = %call.tool_id,
                            verifier = verifier.name(),
                            %reason,
                            "pre-execution verifier blocked tool call"
                        );
                        self.update_metrics(|m| m.pre_execution_blocks += 1);
                        self.push_security_event(
                            zeph_common::SecurityEventCategory::PreExecutionBlock,
                            call.tool_id.as_str(),
                            format!("{}: {}", verifier.name(), reason),
                        );
                        if let Some(ref logger) = self.tool_orchestrator.audit_logger {
                            let args_json = serde_json::to_string(&args_value).unwrap_or_default();
                            let entry = zeph_tools::AuditEntry {
                                timestamp: zeph_tools::chrono_now(),
                                tool: call.tool_id.clone(),
                                command: args_json,
                                result: zeph_tools::AuditResult::Blocked {
                                    reason: format!("{}: {}", verifier.name(), reason),
                                },
                                duration_ms: 0,
                                error_category: Some("pre_execution_block".to_owned()),
                                error_domain: Some("security".to_owned()),
                                error_phase: Some(
                                    zeph_tools::error_taxonomy::ToolInvocationPhase::Setup
                                        .label()
                                        .to_owned(),
                                ),
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
                            let logger = std::sync::Arc::clone(logger);
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
                }
            }
        }
        pre_exec_blocked
    }

    fn compute_utility_actions(
        &mut self,
        calls: &[ToolCall],
        pre_exec_blocked: &[bool],
    ) -> Vec<zeph_tools::UtilityAction> {
        #[allow(clippy::cast_possible_truncation)]
        let tokens_consumed =
            usize::try_from(self.runtime.providers.cached_prompt_tokens).unwrap_or(usize::MAX);
        // token_budget = 0 signals "unknown" to UtilityContext — cost component is zeroed.
        let token_budget: usize = 0;
        let tool_calls_this_turn = self.tool_orchestrator.recent_tool_calls.len();
        // Detect explicit tool request from the last user message text only.
        // We only read MessagePart::Text parts so tool outputs/thinking blocks are excluded.
        let explicit_request = self
            .msg
            .messages
            .iter()
            .rfind(|m| m.role == zeph_llm::provider::Role::User)
            .is_some_and(|m| {
                let text = if m.parts.is_empty() {
                    m.content.clone()
                } else {
                    m.parts
                        .iter()
                        .filter_map(|p| {
                            if let zeph_llm::provider::MessagePart::Text { text } = p {
                                Some(text.as_str())
                            } else {
                                None
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(" ")
                };
                zeph_tools::has_explicit_tool_request(&text)
            });
        calls
            .iter()
            .enumerate()
            .map(|(idx, call)| {
                if pre_exec_blocked[idx] {
                    return zeph_tools::UtilityAction::ToolCall;
                }
                if self
                    .tool_orchestrator
                    .utility_scorer
                    .is_exempt(call.tool_id.as_str())
                {
                    return zeph_tools::UtilityAction::ToolCall;
                }
                let ctx = zeph_tools::UtilityContext {
                    tool_calls_this_turn: tool_calls_this_turn + idx,
                    tokens_consumed,
                    token_budget,
                    user_requested: explicit_request,
                };
                let score = self.tool_orchestrator.utility_scorer.score(call, &ctx);
                let action = self
                    .tool_orchestrator
                    .utility_scorer
                    .recommend_action(score.as_ref(), &ctx);
                tracing::debug!(
                    tool = %call.tool_id,
                    score = ?score.as_ref().map(|s| s.total),
                    threshold = self.tool_orchestrator.utility_scorer.threshold(),
                    action = ?action,
                    "utility gate: action recommended"
                );
                if action != zeph_tools::UtilityAction::ToolCall {
                    tracing::info!(
                        tool = %call.tool_id,
                        action = ?action,
                        "utility gate: non-execute action"
                    );
                }
                // Record call regardless so subsequent calls in this batch see it as prior.
                self.tool_orchestrator.utility_scorer.record_call(call);
                action
            })
            .collect()
    }

    #[tracing::instrument(
        name = "core.tool.handle_native_tool_calls",
        skip_all,
        level = "debug",
        fields(tool_count = tool_calls.len()),
        err
    )]
    pub(super) async fn handle_native_tool_calls(
        &mut self,
        text: Option<&str>,
        tool_calls: &[zeph_llm::provider::ToolUseRequest],
    ) -> Result<(), crate::agent::error::AgentError> {
        let t_tool_exec = std::time::Instant::now();
        tracing::debug!("turn timing: tool_exec start");
        // Scan for image-exfiltration in accompanying text, send to channel, persist
        // the assistant ToolUse message.
        self.push_assistant_tool_use_message(text, tool_calls)
            .await?;

        // Build calls, assign IDs, run exfiltration guard, gate checks (pre-exec/utility/
        // quota/repeat/cache), and inject skill env. Extracted to keep this function under
        // the clippy line limit.
        let ToolDispatchContext {
            calls,
            tool_call_ids,
            mut tool_started_ats,
            pre_exec_blocked,
            utility_actions,
            quota_blocked,
            args_hashes,
            repeat_blocked,
            cache_hits,
        } = self.prepare_tool_dispatch(tool_calls);

        let max_retries = self.tool_orchestrator.max_tool_retries;
        // Clamp to 1 to prevent Semaphore(0) deadlock when config is set to 0.
        let max_parallel = self.runtime.config.timeouts.max_parallel_tools.max(1);
        let cancel = self.runtime.lifecycle.cancel_token.clone();

        // Causal IPI pre-probe: record behavioral baseline before tool batch dispatch.
        let causal_pre_response = self.run_causal_pre_probe().await;

        // Phase 1: Tiered parallel execution bounded by a shared semaphore.
        // Extracted to run_tier_execution_loop to satisfy the line-count limit.
        // Returns None when the user cancelled (caller must return Ok(())).
        let tier_data = self
            .run_tier_execution_loop(
                tool_calls,
                &calls,
                &pre_exec_blocked,
                &utility_actions,
                quota_blocked,
                &args_hashes,
                &repeat_blocked,
                &cache_hits,
                max_parallel,
                &cancel,
                &tool_call_ids,
                &mut tool_started_ats,
            )
            .await?;

        // Unpack tier execution output. None means the user cancelled — return early.
        let Some(TierLoopData {
            mut tool_results,
            pending_focus_checkpoint,
            pending_system_hints,
        }) = tier_data
        else {
            return Ok(());
        };

        // Phases 2a / 2 / 3: confirmation, transient retry, parameter reformat.
        // Each phase may return early on cancellation (Ok(())) or propagate channel errors.
        self.run_post_dispatch_phases(tool_calls, &calls, &mut tool_results, max_retries, &cancel)
            .await?;

        // Process results, persist messages, run LSP hooks, fire deferred reflection.
        // Also clears skill env and syncs cache counters after execution.
        // Extracted to process_tool_result_batch to satisfy the line-count limit.
        self.process_tool_result_batch(
            tool_calls,
            &tool_call_ids,
            &tool_started_ats,
            tool_results,
            causal_pre_response,
            pending_focus_checkpoint,
            pending_system_hints,
        )
        .await?;

        let tool_exec_ms = u64::try_from(t_tool_exec.elapsed().as_millis()).unwrap_or(u64::MAX);
        tracing::debug!(ms = tool_exec_ms, "turn timing: tool_exec done");
        self.runtime.metrics.pending_timings.tool_exec_ms = self
            .runtime
            .metrics
            .pending_timings
            .tool_exec_ms
            .saturating_add(tool_exec_ms);

        Ok(())
    }

    async fn push_assistant_tool_use_message(
        &mut self,
        text: Option<&str>,
        tool_calls: &[zeph_llm::provider::ToolUseRequest],
    ) -> Result<(), crate::agent::error::AgentError> {
        // S4: scan text accompanying ToolUse responses for markdown image exfiltration.
        let cleaned_text: Option<String> = if let Some(t) = text
            && !t.is_empty()
        {
            Some(self.scan_output_and_warn(t))
        } else {
            None
        };

        if let Some(ref t) = cleaned_text
            && !t.is_empty()
        {
            let display = self.maybe_redact(t);
            self.channel.send(&display).await?;
        }

        let mut parts: Vec<MessagePart> = Vec::new();
        if let Some(ref t) = cleaned_text
            && !t.is_empty()
        {
            parts.push(MessagePart::Text { text: t.clone() });
        }
        for tc in tool_calls {
            parts.push(MessagePart::ToolUse {
                id: tc.id.clone(),
                name: tc.name.to_string(),
                input: tc.input.clone(),
            });
        }
        let assistant_msg = Message::from_parts(Role::Assistant, parts);
        self.persist_message(
            Role::Assistant,
            &assistant_msg.content,
            &assistant_msg.parts,
            false,
        )
        .await;
        self.push_message(assistant_msg);
        if let (Some(id), Some(last)) = (
            self.msg.last_persisted_message_id,
            self.msg.messages.last_mut(),
        ) {
            last.metadata.db_id = Some(id);
        }
        Ok(())
    }

    fn prepare_tool_dispatch(
        &mut self,
        tool_calls: &[zeph_llm::provider::ToolUseRequest],
    ) -> ToolDispatchContext {
        let tafc_enabled = self.tool_orchestrator.tafc.enabled;
        // When the orchestration scheduler has set a named execution environment for the
        // current task, inject it into every ToolCall so ShellExecutor::resolve_context
        // uses the right env/cwd without the LLM having to supply it.
        let task_ctx: Option<ExecutionContext> = self
            .services
            .orchestration
            .task_execution_env
            .as_deref()
            .map(|name| ExecutionContext::default().with_name(name));
        // Assign stable IDs before execution so ToolStart, ToolOutputChunk, and ToolOutput
        // all share the same ID, enabling correct per-tool routing in parallel execution.
        let tool_call_ids: Vec<String> = tool_calls
            .iter()
            .map(|_| uuid::Uuid::new_v4().to_string())
            .collect();

        let calls: Vec<ToolCall> = tool_calls
            .iter()
            .enumerate()
            .filter_map(|(idx, tc)| {
                let mut params: serde_json::Map<String, serde_json::Value> =
                    if let serde_json::Value::Object(map) = &tc.input {
                        map.clone()
                    } else {
                        serde_json::Map::new()
                    };
                if tafc_enabled && strip_tafc_fields(&mut params, tc.name.as_str()).is_err() {
                    // Model produced only think fields — skip this tool call.
                    return None;
                }
                Some(ToolCall {
                    tool_id: tc.name.clone(),
                    params,
                    caller_id: None,
                    context: task_ctx.clone(),
                    tool_call_id: tool_call_ids[idx].clone(),
                })
            })
            .collect();
        // tool_started_ats is populated per-tier just before each tier's join_all so that
        // audit timestamps reflect actual execution start rather than pre-build time.
        let tool_started_ats: Vec<std::time::Instant> =
            vec![std::time::Instant::now(); tool_calls.len()];

        self.check_exfiltration_urls(tool_calls);

        // Pre-execution verification (TrustBench pattern, issue #1630).
        // Runs after exfiltration guard (flag-only) and before repeat-detection.
        // Block: return synthetic error result for this call without executing.
        // Warn: log + emit security event + continue execution.
        let pre_exec_blocked = self.run_pre_execution_verifiers(&calls);

        // Utility gate: score each call and recommend an action (#2477).
        // user_requested is detected from the last user message only — never from LLM content or
        // tool outputs to prevent prompt-injection bypass (C2 fix).
        let utility_actions = self.compute_utility_actions(&calls, &pre_exec_blocked);

        // Per-session quota check. Counted once per logical dispatch batch (M3: retries do not
        // consume additional quota slots). When exceeded, all calls in this batch are quota-blocked.
        let quota_blocked = if let Some(max) = self.tool_orchestrator.check_quota() {
            tracing::warn!(
                max,
                count = self.tool_orchestrator.session_tool_call_count,
                "tool call quota exceeded for session"
            );
            true
        } else {
            // Increment before the retry loop — each call in this batch counts as one logical call.
            self.tool_orchestrator.session_tool_call_count = self
                .tool_orchestrator
                .session_tool_call_count
                .saturating_add(u32::try_from(calls.len()).unwrap_or(u32::MAX));
            false
        };

        // Build args hashes and check for repeats. Blocked calls get a pre-built error result.
        let args_hashes: Vec<u64> = calls.iter().map(|c| tool_args_hash(&c.params)).collect();
        let repeat_blocked: Vec<bool> = calls
            .iter()
            .zip(args_hashes.iter())
            .map(|(call, &hash)| {
                let blocked = self
                    .tool_orchestrator
                    .is_repeat(call.tool_id.as_str(), hash);
                if blocked {
                    tracing::warn!(
                        tool = %call.tool_id,
                        "[repeat-detect] identical tool call detected, skipping execution"
                    );
                }
                blocked
            })
            .collect();
        // Repeat-detection (CRIT-3): push LLM-initiated calls BEFORE execution.
        // Cache hits are also pushed here (P1 invariant): a cached tool called N times must
        // still trigger repeat-detection to prevent infinite loops if the LLM keeps requesting it.
        for (call, &hash) in calls.iter().zip(args_hashes.iter()) {
            self.tool_orchestrator
                .push_tool_call(call.tool_id.as_str(), hash);
        }

        // Cache lookup: for each non-repeat, cacheable call, check result cache before dispatch.
        // Hits are stored as pre-built results; cache store happens after join_all completes.
        let cache_hits: Vec<Option<zeph_tools::ToolOutput>> = calls
            .iter()
            .zip(args_hashes.iter())
            .zip(repeat_blocked.iter())
            .map(|((call, &hash), &blocked)| {
                if blocked || !zeph_tools::is_cacheable(call.tool_id.as_str()) {
                    return None;
                }
                let key = zeph_tools::CacheKey::new(call.tool_id.as_str(), hash);
                self.tool_orchestrator.result_cache.get(&key)
            })
            .collect();

        // Inject active skill secrets before tool execution.
        self.inject_active_skill_env();

        ToolDispatchContext {
            calls,
            tool_call_ids,
            tool_started_ats,
            pre_exec_blocked,
            utility_actions,
            quota_blocked,
            args_hashes,
            repeat_blocked,
            cache_hits,
        }
    }

    fn check_exfiltration_urls(&mut self, tool_calls: &[zeph_llm::provider::ToolUseRequest]) {
        for tc in tool_calls {
            let args_json = tc.input.to_string();
            let url_events = self
                .services
                .security
                .exfiltration_guard
                .validate_tool_call(
                    tc.name.as_str(),
                    &args_json,
                    &self.services.security.flagged_urls,
                );
            if !url_events.is_empty() {
                tracing::warn!(
                    tool = %tc.name,
                    count = url_events.len(),
                    "exfiltration guard: suspicious URLs in tool arguments (flag-only, not blocked)"
                );
                self.update_metrics(|m| {
                    m.exfiltration_tool_urls_flagged += url_events.len() as u64;
                });
                self.push_security_event(
                    zeph_common::SecurityEventCategory::ExfiltrationBlock,
                    tc.name.as_str(),
                    format!(
                        "{} suspicious URL(s) flagged in tool args",
                        url_events.len()
                    ),
                );
            }
        }
    }

    #[tracing::instrument(name = "core.tool.run_causal_pre_probe", skip_all, level = "debug")]
    async fn run_causal_pre_probe(&mut self) -> Option<(String, String)> {
        let analyzer = self.services.security.causal_analyzer.as_ref()?;
        let context_summary = self.build_causal_context_summary();
        match analyzer.probe(&context_summary).await {
            Ok(resp) => Some((resp, context_summary)),
            Err(e) => {
                tracing::warn!(error = %e, "causal IPI pre-probe failed, skipping analysis");
                None
            }
        }
    }

    #[tracing::instrument(
        name = "core.tool.run_tier_execution_loop",
        skip_all,
        level = "debug",
        fields(tool_count = tool_calls.len()),
        err
    )]
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    async fn run_tier_execution_loop(
        &mut self,
        tool_calls: &[zeph_llm::provider::ToolUseRequest],
        calls: &[ToolCall],
        pre_exec_blocked: &[bool],
        utility_actions: &[zeph_tools::UtilityAction],
        quota_blocked: bool,
        args_hashes: &[u64],
        repeat_blocked: &[bool],
        cache_hits: &[Option<zeph_tools::ToolOutput>],
        max_parallel: usize,
        cancel: &tokio_util::sync::CancellationToken,
        tool_call_ids: &[String],
        tool_started_ats: &mut [std::time::Instant],
    ) -> Result<TierLoopOutput, crate::agent::error::AgentError> {
        // Build a dependency DAG over tool_use_id references in call arguments. When the
        // DAG is trivial (no dependencies — the common case), we execute all calls in a
        // single tier with zero overhead. When dependencies exist, we partition calls into
        // topological tiers and execute each tier in parallel, awaiting the previous tier
        // before starting the next.
        //
        // ToolStartEvent is sent at the beginning of each tier so the UI reflects actual
        // execution start time rather than pre-build time.
        let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(max_parallel));
        let dag = super::tool_call_dag::ToolCallDag::build(tool_calls);
        let trivial = dag.is_trivial();
        let tiers = dag.tiers();
        let tier_count = tiers.len();
        // Clone the Arc before the mutable borrow loop so try_commit can be called without
        // holding a borrow on self across await points.
        let speculation_engine = self.services.speculation_engine.clone();
        tracing::debug!(
            trivial,
            tier_count,
            tool_count = tool_calls.len(),
            "tool dispatch: partitioned into tiers"
        );

        // Pre-allocate result vector; slots are filled as tiers complete.
        let mut tool_results: Vec<Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError>> =
            (0..tool_calls.len()).map(|_| Ok(None)).collect();

        // Pre-process focus tool calls (#1850) and compress_context (#2218).
        // These need &mut self and cannot run inside the parallel tier futures.
        // Pre-populate their results so the tier loop skips them.
        let pending_focus_checkpoint = self
            .preprocess_focus_compress_calls(tool_calls, &mut tool_results)
            .await;

        // Track which indices have a failed/ConfirmationRequired prerequisite so that
        // dependent calls in later tiers receive a synthetic error instead of executing.
        // IMP-02: ConfirmationRequired is treated as a failure for dependency propagation —
        // a dependent tool must not proceed when its prerequisite is awaiting user approval.
        let mut failed_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
        // Utility gate hints (Retrieve/Verify) are deferred so they are pushed after
        // User(tool_results), maintaining valid OpenAI message ordering (#2615).
        let mut pending_system_hints: Vec<String> = Vec::new();

        for (tier_idx, tier) in tiers.into_iter().enumerate() {
            if cancel.is_cancelled() {
                self.tool_executor.set_skill_env(None);
                tracing::info!("tool execution cancelled by user");
                self.update_metrics(|m| m.cancellations += 1);
                self.channel.send("[Cancelled]").await?;
                self.persist_cancelled_tool_results(tool_calls).await;
                return Ok(None);
            }

            if tier_count > 1 {
                let _ = self
                    .channel
                    .send_status(&format!(
                        "Executing tools (tier {}/{})\u{2026}",
                        tier_idx + 1,
                        tier_count
                    ))
                    .await;
            }

            // Pre-scan: commit speculative handles and emit speculative ToolStartEvents.
            let speculative_commits = self
                .commit_speculative_tier(
                    &tier.indices,
                    calls,
                    tool_calls,
                    tool_call_ids,
                    tool_started_ats,
                    speculation_engine.as_ref(),
                )
                .await?;

            // Stamp execution start time and send ToolStartEvent for non-committed calls (§3.7).
            let non_committed_indices: Vec<usize> = tier
                .indices
                .iter()
                .copied()
                .filter(|idx| !speculative_commits.contains_key(idx))
                .collect();
            self.stamp_and_send_tier_start(
                &non_committed_indices,
                tool_calls,
                tool_call_ids,
                tool_started_ats,
            )
            .await?;

            // Build futures for non-committed calls in this tier.
            let mut tier_futs = self
                .build_tier_call_futures(
                    tool_calls,
                    calls,
                    &non_committed_indices,
                    &dag,
                    &failed_ids,
                    quota_blocked,
                    pre_exec_blocked,
                    utility_actions,
                    repeat_blocked,
                    cache_hits,
                    &semaphore,
                    &mut pending_system_hints,
                )
                .await?;

            // Inject committed speculative results as ready futures.
            for (idx, result) in speculative_commits {
                tier_futs.push((idx, Box::pin(std::future::ready(result))));
            }

            // Execute futures concurrently with cancellation and MCP elicitation drain.
            let (indices, futs): (Vec<usize>, Vec<ToolExecFut>) = tier_futs.into_iter().unzip();
            let Some(tier_results) = self.execute_tier_join(futs, cancel, tool_calls).await? else {
                return Ok(None);
            };

            // Store results, update dependency graph, and run after_tool hooks.
            self.apply_tier_results(
                indices,
                tier_results,
                tool_calls,
                calls,
                cache_hits,
                args_hashes,
                tool_started_ats,
                &mut failed_ids,
                &mut tool_results,
            )
            .await;

            if tier_count > 1 {
                let _ = self.channel.send_status("").await;
            }
        }

        // Pad with empty results if needed (defensive; should not happen).
        while tool_results.len() < tool_calls.len() {
            tool_results.push(Ok(None));
        }

        Ok(Some(TierLoopData {
            tool_results,
            pending_focus_checkpoint,
            pending_system_hints,
        }))
    }

    #[tracing::instrument(
        name = "core.tool.preprocess_focus_compress",
        skip_all,
        level = "debug"
    )]
    async fn preprocess_focus_compress_calls(
        &mut self,
        tool_calls: &[zeph_llm::provider::ToolUseRequest],
        tool_results: &mut [Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError>],
    ) -> Option<zeph_llm::provider::Message> {
        let mut pending_focus_checkpoint: Option<zeph_llm::provider::Message> = None;
        for (idx, tc) in tool_calls.iter().enumerate() {
            let is_focus_tool = self.services.focus.config.enabled
                && (tc.name == "start_focus" || tc.name == "complete_focus");
            let is_compress = tc.name == "compress_context";
            if is_focus_tool || is_compress {
                let result = if is_compress {
                    self.handle_compress_context().await
                } else {
                    let (text, maybe_checkpoint) =
                        self.handle_focus_tool(tc.name.as_str(), &tc.input);
                    if let Some(cp) = maybe_checkpoint {
                        pending_focus_checkpoint = Some(cp);
                    }
                    text
                };
                tool_results[idx] = Ok(Some(skipped_output(tc.name.clone(), result)));
            }
        }
        pending_focus_checkpoint
    }

    async fn stamp_and_send_tier_start(
        &mut self,
        tier_indices: &[usize],
        tool_calls: &[zeph_llm::provider::ToolUseRequest],
        tool_call_ids: &[String],
        tool_started_ats: &mut [std::time::Instant],
    ) -> Result<(), crate::agent::error::AgentError> {
        let tier_start = std::time::Instant::now();
        for &idx in tier_indices {
            tool_started_ats[idx] = tier_start;
        }
        for &idx in tier_indices {
            let tc = &tool_calls[idx];
            self.channel
                .send_tool_start(ToolStartEvent {
                    tool_name: tc.name.clone(),
                    tool_call_id: tool_call_ids[idx].clone(),
                    params: Some(tc.input.clone()),
                    parent_tool_use_id: self.services.session.parent_tool_use_id.clone(),
                    started_at: std::time::Instant::now(),
                    speculative: false,
                    sandbox_profile: None,
                })
                .await?;
        }
        Ok(())
    }

    #[tracing::instrument(
        name = "core.tool.commit_speculative_tier",
        skip_all,
        level = "debug",
        fields(tier_size = tier_indices.len()),
        err
    )]
    pub(super) async fn commit_speculative_tier(
        &mut self,
        tier_indices: &[usize],
        calls: &[ToolCall],
        tool_calls: &[zeph_llm::provider::ToolUseRequest],
        tool_call_ids: &[String],
        tool_started_ats: &mut [std::time::Instant],
        engine: Option<&std::sync::Arc<crate::agent::speculative::SpeculationEngine>>,
    ) -> Result<
        std::collections::HashMap<
            usize,
            Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError>,
        >,
        crate::agent::error::AgentError,
    > {
        let mut commits: std::collections::HashMap<
            usize,
            Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError>,
        > = std::collections::HashMap::new();

        let Some(engine) = engine else {
            return Ok(commits);
        };

        for &idx in tier_indices {
            let Some(result) = engine.try_commit(&calls[idx]).await else {
                continue;
            };
            if let Err(ref e) = result {
                tracing::warn!(
                    tool = %calls[idx].tool_id,
                    error = %e,
                    "speculative commit returned Err — result will be used as-is"
                );
                // Invariant: ConfirmationRequired must never reach the commit boundary —
                // try_dispatch guards against it at dispatch time via requires_confirmation_erased.
                #[cfg(debug_assertions)]
                if matches!(e, zeph_tools::ToolError::ConfirmationRequired { .. }) {
                    tracing::error!(
                        tool = %calls[idx].tool_id,
                        "invariant violated: committed speculative result is ConfirmationRequired"
                    );
                }
            }
            // M1: stamp actual dispatch time so build_tool_output_messages computes correct elapsed.
            tool_started_ats[idx] = std::time::Instant::now();
            commits.insert(idx, result);
        }

        // Emit ToolStartEvent with speculative: true for all committed calls.
        for &idx in tier_indices {
            if commits.contains_key(&idx) {
                let tc = &tool_calls[idx];
                self.channel
                    .send_tool_start(ToolStartEvent {
                        tool_name: tc.name.clone(),
                        tool_call_id: tool_call_ids[idx].clone(),
                        params: Some(tc.input.clone()),
                        parent_tool_use_id: self.services.session.parent_tool_use_id.clone(),
                        started_at: tool_started_ats[idx],
                        speculative: true,
                        sandbox_profile: None,
                    })
                    .await?;
            }
        }

        Ok(commits)
    }

    async fn handle_utility_gate(
        &mut self,
        idx: usize,
        tc: &zeph_llm::provider::ToolUseRequest,
        utility_actions: &[zeph_tools::UtilityAction],
        pending_system_hints: &mut Vec<String>,
    ) -> Result<Option<(usize, ToolExecFut)>, crate::agent::error::AgentError> {
        match utility_actions[idx] {
            zeph_tools::UtilityAction::ToolCall => Ok(None),
            zeph_tools::UtilityAction::Respond => {
                let _ = self
                    .channel
                    .send_status(&format!("Utility action: Respond ({})", tc.name))
                    .await;
                Ok(Some(ready_fut(
                    idx,
                    skipped_output(
                        tc.name.clone(),
                        format!(
                            "[skipped] Tool call to {} skipped — utility policy recommends a \
                             direct response without further tool use.",
                            tc.name
                        ),
                    ),
                )))
            }
            zeph_tools::UtilityAction::Retrieve => {
                let _ = self
                    .channel
                    .send_status(&format!("Utility action: Retrieve ({})", tc.name))
                    .await;
                // Inject a system message directing the LLM to retrieve context first (#2620).
                pending_system_hints.push(format!(
                    "[utility:retrieve] Before executing the '{}' tool, retrieve \
                     relevant context via memory_search or a related lookup to ensure \
                     the call is well-targeted. After retrieving context, you MUST call \
                     the '{}' tool again with the same arguments.",
                    tc.name, tc.name
                ));
                Ok(Some(ready_fut(
                    idx,
                    skipped_output(
                        tc.name.clone(),
                        format!(
                            "[skipped] Tool call to {} skipped — utility policy recommends \
                             retrieving additional context first.",
                            tc.name
                        ),
                    ),
                )))
            }
            zeph_tools::UtilityAction::Verify => {
                let _ = self
                    .channel
                    .send_status(&format!("Utility action: Verify ({})", tc.name))
                    .await;
                pending_system_hints.push(format!(
                    "[utility:verify] Before executing the '{}' tool again, verify \
                     the result of the previous tool call to confirm it is correct \
                     and that further tool use is necessary.",
                    tc.name
                ));
                Ok(Some(ready_fut(
                    idx,
                    skipped_output(
                        tc.name.clone(),
                        format!(
                            "[skipped] Tool call to {} skipped — utility policy recommends \
                             verifying the previous result first.",
                            tc.name
                        ),
                    ),
                )))
            }
            zeph_tools::UtilityAction::Stop => {
                let _ = self
                    .channel
                    .send_status(&format!("Utility action: Stop ({})", tc.name))
                    .await;
                let threshold = self.tool_orchestrator.utility_scorer.threshold();
                Ok(Some(ready_fut(
                    idx,
                    skipped_output(
                        tc.name.clone(),
                        format!(
                            "[stopped] Tool call to {} halted by the utility gate — \
                             budget exhausted or score below threshold {threshold:.2}.",
                            tc.name
                        ),
                    ),
                )))
            }
        }
    }

    async fn run_before_tool_hooks(
        &mut self,
        idx: usize,
        tc: &zeph_llm::provider::ToolUseRequest,
        call: &ToolCall,
    ) -> Option<(usize, ToolExecFut)> {
        if self.runtime.config.layers.is_empty() {
            return None;
        }
        let conv_id_str = self
            .services
            .memory
            .persistence
            .conversation_id
            .map(|id| id.0.to_string());
        let ctx = crate::runtime_layer::LayerContext {
            conversation_id: conv_id_str.as_deref(),
            turn_number: u32::try_from(self.services.sidequest.turn_counter).unwrap_or(u32::MAX),
        };
        let mut sc_result: crate::runtime_layer::BeforeToolResult = None;
        for layer in &self.runtime.config.layers {
            let hook_result = std::panic::AssertUnwindSafe(layer.before_tool(&ctx, call))
                .catch_unwind()
                .await;
            match hook_result {
                Ok(Some(r)) => {
                    sc_result = Some(r);
                    break;
                }
                Ok(None) => {}
                Err(_) => {
                    tracing::warn!("RuntimeLayer::before_tool panicked, continuing");
                }
            }
        }
        let r = sc_result?;
        // Fire PermissionDenied hooks (fail_open: hook errors are logged, not fatal).
        let pd_hooks = self.services.session.hooks_config.permission_denied.clone();
        if !pd_hooks.is_empty() {
            let _span =
                tracing::info_span!("core.hooks.permission_denied", tool = %tc.name).entered();
            let mut env = std::collections::HashMap::new();
            env.insert("ZEPH_DENIED_TOOL".to_owned(), tc.name.to_string());
            env.insert("ZEPH_DENY_REASON".to_owned(), r.reason.clone());
            let dispatch = self.mcp_dispatch();
            let mcp: Option<&dyn zeph_subagent::McpDispatch> = dispatch
                .as_ref()
                .map(|d| d as &dyn zeph_subagent::McpDispatch);
            // TODO: implement retry-on-{"retry":true} stdout signal (#3292)
            if let Err(e) = zeph_subagent::hooks::fire_hooks(&pd_hooks, &env, mcp).await {
                tracing::warn!(error = %e, tool = %tc.name, "PermissionDenied hook failed");
            }
        }
        Some((idx, Box::pin(std::future::ready(r.result))))
    }

    #[allow(clippy::too_many_arguments)]
    async fn check_call_gates(
        &mut self,
        idx: usize,
        tc: &zeph_llm::provider::ToolUseRequest,
        has_failed_dep: bool,
        quota_blocked: bool,
        pre_exec_blocked: &[bool],
        utility_actions: &[zeph_tools::UtilityAction],
        repeat_blocked: &[bool],
        pending_system_hints: &mut Vec<String>,
    ) -> Result<Option<(usize, ToolExecFut)>, crate::agent::error::AgentError> {
        if has_failed_dep {
            return Ok(Some(ready_fut(
                idx,
                skipped_output(
                    tc.name.clone(),
                    "[error] Skipped: a prerequisite tool failed or requires confirmation",
                ),
            )));
        }
        if quota_blocked {
            let max = self
                .tool_orchestrator
                .max_tool_calls_per_session
                .unwrap_or(0);
            return Ok(Some(ready_fut(
                idx,
                skipped_output(
                    tc.name.clone(),
                    format!(
                        "[error] Tool call quota exceeded (session limit: {max} calls). \
                         No further tool calls are allowed this session."
                    ),
                ),
            )));
        }
        if pre_exec_blocked[idx] {
            return Ok(Some(ready_fut(
                idx,
                skipped_output(
                    tc.name.clone(),
                    format!(
                        "[error] Tool call to {} was blocked by pre-execution verifier. \
                         The requested operation is not permitted.",
                        tc.name
                    ),
                ),
            )));
        }
        if let Some(fut) = self
            .handle_utility_gate(idx, tc, utility_actions, pending_system_hints)
            .await?
        {
            return Ok(Some(fut));
        }
        if repeat_blocked[idx] {
            return Ok(Some(ready_fut(
                idx,
                skipped_output(
                    tc.name.clone(),
                    format!(
                        "[error] Repeated identical call to {} detected. \
                         Use different arguments or a different approach.",
                        tc.name
                    ),
                ),
            )));
        }
        Ok(None)
    }

    #[tracing::instrument(
        name = "core.tool.build_tier_call_futures",
        skip_all,
        level = "debug",
        fields(tier_size = tier_indices.len()),
        err
    )]
    #[allow(clippy::too_many_arguments, clippy::ptr_arg, clippy::too_many_lines)]
    async fn build_tier_call_futures(
        &mut self,
        tool_calls: &[zeph_llm::provider::ToolUseRequest],
        calls: &[ToolCall],
        tier_indices: &[usize],
        dag: &super::tool_call_dag::ToolCallDag,
        failed_ids: &std::collections::HashSet<String>,
        quota_blocked: bool,
        pre_exec_blocked: &[bool],
        utility_actions: &[zeph_tools::UtilityAction],
        repeat_blocked: &[bool],
        cache_hits: &[Option<zeph_tools::ToolOutput>],
        semaphore: &std::sync::Arc<tokio::sync::Semaphore>,
        pending_system_hints: &mut Vec<String>,
    ) -> Result<Vec<(usize, ToolExecFut)>, crate::agent::error::AgentError> {
        let tier_tool_names: Vec<&str> = tier_indices
            .iter()
            .map(|&i| tool_calls[i].name.as_str())
            .collect();
        let rate_results = self
            .runtime
            .config
            .rate_limiter
            .check_batch(&tier_tool_names);

        let mut tier_futs: Vec<(usize, ToolExecFut)> = Vec::with_capacity(tier_indices.len());
        for (tier_local_idx, &idx) in tier_indices.iter().enumerate() {
            let tc = &tool_calls[idx];
            let call = &calls[idx];

            // Skip focus tools and compress_context — pre-handled before the tier loop.
            if tc.name == "compress_context"
                || (self.services.focus.config.enabled
                    && (tc.name == "start_focus" || tc.name == "complete_focus"))
            {
                continue;
            }

            // Check static gates: dep failure, quota, pre-exec block, utility gate, repeat.
            let has_failed_dep = dag
                .string_values_for(idx)
                .iter()
                .any(|v| failed_ids.contains(v));
            if let Some(fut) = self
                .check_call_gates(
                    idx,
                    tc,
                    has_failed_dep,
                    quota_blocked,
                    pre_exec_blocked,
                    utility_actions,
                    repeat_blocked,
                    pending_system_hints,
                )
                .await?
            {
                tier_futs.push(fut);
                continue;
            }

            // Cache hit: return pre-computed result without executing the tool.
            if let Some(cached_output) = cache_hits[idx].clone() {
                tracing::debug!(
                    tool = %tc.name,
                    "[tool-cache] returning cached result, skipping execution"
                );
                tier_futs.push((idx, Box::pin(std::future::ready(Ok(Some(cached_output))))));
                continue;
            }

            // Rate limiter: check the pre-computed batch result for this call.
            if let Some(ref exceeded) = rate_results[tier_local_idx] {
                tracing::warn!(
                    tool = %tc.name,
                    category = exceeded.category.as_str(),
                    limit = exceeded.limit,
                    "tool rate limiter: blocking call"
                );
                self.update_metrics(|m| m.rate_limit_trips += 1);
                self.push_security_event(
                    zeph_common::SecurityEventCategory::RateLimit,
                    tc.name.as_str(),
                    format!(
                        "{} calls exceeded {}/min",
                        exceeded.category.as_str(),
                        exceeded.limit
                    ),
                );
                tier_futs.push(ready_fut(
                    idx,
                    skipped_output(tc.name.clone(), exceeded.to_error_message()),
                ));
                continue;
            }

            // Fire PreToolUse hooks before the RuntimeLayer permission check (fail-open).
            let pre_hooks = self.services.session.hooks_config.pre_tool_use.clone();
            if !pre_hooks.is_empty() {
                let matched: Vec<&zeph_config::HookDef> =
                    zeph_subagent::matching_hooks(&pre_hooks, tc.name.as_str());
                if !matched.is_empty() {
                    let _span =
                        tracing::info_span!("core.hooks.pre_tool_use", tool = %tc.name).entered();
                    let conv_id_str = self
                        .services
                        .memory
                        .persistence
                        .conversation_id
                        .map(|id| id.0.to_string());
                    let env =
                        make_tool_hook_env(tc.name.as_str(), &tc.input, conv_id_str.as_deref());
                    let owned: Vec<zeph_config::HookDef> = matched.into_iter().cloned().collect();
                    let dispatch = self.mcp_dispatch();
                    let mcp: Option<&dyn zeph_subagent::McpDispatch> = dispatch
                        .as_ref()
                        .map(|d| d as &dyn zeph_subagent::McpDispatch);
                    if let Err(e) = zeph_subagent::hooks::fire_hooks(&owned, &env, mcp).await {
                        tracing::warn!(
                            error = %e,
                            tool = %tc.name,
                            "PreToolUse hook failed"
                        );
                    }
                }
            }

            if let Some(fut) = self.run_before_tool_hooks(idx, tc, call).await {
                tier_futs.push(fut);
                continue;
            }

            // Speculative try_commit (#3641): reuse a pre-executed result when available.
            // Uses the LLM-assigned `tool_use_id` (tc.id) for result routing (critic H3).
            // TODO(#3645): add circuit-breaker check when implemented.
            if let Some(engine) = self.services.speculation_engine.as_ref()
                && let Some(result) =
                    crate::agent::speculative::stream_drainer::try_commit_with_timeout(engine, call)
                        .await
            {
                tracing::debug!(tool = %tc.name, llm_id = %tc.id, "speculative try_commit hit");
                tier_futs.push((idx, Box::pin(std::future::ready(result))));
                continue;
            }

            tier_futs.push(self.make_exec_future(idx, tc, call, semaphore));
        }
        Ok(tier_futs)
    }

    fn make_exec_future(
        &self,
        idx: usize,
        tc: &zeph_llm::provider::ToolUseRequest,
        call: &ToolCall,
        semaphore: &std::sync::Arc<tokio::sync::Semaphore>,
    ) -> (usize, ToolExecFut) {
        let sem = std::sync::Arc::clone(semaphore);
        let executor = std::sync::Arc::clone(&self.tool_executor);
        let call = call.clone();
        let tool_name = tc.name.clone();
        let tool_id = tc.id.clone();
        let fut = async move {
            let _permit = sem.acquire().await.map_err(|_| {
                zeph_tools::ToolError::Execution(std::io::Error::other(
                    "semaphore closed during tool execution",
                ))
            })?;
            executor
                .execute_tool_call_erased(&call)
                .instrument(tracing::info_span!(
                    "tool_exec",
                    tool_name = %tool_name,
                    idx = %tool_id
                ))
                .await
        };
        (idx, Box::pin(fut))
    }

    #[tracing::instrument(name = "core.tool.execute_tier_join", skip_all, level = "debug", err)]
    #[allow(clippy::type_complexity)]
    async fn execute_tier_join(
        &mut self,
        futs: Vec<ToolExecFut>,
        cancel: &tokio_util::sync::CancellationToken,
        tool_calls: &[zeph_llm::provider::ToolUseRequest],
    ) -> Result<
        Option<Vec<Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError>>>,
        crate::agent::error::AgentError,
    > {
        let mut join_fut = std::pin::pin!(futures::future::join_all(futs));
        // Take elicitation_rx out of self so we can hold &mut self for handling.
        let mut elicitation_rx = self.services.mcp.elicitation_rx.take();
        let result = loop {
            tokio::select! {
                results = &mut join_fut => break results,
                () = cancel.cancelled() => {
                    self.services.mcp.elicitation_rx = elicitation_rx;
                    self.tool_executor.set_skill_env(None);
                    tracing::info!("tool execution cancelled by user");
                    self.update_metrics(|m| m.cancellations += 1);
                    self.channel.send("[Cancelled]").await?;
                    self.persist_cancelled_tool_results(tool_calls).await;
                    return Ok(None);
                }
                event = recv_elicitation(&mut elicitation_rx) => {
                    if let Some(ev) = event {
                        self.handle_elicitation_event(ev).await;
                    } else {
                        tracing::debug!("elicitation channel closed during tier exec");
                        elicitation_rx = None;
                    }
                }
            }
        };
        self.services.mcp.elicitation_rx = elicitation_rx;
        Ok(Some(result))
    }

    #[allow(clippy::too_many_arguments)]
    async fn apply_tier_results(
        &mut self,
        indices: Vec<usize>,
        tier_results: Vec<Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError>>,
        tool_calls: &[zeph_llm::provider::ToolUseRequest],
        calls: &[ToolCall],
        cache_hits: &[Option<zeph_tools::ToolOutput>],
        args_hashes: &[u64],
        tool_started_ats: &[std::time::Instant],
        failed_ids: &mut std::collections::HashSet<String>,
        tool_results: &mut [Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError>],
    ) {
        for (idx, result) in indices.into_iter().zip(tier_results) {
            // IMP-02: Err(_) covers all error variants including ConfirmationRequired.
            // Ok(Some(out)) with "[error]" prefix covers synthetic/blocked results.
            let is_failed = match &result {
                Err(_) => true,
                Ok(Some(out)) => out.summary.starts_with("[error]"),
                Ok(None) => false,
            };
            if is_failed {
                failed_ids.insert(tool_calls[idx].id.clone());
            }

            // Store successful, non-cached results in the tool result cache.
            if !is_failed
                && cache_hits[idx].is_none()
                && zeph_tools::is_cacheable(tool_calls[idx].name.as_str())
                && let Ok(Some(ref out)) = result
            {
                let key =
                    zeph_tools::CacheKey::new(tool_calls[idx].name.to_string(), args_hashes[idx]);
                self.tool_orchestrator.result_cache.put(key, out.clone());
            }

            // Record successful tool completions for the dependency graph (#2024).
            if !is_failed && self.services.tool_state.dependency_graph.is_some() {
                self.services
                    .tool_state
                    .completed_tool_ids
                    .insert(tool_calls[idx].name.to_string());
            }

            // RuntimeLayer after_tool hooks.
            if !self.runtime.config.layers.is_empty() {
                let conv_id_str = self
                    .services
                    .memory
                    .persistence
                    .conversation_id
                    .map(|id| id.0.to_string());
                let ctx = crate::runtime_layer::LayerContext {
                    conversation_id: conv_id_str.as_deref(),
                    turn_number: u32::try_from(self.services.sidequest.turn_counter)
                        .unwrap_or(u32::MAX),
                };
                for layer in &self.runtime.config.layers {
                    let hook_result =
                        std::panic::AssertUnwindSafe(layer.after_tool(&ctx, &calls[idx], &result))
                            .catch_unwind()
                            .await;
                    if hook_result.is_err() {
                        tracing::warn!("RuntimeLayer::after_tool panicked, continuing");
                    }
                }
            }

            // Fire PostToolUse hooks after the tool result is available (fail-open).
            let post_hooks = self.services.session.hooks_config.post_tool_use.clone();
            if !post_hooks.is_empty() {
                let matched: Vec<&zeph_config::HookDef> =
                    zeph_subagent::matching_hooks(&post_hooks, tool_calls[idx].name.as_str());
                if !matched.is_empty() {
                    let _span = tracing::info_span!(
                        "core.hooks.post_tool_use",
                        tool = %tool_calls[idx].name
                    )
                    .entered();
                    let conv_id_str = self
                        .services
                        .memory
                        .persistence
                        .conversation_id
                        .map(|id| id.0.to_string());
                    let mut env = make_tool_hook_env(
                        tool_calls[idx].name.as_str(),
                        &tool_calls[idx].input,
                        conv_id_str.as_deref(),
                    );
                    let duration_ms = tool_started_ats[idx].elapsed().as_millis();
                    env.insert("ZEPH_TOOL_DURATION_MS".to_owned(), duration_ms.to_string());
                    let owned: Vec<zeph_config::HookDef> = matched.into_iter().cloned().collect();
                    let dispatch = self.mcp_dispatch();
                    let mcp: Option<&dyn zeph_subagent::McpDispatch> = dispatch
                        .as_ref()
                        .map(|d| d as &dyn zeph_subagent::McpDispatch);
                    if let Err(e) = zeph_subagent::hooks::fire_hooks(&owned, &env, mcp).await {
                        tracing::warn!(
                            error = %e,
                            tool = %tool_calls[idx].name,
                            "PostToolUse hook failed"
                        );
                    }
                }
            }

            tool_results[idx] = result;
        }
    }

    #[tracing::instrument(
        name = "core.tool.run_causal_ipi_post_probe",
        skip_all,
        level = "debug"
    )]
    async fn run_causal_ipi_post_probe(
        &mut self,
        causal_pre_response: Option<(String, String)>,
        result_parts: &[MessagePart],
    ) {
        let Some((pre_response, context_summary)) = causal_pre_response else {
            return;
        };
        let snippets: Vec<String> = result_parts
            .iter()
            .filter_map(|p| {
                if let MessagePart::ToolResult {
                    content, is_error, ..
                } = p
                {
                    if *is_error {
                        Some(zeph_sanitizer::causal_ipi::format_error_snippet(content))
                    } else {
                        Some(zeph_sanitizer::causal_ipi::format_tool_snippet(content))
                    }
                } else {
                    None
                }
            })
            .collect();
        let tool_snippets = if snippets.is_empty() {
            "[empty]".to_owned()
        } else {
            snippets.join("---")
        };
        let Some(ref analyzer) = self.services.security.causal_analyzer else {
            return;
        };
        match analyzer.post_probe(&context_summary, &tool_snippets).await {
            Ok(post_response) => {
                let analysis = analyzer.analyze(&pre_response, &post_response);
                if analysis.is_flagged {
                    let pre_excerpt = &pre_response[..pre_response.floor_char_boundary(100)];
                    let post_excerpt = &post_response[..post_response.floor_char_boundary(100)];
                    tracing::warn!(
                        deviation_score = analysis.deviation_score,
                        threshold = analyzer.threshold(),
                        pre = %pre_excerpt,
                        post = %post_excerpt,
                        "causal IPI: behavioral deviation detected at tool-return boundary"
                    );
                    self.update_metrics(|m| m.causal_ipi_flags += 1);
                    self.push_security_event(
                        zeph_common::SecurityEventCategory::CausalIpiFlag,
                        "tool_batch",
                        format!("deviation={:.3}", analysis.deviation_score),
                    );
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "causal IPI post-probe failed, skipping analysis");
            }
        }
    }

    #[tracing::instrument(
        name = "core.tool.process_tool_result_batch",
        skip_all,
        level = "debug",
        fields(batch_size = tool_calls.len()),
        err
    )]
    #[allow(clippy::too_many_arguments)]
    async fn process_tool_result_batch(
        &mut self,
        tool_calls: &[zeph_llm::provider::ToolUseRequest],
        tool_call_ids: &[String],
        tool_started_ats: &[std::time::Instant],
        mut tool_results: Vec<Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError>>,
        causal_pre_response: Option<(String, String)>,
        pending_focus_checkpoint: Option<zeph_llm::provider::Message>,
        pending_system_hints: Vec<String>,
    ) -> Result<(), crate::agent::error::AgentError> {
        self.tool_executor.set_skill_env(None);

        // Sync cache counters to metrics after all tool execution is complete.
        {
            let hits = self.tool_orchestrator.result_cache.hits();
            let misses = self.tool_orchestrator.result_cache.misses();
            let entries = self.tool_orchestrator.result_cache.len();
            self.update_metrics(|m| {
                m.tool_cache_hits = hits;
                m.tool_cache_misses = misses;
                m.tool_cache_entries = entries;
            });
        }

        // Collect (name, params, output) for LSP hooks. Built during the results loop below.
        let mut lsp_tool_calls: Vec<(String, serde_json::Value, String)> = Vec::new();

        // Process results sequentially (metrics, channel sends, message parts).
        // self_reflection is deferred until after all result_parts are assembled and user_msg
        // is pushed to history. Calling it inside the loop would insert a reflection dialogue
        // (User{prompt} + Assistant{response}) between Assistant{ToolUse} and User{ToolResults},
        // violating the OpenAI/Claude API message ordering protocol → HTTP 400.
        let mut result_parts: Vec<MessagePart> = Vec::new();
        // Accumulates injection flags across all tools in the batch (Bug #1490 fix).
        let mut has_any_injection_flags = false;
        // Deferred self-reflection: set to the sanitized error output of the first failing tool
        // that is eligible for reflection. Consumed after user_msg is pushed to history.
        let mut pending_reflection: Option<String> = None;
        // Accumulate skill outcomes during the tool loop; flushed once after the loop via
        // flush_skill_outcomes to avoid N×M×13 sequential SQLite awaits (#2770).
        let mut pending_outcomes: Vec<crate::agent::learning::PendingSkillOutcome> = Vec::new();
        for idx in 0..tool_calls.len() {
            let tc = &tool_calls[idx];
            let tool_call_id = &tool_call_ids[idx];
            let started_at = &tool_started_ats[idx];
            let tool_result = std::mem::replace(&mut tool_results[idx], Ok(None));
            self.process_one_tool_result(
                tc,
                tool_call_id,
                started_at,
                tool_result,
                &mut result_parts,
                &mut lsp_tool_calls,
                &mut has_any_injection_flags,
                &mut pending_reflection,
                &mut pending_outcomes,
            )
            .await?;
        }

        // Flush all accumulated skill outcomes from the tool batch in a single pass.
        // This replaces the per-tool record_skill_outcomes calls that caused N×M sequential
        // SQLite awaits (#2770).
        self.flush_skill_outcomes(pending_outcomes).await;

        self.run_causal_ipi_post_probe(causal_pre_response, &result_parts)
            .await;

        let user_msg = Message::from_parts(Role::User, result_parts);
        // flagged_urls accumulates across ALL tools in this batch (cross-tool trust boundary).
        // A URL from tool N's output can flag tool M's arguments even if tool M returned clean
        // output. has_any_injection_flags covers pure text injections (no URL); flagged_urls
        // covers URL-based exfiltration. Both are OR-combined for conservative guarding.
        // Individual per-tool granularity would require separate persist_message calls per
        // result, which would change message history structure.
        let tool_results_have_flags =
            has_any_injection_flags || !self.services.security.flagged_urls.is_empty();
        tracing::debug!("tool_batch: calling persist_message for tool results");
        self.persist_message(
            Role::User,
            &user_msg.content,
            &user_msg.parts,
            tool_results_have_flags,
        )
        .await;
        tracing::debug!("tool_batch: persist_message done, pushing message");
        self.push_message(user_msg);
        tracing::debug!("tool_batch: message pushed, starting LSP hooks");
        if let (Some(id), Some(last)) = (
            self.msg.last_persisted_message_id,
            self.msg.messages.last_mut(),
        ) {
            last.metadata.db_id = Some(id);
        }

        // Flush deferred start_focus checkpoint AFTER User(tool_results) so the ordering
        // Assistant→User→System is valid for OpenAI (#3262).
        if let Some(checkpoint) = pending_focus_checkpoint {
            self.push_message(checkpoint);
        }

        // Flush deferred utility gate hints (Retrieve/Verify). Pushed after User(tool_results)
        // so the ordering Assistant→User→System is valid for OpenAI (#2615).
        for hint in pending_system_hints {
            self.push_message(zeph_llm::provider::Message::from_legacy(
                zeph_llm::provider::Role::System,
                &hint,
            ));
        }

        // Deferred self-reflection: user_msg is now in history so the reflection dialogue
        // (User{prompt} + Assistant{response}) appends after User{ToolResults}, preserving
        // API message ordering. Only the first eligible error per batch triggers reflection.
        if let Some(sanitized_out) = pending_reflection {
            match self
                .attempt_self_reflection(&sanitized_out, &sanitized_out)
                .await
            {
                Ok(_) | Err(_) => {
                    // Whether reflection succeeded, declined, or errored: the ToolResults are
                    // already committed to history. Return Ok regardless so the caller continues
                    // the tool loop normally (#2197).
                }
            }
        }

        // Fire LSP hooks for each completed tool call (non-blocking: diagnostics fetch
        // is spawned in background; hover calls are awaited but short-lived).
        // `lsp_tool_calls` collects (name, params, output) tuples built during the
        // results loop above. They are captured into a separate Vec so we can call
        // `&mut self.services.session.lsp_hooks` without conflicting borrows.
        //
        // The entire batch is capped at 30s to prevent stalls when many files are
        // modified in one tool batch (#2750). Per the critic review, a single outer
        // timeout is more effective than per-call timeouts because it bounds total
        // blocking time regardless of N.
        if self.services.session.lsp_hooks.is_some() {
            let tc_arc = std::sync::Arc::clone(&self.runtime.metrics.token_counter);
            let sanitizer = self.services.security.sanitizer.clone();
            let _ = self.channel.send_status("Analyzing changes...").await;
            // TODO: cooperative MCP cancellation — dropped futures here may leave
            // in-flight MCP JSON-RPC requests pending until the server-side timeout.
            let lsp_result = tokio::time::timeout(std::time::Duration::from_secs(30), async {
                for (name, input, output) in lsp_tool_calls {
                    if let Some(ref mut lsp) = self.services.session.lsp_hooks {
                        lsp.after_tool(&name, &input, &output, &tc_arc, &sanitizer)
                            .await;
                    }
                }
            })
            .await;
            let _ = self.channel.send_status("").await;
            if lsp_result.is_err() {
                tracing::warn!("LSP after_tool batch timed out (30s)");
            }
            tracing::debug!("tool_batch: LSP hooks done");
        }

        // Defense-in-depth: check if process cwd changed during this tool batch.
        // Normally only changes via set_working_directory; this also catches any
        // future code path that calls set_current_dir.
        self.check_cwd_changed().await;

        Ok(())
    }
}

async fn recv_elicitation(
    rx: &mut Option<tokio::sync::mpsc::Receiver<zeph_mcp::ElicitationEvent>>,
) -> Option<zeph_mcp::ElicitationEvent> {
    match rx {
        Some(r) => r.recv().await,
        None => std::future::pending().await,
    }
}

fn skipped_output(
    tool_name: impl Into<zeph_common::ToolName>,
    summary: impl Into<String>,
) -> zeph_tools::ToolOutput {
    zeph_tools::ToolOutput {
        tool_name: tool_name.into(),
        summary: summary.into(),
        blocks_executed: 0,
        filter_stats: None,
        diff: None,
        streamed: false,
        terminal_id: None,
        locations: None,
        raw_response: None,
        claim_source: None,
    }
}

fn ready_fut(idx: usize, out: zeph_tools::ToolOutput) -> (usize, ToolExecFut) {
    (idx, Box::pin(std::future::ready(Ok(Some(out)))))
}

impl<C: Channel> Agent<C> {
    #[tracing::instrument(name = "core.tool.native_loop", skip_all, level = "debug", err)]
    pub(super) async fn process_response_native_tools(
        &mut self,
    ) -> Result<(), crate::agent::error::AgentError> {
        self.tool_orchestrator.clear_doom_history();
        self.tool_orchestrator.clear_recent_tool_calls();
        self.tool_orchestrator.clear_utility_state();

        // `mut` required when context-compression is enabled to inject focus tool definitions.
        let tafc = &self.tool_orchestrator.tafc;
        let mut tool_defs: Vec<ToolDefinition> = self
            .tool_executor
            .tool_definitions_erased()
            .iter()
            .map(|def| super::tool_def_to_definition_with_tafc(def, tafc))
            .collect();

        // Inject focus tool definitions when the feature is enabled and configured (#1850).
        if self.services.focus.config.enabled {
            tool_defs.extend(crate::agent::focus::focus_tool_definitions());
        }

        // Inject compress_context tool — always available when context-compression is enabled (#2218).
        tool_defs.push(crate::agent::focus::compress_context_tool_definition());

        // Pre-compute the full tool set for iterations 1+ before filtering.
        let all_tool_defs = tool_defs.clone();

        // Iteration 0: apply dynamic tool schema filter (#2020) if cached IDs are available.
        if let Some(ref filtered_ids) = self.services.tool_state.cached_filtered_tool_ids {
            tool_defs.retain(|d| filtered_ids.contains(d.name.as_str()));
            tracing::debug!(
                filtered = tool_defs.len(),
                total = all_tool_defs.len(),
                "tool schema filter: iteration 0 using filtered tool set"
            );
        }

        tracing::debug!(
            tool_count = tool_defs.len(),
            tools = ?tool_defs.iter().map(|t| &t.name).collect::<Vec<_>>(),
            "native tool_use: collected tool definitions"
        );

        let query_embedding = match self.check_response_cache().await? {
            CacheCheckResult::Hit(cached) => {
                self.persist_message(Role::Assistant, &cached, &[], false)
                    .await;
                self.msg
                    .messages
                    .push(Message::from_legacy(Role::Assistant, cached.as_str()));
                if cached.contains(zeph_llm::provider::MAX_TOKENS_TRUNCATION_MARKER) {
                    let _ = self.channel.send_stop_hint(StopHint::MaxTokens).await;
                }
                self.channel.flush_chunks().await?;
                return Ok(());
            }
            CacheCheckResult::Miss { query_embedding } => query_embedding,
        };

        for iteration in 0..self.tool_orchestrator.max_iterations {
            if *self.runtime.lifecycle.shutdown.borrow() {
                tracing::info!("native tool loop interrupted by shutdown");
                break;
            }
            if self.runtime.lifecycle.cancel_token.is_cancelled() {
                tracing::info!("native tool loop cancelled by user");
                break;
            }
            // Iteration 0 uses filtered tool_defs (schema filter + dependency gates).
            // Iterations 1+ expand to the full set but still apply hard dependency gates
            // so tools with unmet `requires` cannot re-enter through the expansion path (#2024).
            let defs_for_iter: Vec<ToolDefinition>;
            let defs_for_turn: &[ToolDefinition] = if iteration == 0 {
                &tool_defs
            } else {
                defs_for_iter = build_gated_defs_for_iteration(
                    iteration,
                    &all_tool_defs,
                    &self.services.tool_state,
                );
                &defs_for_iter
            };
            // None = continue loop, Some(()) = return Ok, Err = propagate
            if self
                .process_single_native_turn(defs_for_turn, iteration, query_embedding.clone())
                .await?
                .is_some()
            {
                return Ok(());
            }
            if self.check_doom_loop(iteration).await? {
                break;
            }
        }

        let _ = self.channel.send_stop_hint(StopHint::MaxTurnRequests).await;
        self.channel.flush_chunks().await?;
        Ok(())
    }

    /// Returns `true` if a doom loop was detected and the caller should break.
    async fn check_doom_loop(
        &mut self,
        iteration: usize,
    ) -> Result<bool, crate::agent::error::AgentError> {
        if let Some(last_msg) = self.msg.messages.last() {
            let hash = zeph_agent_tools::doom_loop_hash(&last_msg.content);
            tracing::debug!(
                iteration,
                hash,
                content_len = last_msg.content.len(),
                content_preview = &last_msg.content[..last_msg.content.len().min(120)],
                "doom-loop hash recorded"
            );
            self.tool_orchestrator.push_doom_hash(hash);
            if self.tool_orchestrator.is_doom_loop() {
                tracing::warn!(
                    iteration,
                    hash,
                    content_len = last_msg.content.len(),
                    content_preview = &last_msg.content[..last_msg.content.len().min(200)],
                    "doom-loop detected: {} consecutive identical outputs",
                    crate::agent::DOOM_LOOP_WINDOW
                );
                self.channel
                    .send("Stopping: detected repeated identical tool outputs.")
                    .await?;
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Execute one turn of the native tool loop. Returns `Ok(Some(()))` when the LLM produced
    /// a terminal text response (caller should return `Ok(())`), `Ok(None)` to continue the
    /// loop, or `Err` on a hard error.
    #[tracing::instrument(
        name = "core.tool.single_turn",
        skip_all,
        level = "debug",
        fields(iteration),
        err
    )]
    async fn process_single_native_turn(
        &mut self,
        tool_defs: &[ToolDefinition],
        iteration: usize,
        query_embedding: Option<Vec<f32>>,
    ) -> Result<Option<()>, crate::agent::error::AgentError> {
        // Track iteration for BudgetHint injection (#2267).
        self.services.tool_state.current_tool_iteration = iteration;
        self.channel.send_typing().await?;

        if let Some(ref budget) = self.context_manager.budget {
            let used =
                usize::try_from(self.runtime.providers.cached_prompt_tokens).unwrap_or(usize::MAX);
            let threshold = budget.max_tokens() * 4 / 5;
            if used >= threshold {
                tracing::warn!(
                    iteration,
                    used,
                    threshold,
                    "stopping tool loop: context budget nearing limit"
                );
                self.channel
                    .send("Stopping: context window is nearly full.")
                    .await?;
                return Ok(Some(()));
            }
        }

        // Show triage status indicator before inference when triage routing is active.
        if matches!(self.provider, zeph_llm::any::AnyProvider::Triage(_)) {
            let _ = self.channel.send_status("Evaluating complexity...").await;
        } else {
            let _ = self.channel.send_status("thinking...").await;
        }
        let chat_result = self.call_chat_with_tools_retry(tool_defs, 2).await?;
        let _ = self.channel.send_status("").await;

        let Some(chat_result) = chat_result else {
            tracing::debug!("chat_with_tools returned None (timeout)");
            return Ok(Some(()));
        };

        tracing::debug!(iteration, ?chat_result, "native tool loop iteration");

        if let zeph_llm::provider::ChatResponse::Text(text) = &chat_result {
            // RV-1: response verification before delivery.
            if self.run_response_verification(text) {
                let _ = self
                    .channel
                    .send("[security] Response blocked by injection detection.")
                    .await;
                self.channel.flush_chunks().await?;
                return Ok(Some(()));
            }
            let cleaned = self.scan_output_and_warn(text);
            if !cleaned.is_empty() {
                let display = self.maybe_redact(&cleaned);
                self.channel.send(&display).await?;
            }
            self.store_response_in_cache(&cleaned, query_embedding)
                .await;
            self.persist_message(Role::Assistant, &cleaned, &[], false)
                .await;
            self.msg
                .messages
                .push(Message::from_legacy(Role::Assistant, cleaned.as_str()));
            // Detect context loss after compaction and log failure pair if found.
            self.maybe_log_compression_failure(&cleaned).await;
            if cleaned.contains(zeph_llm::provider::MAX_TOKENS_TRUNCATION_MARKER) {
                let _ = self.channel.send_stop_hint(StopHint::MaxTokens).await;
            }
            return Ok(Some(()));
        }

        let zeph_llm::provider::ChatResponse::ToolUse {
            text,
            tool_calls,
            thinking_blocks,
        } = chat_result
        else {
            unreachable!();
        };
        self.preserve_thinking_blocks(thinking_blocks);
        self.handle_native_tool_calls(text.as_deref(), &tool_calls)
            .await?;

        // Summarize before pruning; apply deferred summaries after pruning.
        self.maybe_summarize_tool_pair().await;
        let keep_recent = 2 * self.services.memory.persistence.tool_call_cutoff + 2;
        self.prune_stale_tool_outputs(keep_recent);
        self.maybe_apply_deferred_summaries();
        self.flush_deferred_summaries().await;
        // Mid-iteration soft compaction: fires after summarization so fresh results are
        // either summarized or protected before pruning. Does not touch turn counters,
        // cooldown, or trigger Hard tier (no LLM call during tool loop).
        self.maybe_soft_compact_mid_iteration();
        self.flush_deferred_summaries().await;

        Ok(None)
    }
}

/// Build the tool definition slice for iterations 1+ of the native tool loop.
///
/// Applies hard dependency-gate filtering when a dependency graph is configured, ensuring tools
/// with unmet `requires` cannot re-enter through the expansion path after iteration 0 (#2024).
///
/// Returns the allowed set as an owned `Vec`; the caller holds a reference into it.
/// When no dependency graph is present the full `all_tool_defs` slice is returned as-is (cloned).
fn build_gated_defs_for_iteration(
    iteration: usize,
    all_tool_defs: &[ToolDefinition],
    tool_state: &crate::agent::state::ToolState,
) -> Vec<ToolDefinition> {
    let Some(ref dep_graph) = tool_state.dependency_graph else {
        return all_tool_defs.to_vec();
    };
    if dep_graph.is_empty() {
        return all_tool_defs.to_vec();
    }

    let names: Vec<&str> = all_tool_defs.iter().map(|d| d.name.as_str()).collect();
    let allowed = dep_graph.filter_tool_names(
        &names,
        &tool_state.completed_tool_ids,
        &tool_state.dependency_always_on,
    );
    let allowed_set: std::collections::HashSet<&str> = allowed.into_iter().collect();

    // Deadlock fallback: if all non-always-on tools would be blocked, use the full set.
    let non_ao_allowed = allowed_set
        .iter()
        .filter(|n| !tool_state.dependency_always_on.contains(**n))
        .count();
    let non_ao_total = all_tool_defs
        .iter()
        .filter(|d| !tool_state.dependency_always_on.contains(d.name.as_str()))
        .count();
    if non_ao_allowed == 0 && non_ao_total > 0 {
        tracing::warn!(
            iteration,
            "tool dependency graph: all non-always-on tools gated on iter 1+; \
             disabling hard gates for this iteration"
        );
        return all_tool_defs.to_vec();
    }

    all_tool_defs
        .iter()
        .filter(|d| allowed_set.contains(d.name.as_str()))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json_val(s: &str) -> serde_json::Value {
        serde_json::from_str(s).unwrap()
    }

    #[test]
    fn make_tool_hook_env_sets_tool_name() {
        let env = make_tool_hook_env("Edit", &serde_json::Value::Null, None);
        assert_eq!(env.get("ZEPH_TOOL_NAME").map(String::as_str), Some("Edit"));
    }

    #[test]
    fn make_tool_hook_env_sets_args_json_for_small_payload() {
        let input = json_val(r#"{"path": "/tmp/foo.txt"}"#);
        let env = make_tool_hook_env("Write", &input, None);
        let args = env
            .get("ZEPH_TOOL_ARGS_JSON")
            .expect("ZEPH_TOOL_ARGS_JSON missing");
        let parsed: serde_json::Value = serde_json::from_str(args).unwrap();
        assert_eq!(parsed["path"], "/tmp/foo.txt");
    }

    #[test]
    fn make_tool_hook_env_truncates_large_payload_safely() {
        // Build a JSON string > 64 KiB with a multi-byte char near the boundary.
        let mut big = String::from(r#"{"data":""#);
        // Fill mostly with ASCII, then add a 3-byte char (€ = 0xE2 0x82 0xAC) right at boundary.
        // We want the char boundary to fall inside the limit so truncation must round down.
        while big.len() < TOOL_ARGS_JSON_LIMIT - 3 {
            big.push('a');
        }
        big.push('€'); // 3 bytes — may straddle the limit
        while big.len() < TOOL_ARGS_JSON_LIMIT + 100 {
            big.push('b');
        }
        big.push_str(r#""}"#);
        let input: serde_json::Value = serde_json::from_str(&big).unwrap_or_default();
        // Must not panic and must end with the ellipsis character.
        let env = make_tool_hook_env("Shell", &input, None);
        let args = env
            .get("ZEPH_TOOL_ARGS_JSON")
            .expect("ZEPH_TOOL_ARGS_JSON missing");
        assert!(
            args.ends_with('…'),
            "truncated value should end with ellipsis"
        );
        assert!(
            args.is_char_boundary(args.len()),
            "truncation must land on char boundary"
        );
    }

    #[test]
    fn make_tool_hook_env_sets_session_id_when_present() {
        let env = make_tool_hook_env("Read", &serde_json::Value::Null, Some("sess-42"));
        assert_eq!(
            env.get("ZEPH_SESSION_ID").map(String::as_str),
            Some("sess-42")
        );
    }

    #[test]
    fn make_tool_hook_env_omits_session_id_when_none() {
        let env = make_tool_hook_env("Read", &serde_json::Value::Null, None);
        assert!(!env.contains_key("ZEPH_SESSION_ID"));
    }
}
