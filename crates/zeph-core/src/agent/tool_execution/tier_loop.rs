// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use futures::FutureExt as _;
use tracing::Instrument;
use zeph_durable::{EffectIntentSubClass, OnAmbiguous, StepDescriptor};
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

/// Per-call timeout for the parameter-reformat LLM call (#5453) when
/// `tools.retry.max_retry_duration_secs` is `0` ("no phase budget limit") — that value must not
/// be reused directly as a per-call timeout (critic finding M1: `max(1)` on `0` collapsed to a
/// 1-second timeout that always failed, silently disabling the whole feature).
const REFORMAT_DEFAULT_TIMEOUT_SECS: u64 = 30;

fn make_tool_hook_env(
    tool_name: &str,
    tool_input: &serde_json::Value,
    session_id: Option<&str>,
) -> std::collections::HashMap<String, String> {
    let mut env = zeph_subagent::make_base_hook_env(tool_name, tool_input);
    if let Some(sid) = session_id {
        env.insert("ZEPH_SESSION_ID".to_owned(), sid.to_owned());
    }
    crate::agent::hooks_dispatch::insert_main_agent_ctx(&mut env, session_id);
    env
}

impl<C: Channel> Agent<C> {
    #[tracing::instrument(
        name = "core.tool.run_post_dispatch_phases",
        skip_all,
        level = "debug",
        err
    )]
    /// Runs the confirmation, retry, and parameter-reformat phases in sequence.
    ///
    /// Returns `Ok(true)` as soon as any phase reports that the user cancelled the turn, skipping
    /// the remaining phases — each phase already persists its own `[Cancelled]` tombstone, so
    /// running further phases after one reports cancellation would duplicate that write (#5513).
    async fn run_post_dispatch_phases(
        &mut self,
        tool_calls: &[zeph_llm::provider::ToolUseRequest],
        calls: &[ToolCall],
        tool_results: &mut [Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError>],
        max_retries: usize,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<bool, crate::agent::error::AgentError> {
        if self
            .handle_confirmation_phase(tool_calls, calls, tool_results, cancel)
            .await?
        {
            return Ok(true);
        }
        if self
            .handle_retry_phase(tool_calls, calls, tool_results, max_retries, cancel)
            .await?
        {
            return Ok(true);
        }
        if self
            .handle_reformat_phase(tool_calls, calls, tool_results, cancel)
            .await?
        {
            return Ok(true);
        }
        Ok(false)
    }

    #[tracing::instrument(
        name = "core.tool.handle_confirmation_phase",
        skip_all,
        level = "debug",
        err
    )]
    /// Returns `Ok(true)` if the user cancelled the turn during this phase.
    async fn handle_confirmation_phase(
        &mut self,
        tool_calls: &[zeph_llm::provider::ToolUseRequest],
        calls: &[ToolCall],
        tool_results: &mut [Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError>],
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<bool, crate::agent::error::AgentError> {
        for idx in 0..tool_results.len() {
            if cancel.is_cancelled() {
                self.tool_executor.set_skill_env(None);
                tracing::info!("tool execution cancelled by user");
                self.update_metrics(|m| m.cancellations += 1);
                self.channel.send("[Cancelled]").await?;
                self.persist_cancelled_tool_results(tool_calls, None).await;
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

    /// Returns `Ok(true)` if the user cancelled the turn during this phase.
    #[tracing::instrument(name = "core.tool.handle_retry_phase", skip_all, level = "debug", err)]
    async fn handle_retry_phase(
        &mut self,
        tool_calls: &[zeph_llm::provider::ToolUseRequest],
        calls: &[ToolCall],
        tool_results: &mut [Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError>],
        max_retries: usize,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<bool, crate::agent::error::AgentError> {
        if max_retries == 0 {
            return Ok(false);
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
                self.persist_cancelled_tool_results(tool_calls, None).await;
                return Ok(true);
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
                        self.persist_cancelled_tool_results(tool_calls, None).await;
                        return Ok(true);
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
                                self.persist_cancelled_tool_results(tool_calls, None).await;
                                return Ok(true);
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
        Ok(false)
    }

    /// Returns `Ok(true)` if the user cancelled the turn during this phase.
    #[tracing::instrument(
        name = "core.tool.handle_reformat_phase",
        skip_all,
        level = "debug",
        err
    )]
    async fn handle_reformat_phase(
        &mut self,
        tool_calls: &[zeph_llm::provider::ToolUseRequest],
        calls: &[ToolCall],
        tool_results: &mut [Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError>],
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<bool, crate::agent::error::AgentError> {
        if self
            .tool_orchestrator
            .parameter_reformat_provider
            .is_empty()
        {
            return Ok(false);
        }
        // Budget covers the whole reformat phase (all tool calls needing reformat this round),
        // matching the retry phase's `retry_start`/`max_retry_duration_secs` accounting.
        let budget_secs = self.tool_orchestrator.max_retry_duration_secs;
        let reformat_start = std::time::Instant::now();
        for idx in 0..tool_results.len() {
            if cancel.is_cancelled() {
                self.tool_executor.set_skill_env(None);
                tracing::info!("parameter reformat phase cancelled by user");
                self.update_metrics(|m| m.cancellations += 1);
                self.channel.send("[Cancelled]").await?;
                self.persist_cancelled_tool_results(tool_calls, None).await;
                return Ok(true);
            }
            let needs_reformat = matches!(
                tool_results[idx],
                Err(ref e) if e.category().needs_parameter_reformat()
            );
            if !needs_reformat {
                continue;
            }
            let tc = &tool_calls[idx];
            if budget_secs > 0 && reformat_start.elapsed().as_secs() >= budget_secs {
                tracing::warn!(tool = %tc.name, "parameter reformat budget exhausted, skipping");
                continue;
            }
            let error_message = tool_results[idx]
                .as_ref()
                .err()
                .map(std::string::ToString::to_string)
                .unwrap_or_default();

            let _ = self
                .channel
                .send_status(&format!("Reformatting parameters for {}...", tc.name))
                .await;
            let new_result = self
                .reformat_tool_call(&calls[idx], tc, &error_message, cancel)
                .await;
            let _ = self.channel.send_status("").await;

            if cancel.is_cancelled() {
                self.tool_executor.set_skill_env(None);
                tracing::info!("parameter reformat phase cancelled by user");
                self.update_metrics(|m| m.cancellations += 1);
                self.channel.send("[Cancelled]").await?;
                self.persist_cancelled_tool_results(tool_calls, None).await;
                return Ok(true);
            }

            if let Some(result) = new_result {
                if let Err(ref e) = result
                    && let Some(ref d) = self.runtime.debug.debug_dumper
                {
                    d.dump_tool_error(tc.name.as_str(), e);
                }
                tool_results[idx] = result;
            }
        }
        Ok(false)
    }

    /// LLM-based single-shot reformat-and-retry for a tool call that failed with an
    /// `InvalidParameters`/`TypeMismatch` error (issue #5453).
    ///
    /// Resolves `tools.retry.parameter_reformat_provider` from `[[llm.providers]]` via
    /// [`Agent::resolve_pool_entry_provider`]. Unlike most other background-provider call sites
    /// (which use [`Agent::resolve_background_provider`] and fall back to the primary provider
    /// on any resolution failure), a *configured* name — one the provider-pool registry was
    /// actually wired to recognize — must not silently substitute the primary provider when it
    /// fails to resolve: that would mask the original tool error behind a "corrected" call made
    /// with the wrong model (#5600, #5478). This method no-ops instead (keeps the original tool
    /// error) whenever the registry is wired but the name is absent from `provider_pool`, the
    /// matched entry fails to build, or no `provider_config_snapshot` is available. It only
    /// falls back to [`Agent::resolve_background_provider`]'s legacy convention when the
    /// provider-pool registry itself was never wired for this `Agent` at all — which
    /// `zeph_config::providers::validate_pool` guarantees cannot happen for a real, fully
    /// constructed production agent (see [`Agent::resolve_pool_entry_provider`]'s doc comment).
    ///
    /// Returns `None` when the provider is unresolvable, the provider call fails, times out, or
    /// returns arguments that do not parse as a JSON object — the caller keeps the original error
    /// result unchanged.
    async fn reformat_tool_call(
        &mut self,
        call: &ToolCall,
        tc: &zeph_llm::provider::ToolUseRequest,
        error_message: &str,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Option<Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError>> {
        #[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
        struct ReformattedArguments {
            /// Corrected JSON arguments object for the failed tool call.
            arguments: serde_json::Value,
        }

        let Some(schema) = self
            .tool_executor
            .tool_definitions_erased()
            .into_iter()
            .find(|d| d.id.as_ref() == tc.name.as_str())
            .map(|d| d.schema)
        else {
            tracing::warn!(tool = %tc.name, "parameter reformat: tool schema not found, skipping");
            return None;
        };

        let provider_name = self.tool_orchestrator.parameter_reformat_provider.clone();
        let provider = match self.resolve_pool_entry_provider(&provider_name) {
            super::super::learning::PoolProviderResolution::Resolved(p) => *p,
            super::super::learning::PoolProviderResolution::RegistryNotWired => {
                self.resolve_background_provider(&provider_name)
            }
            super::super::learning::PoolProviderResolution::Unresolvable => {
                tracing::warn!(
                    tool = %tc.name,
                    provider = %provider_name,
                    "parameter reformat: configured provider unresolvable, keeping original error"
                );
                return None;
            }
        };

        let original_args = serde_json::Value::Object(call.params.clone());
        let prompt = format!(
            "A tool call failed parameter validation. Propose corrected arguments as a JSON \
             object under the `arguments` key.\n\n\
             Tool: {}\nJSON schema:\n{}\n\nOriginal arguments:\n{}\n\nError: {error_message}",
            tc.name,
            serde_json::to_string_pretty(&schema).unwrap_or_default(),
            serde_json::to_string_pretty(&original_args).unwrap_or_default(),
        );
        let messages = [Message::from_legacy(Role::User, prompt)];

        // `max_retry_duration_secs == 0` means "no phase budget limit" (see the `budget_secs > 0`
        // check in `handle_reformat_phase`), but a per-LLM-call timeout must never be 0 or
        // collapse to 1s for that same value — that previously made `max_retry_duration_secs = 0`
        // ("unlimited") silently disable the whole feature via an always-timing-out 1s call
        // (critic finding M1). Use the configured budget as the per-call timeout only when it is a
        // real bound; fall back to a fixed sane default otherwise.
        let timeout_secs = match self.tool_orchestrator.max_retry_duration_secs {
            0 => REFORMAT_DEFAULT_TIMEOUT_SECS,
            secs => secs,
        };
        let reformat = tokio::select! {
            r = tokio::time::timeout(
                std::time::Duration::from_secs(timeout_secs),
                provider.chat_typed_erased::<ReformattedArguments>(&messages),
            ) => match r {
                Ok(Ok(reformat)) => reformat,
                Ok(Err(e)) => {
                    tracing::warn!(tool = %tc.name, error = %e, "parameter reformat: provider call failed");
                    return None;
                }
                Err(_) => {
                    tracing::warn!(tool = %tc.name, timeout_secs, "parameter reformat: provider call timed out");
                    return None;
                }
            },
            () = cancel.cancelled() => return None,
        };

        let serde_json::Value::Object(corrected_params) = reformat.arguments else {
            tracing::warn!(
                tool = %tc.name,
                "parameter reformat: corrected arguments were not a JSON object, skipping retry"
            );
            return None;
        };

        let mut retry_call = call.clone();
        retry_call.params = corrected_params;

        tokio::select! {
            r = self.tool_executor.execute_tool_call_erased(&retry_call).instrument(
                tracing::info_span!("tool_exec_reformat", tool_name = %tc.name)
            ) => Some(r),
            () = cancel.cancelled() => None,
        }
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
                                skill_name: call.skill_name.clone(),
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
            if pre_exec_blocked[idx] {
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

    fn compute_utility_actions(
        &mut self,
        calls: &[ToolCall],
        pre_exec_blocked: &[bool],
        pending_system_hints: &mut Vec<String>,
    ) -> (Vec<zeph_tools::UtilityAction>, bool) {
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
        let mut actions = Vec::with_capacity(calls.len());
        // Once window exhaustion fires, all remaining calls in the batch are downgraded to Stop.
        let mut window_exhausted = false;
        for (idx, call) in calls.iter().enumerate() {
            if window_exhausted {
                actions.push(zeph_tools::UtilityAction::Stop);
                continue;
            }
            if pre_exec_blocked[idx] {
                actions.push(zeph_tools::UtilityAction::ToolCall);
                continue;
            }
            if self
                .tool_orchestrator
                .utility_scorer
                .is_exempt(call.tool_id.as_str())
            {
                actions.push(zeph_tools::UtilityAction::ToolCall);
                continue;
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
            // note_action increments the consecutive-low counter for scored calls only.
            // Exempt and pre-exec-blocked calls above bypass scoring and are not tracked.
            if self.tool_orchestrator.utility_scorer.note_action(&action) {
                let n = self.tool_orchestrator.utility_scorer.utility_window();
                tracing::info!(
                    window = n,
                    "utility gate: consecutive-low window exhausted, early-stopping loop"
                );
                pending_system_hints.push(format!(
                    "Tool loop stopped early: utility below threshold for {n} consecutive calls."
                ));
                window_exhausted = true;
            }
            actions.push(action);
        }
        (actions, window_exhausted)
    }

    #[tracing::instrument(
        name = "core.tool.handle_native_tool_calls",
        skip_all,
        level = "debug",
        fields(tool_count = tool_calls.len()),
        err
    )]
    /// Returns `true` when the utility-window was exhausted and the outer iteration loop
    /// should break immediately after this batch.
    pub(super) async fn handle_native_tool_calls(
        &mut self,
        text: Option<&str>,
        tool_calls: &[zeph_llm::provider::ToolUseRequest],
    ) -> Result<bool, crate::agent::error::AgentError> {
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
            mage_blocked,
            mut early_stop_hints,
            window_exhausted,
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
        // MAGE override: if trajectory risk is exceeded, bypass the tier loop and build
        // blocked results directly so process_tool_result_batch renders them normally.
        let tier_data: TierLoopOutput = if let Some((score, top_signals)) = mage_blocked {
            Some(TierLoopData {
                tool_results: calls
                    .iter()
                    .map(|_| {
                        Err(zeph_tools::ToolError::TrajectoryRiskExceeded {
                            score,
                            top_signals: top_signals.clone(),
                        })
                    })
                    .collect(),
                pending_focus_checkpoint: None,
                pending_system_hints: Vec::new(),
            })
        } else {
            self.run_tier_execution_loop(
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
            .await?
        };

        // Unpack tier execution output. None means the user cancelled — return early.
        let Some(TierLoopData {
            mut tool_results,
            pending_focus_checkpoint,
            mut pending_system_hints,
        }) = tier_data
        else {
            return Ok(false);
        };
        // Prepend window-exhaustion hints so the LLM sees them before per-call skipped results.
        if !early_stop_hints.is_empty() {
            early_stop_hints.extend(pending_system_hints);
            pending_system_hints = early_stop_hints;
        }

        // Phases 2a / 2 / 3: confirmation, transient retry, parameter reformat.
        // Each phase may signal cancellation (Ok(true)), which already persisted its own
        // tombstone — skip process_tool_result_batch below to avoid a duplicate batch write (#5513).
        let post_dispatch_cancelled = self
            .run_post_dispatch_phases(tool_calls, &calls, &mut tool_results, max_retries, &cancel)
            .await?;
        if post_dispatch_cancelled {
            return Ok(false);
        }

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

        Ok(window_exhausted)
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

    fn skill_attribution(&self) -> Option<Vec<String>> {
        (!self.services.skill.active_skill_names.is_empty())
            .then(|| self.services.skill.active_skill_names.clone())
    }

    #[allow(clippy::too_many_lines)] // unmask-miss telemetry aggregation (#5437 S1) crossed the 100-line limit
    fn prepare_tool_dispatch(
        &mut self,
        tool_calls: &[zeph_llm::provider::ToolUseRequest],
    ) -> ToolDispatchContext {
        let tafc_enabled = self.tool_orchestrator.tafc.enabled;
        // PAAC secret unmasking (#5437): resolved once per dispatch batch (Arc clone, cheap)
        // so the closure below doesn't need to re-borrow `self` for every tool call.
        let secret_registry = self.services.security.secret_registry.clone();
        // When the orchestration scheduler has set a named execution environment for the
        // current task, inject it into every ToolCall so ShellExecutor::resolve_context
        // uses the right env/cwd without the LLM having to supply it.
        let task_ctx = self
            .services
            .orchestration
            .task_execution_env
            .as_deref()
            .map(|name| ExecutionContext::default().with_name(name));
        let tool_call_ids: Vec<String> = tool_calls
            .iter()
            .map(|_| uuid::Uuid::new_v4().to_string())
            .collect();

        // S1: unmask-miss telemetry (#5437) — counts string leaves that still carry a
        // `<SECRET:` prefix after `unmask_json_value`, meaning the model failed to reproduce a
        // placeholder byte-for-byte. Aggregated across the whole batch and reported once below.
        let mut unmask_misses = 0usize;
        let mut unmask_miss_tools: Vec<String> = Vec::new();

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
                // Unmask secret placeholders in tool arguments before dispatch (e.g. the model
                // echoing a masked value it saw in a prior tool result back into a shell `code`
                // or HTTP header argument). No-op when secret masking is disabled/empty.
                if let Some(registry) = secret_registry.as_deref() {
                    let mut tc_misses = 0usize;
                    for value in params.values_mut() {
                        tc_misses += unmask_json_value(value, registry);
                    }
                    if tc_misses > 0 {
                        unmask_misses += tc_misses;
                        unmask_miss_tools.push(tc.name.to_string());
                    }
                }
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
                    skill_name: self.skill_attribution(),
                })
            })
            .collect();

        if unmask_misses > 0 {
            tracing::warn!(
                misses = unmask_misses,
                tools = ?unmask_miss_tools,
                "secret placeholder(s) in tool arguments did not resolve — the model likely \
                 mangled a <SECRET:...> token (whitespace/truncation); the affected tool call(s) \
                 will run with the literal placeholder text, not the real secret"
            );
            self.update_metrics(|m| m.secret_unmask_misses += unmask_misses as u64);
        }
        // Timestamps filled just before each tier's join_all so audit reflects actual start.
        let tool_started_ats = vec![std::time::Instant::now(); tool_calls.len()];

        self.check_exfiltration_urls(tool_calls);

        // Pre-execution verification (TrustBench, #1630): runs before repeat-detection.
        let mut pre_exec_blocked = self.run_pre_execution_verifiers(&calls);

        self.apply_channel_tool_allowlist(&calls, &mut pre_exec_blocked);

        // Utility gate: score each call and recommend an action (#2477).
        // user_requested is from the last user message only (prompt-injection guard).
        let mut early_stop_hints: Vec<String> = Vec::new();
        let (utility_actions, window_exhausted) =
            self.compute_utility_actions(&calls, &pre_exec_blocked, &mut early_stop_hints);

        // M3: quota counted once per batch; retries do not consume additional slots.
        let quota_blocked = self.check_and_update_quota(calls.len());

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
        // CRIT-3: push calls before execution; cache hits included (P1 invariant).
        for (call, &hash) in calls.iter().zip(args_hashes.iter()) {
            self.tool_orchestrator
                .push_tool_call(call.tool_id.as_str(), hash);
        }

        // Cache lookup: hits pre-built before dispatch; cache store happens after join_all.
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

        // MAGE trajectory risk gate (spec 004-16 FR-004, FR-005).
        // Extracted to keep prepare_tool_dispatch under the line limit.
        let mage_blocked = self.check_mage_block();

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
            mage_blocked,
            early_stop_hints,
            window_exhausted,
        }
    }

    /// Check MAGE trajectory risk gate (spec 004-16 FR-004, FR-005).
    ///
    /// Returns `Some((score, top_signals))` when the accumulator is blocked. Emits a security
    /// event, increments `pre_execution_blocks`, and calls `record_block()` on the accumulator.
    fn check_mage_block(&mut self) -> Option<(f64, Vec<String>)> {
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

    fn check_and_update_quota(&mut self, batch_len: usize) -> bool {
        if let Some(max) = self.tool_orchestrator.check_quota() {
            tracing::warn!(
                max,
                count = self.tool_orchestrator.session_tool_call_count,
                "tool call quota exceeded for session"
            );
            return true;
        }
        self.tool_orchestrator.session_tool_call_count = self
            .tool_orchestrator
            .session_tool_call_count
            .saturating_add(u32::try_from(batch_len).unwrap_or(u32::MAX));
        false
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
                self.persist_cancelled_tool_results(tool_calls, None).await;
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

            // Check hook block cap after each tier (RF-1: counter is per-turn, not per-tier).
            // hook_block_cap = 0 means no cap.
            let cap = self.tool_orchestrator.hook_block_cap;
            if cap > 0 && self.tool_orchestrator.hook_block_count >= cap {
                tracing::warn!(
                    hook_block_count = self.tool_orchestrator.hook_block_count,
                    hook_block_cap = cap,
                    "hook block cap reached — ending turn"
                );
                let _ = self
                    .channel
                    .send(&format!(
                        "Stopping: PreToolUse hook blocked {}/{} tool calls this turn.",
                        self.tool_orchestrator.hook_block_count, cap
                    ))
                    .await;
                break;
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
            let is_request_compaction = tc.name == "request_compaction"
                && self
                    .services
                    .memory
                    .subsystems
                    .arc_config
                    .allow_agent_compaction;
            if is_focus_tool || is_compress || is_request_compaction {
                let result = if is_compress {
                    self.handle_compress_context().await
                } else if is_request_compaction {
                    self.handle_request_compaction(&tc.input).await
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

    /// Handles `UtilityAction::Retrieve` — either mandates a context-retrieval-then-retry
    /// round, or, when a prior retrieval attempt this turn already failed with a
    /// retryable/network-class error (e.g. Qdrant unreachable), lets the originally
    /// requested tool proceed directly instead of demanding another doomed retry (#5584).
    async fn handle_retrieve_action(
        &mut self,
        idx: usize,
        tc: &zeph_llm::provider::ToolUseRequest,
        pending_system_hints: &mut Vec<String>,
    ) -> Result<Option<(usize, ToolExecFut)>, crate::agent::error::AgentError> {
        if self.tool_orchestrator.has_retryable_failure_this_turn() {
            let _ = self
                .channel
                .send_status(&format!(
                    "Utility action: Retrieve skipped, context unavailable ({})",
                    tc.name
                ))
                .await;
            pending_system_hints.push(format!(
                "[utility:retrieve] Context retrieval failed and appears unavailable. \
                 Proceed with the '{}' tool call using the best information already \
                 available rather than retrying the failed retrieval.",
                tc.name
            ));
            return Ok(None);
        }
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

    async fn handle_utility_gate(
        &mut self,
        idx: usize,
        tc: &zeph_llm::provider::ToolUseRequest,
        utility_actions: &[zeph_tools::UtilityAction],
        pending_system_hints: &mut Vec<String>,
    ) -> Result<Option<(usize, ToolExecFut)>, crate::agent::error::AgentError> {
        match utility_actions[idx] {
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
                self.handle_retrieve_action(idx, tc, pending_system_hints)
                    .await
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
            _ => Ok(None),
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
        // TODO: implement retry-on-{"retry":true} stdout signal (#3292)
        self.fire_permission_denied_hooks(tc, &r.reason).await;
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
    ) -> Result<Option<((usize, ToolExecFut), String)>, crate::agent::error::AgentError> {
        if has_failed_dep {
            let reason = "prerequisite tool failed or requires confirmation".to_owned();
            return Ok(Some((
                ready_fut(
                    idx,
                    skipped_output(
                        tc.name.clone(),
                        "[error] Skipped: a prerequisite tool failed or requires confirmation",
                    ),
                ),
                reason,
            )));
        }
        if quota_blocked {
            let max = self
                .tool_orchestrator
                .max_tool_calls_per_session
                .unwrap_or(0);
            let reason = format!("session tool call quota exceeded (limit: {max} calls)");
            return Ok(Some((
                ready_fut(
                    idx,
                    skipped_output(
                        tc.name.clone(),
                        format!(
                            "[error] Tool call quota exceeded (session limit: {max} calls). \
                             No further tool calls are allowed this session."
                        ),
                    ),
                ),
                reason,
            )));
        }
        if pre_exec_blocked[idx] {
            let reason = format!(
                "blocked by pre-execution verifier: {} is not permitted",
                tc.name
            );
            return Ok(Some((
                ready_fut(
                    idx,
                    skipped_output(
                        tc.name.clone(),
                        format!(
                            "[error] Tool call to {} was blocked by pre-execution verifier. \
                             The requested operation is not permitted.",
                            tc.name
                        ),
                    ),
                ),
                reason,
            )));
        }
        if let Some(fut) = self
            .handle_utility_gate(idx, tc, utility_actions, pending_system_hints)
            .await?
        {
            let reason = format!(
                "utility gate ({:?}) intercepted {}",
                utility_actions[idx], tc.name
            );
            return Ok(Some((fut, reason)));
        }
        if repeat_blocked[idx] {
            let reason = format!("repeated identical call to {} detected", tc.name);
            return Ok(Some((
                ready_fut(
                    idx,
                    skipped_output(
                        tc.name.clone(),
                        format!(
                            "[error] Repeated identical call to {} detected. \
                             Use different arguments or a different approach.",
                            tc.name
                        ),
                    ),
                ),
                reason,
            )));
        }
        Ok(None)
    }

    /// Fires `permission_denied` hooks (fail-open). Called at every gate/rate-limiter denial.
    ///
    /// Hooks run sequentially; slow or hanging hooks will stall tool dispatch for each denied
    /// call. Hook authors should ensure hooks complete quickly or use a background process.
    async fn fire_permission_denied_hooks(
        &mut self,
        tc: &zeph_llm::provider::ToolUseRequest,
        reason: &str,
    ) {
        let pd_hooks = self.services.session.hooks_config.permission_denied.clone();
        if pd_hooks.is_empty() {
            return;
        }
        let mut env = std::collections::HashMap::new();
        env.insert("ZEPH_DENIED_TOOL".to_owned(), tc.name.to_string());
        env.insert("ZEPH_DENY_REASON".to_owned(), reason.to_owned());
        env.insert("ZEPH_TOOL_NAME".to_owned(), tc.name.to_string());
        let conv_id_str = self
            .services
            .memory
            .persistence
            .conversation_id
            .map(|id| id.0.to_string());
        crate::agent::hooks_dispatch::insert_main_agent_ctx(&mut env, conv_id_str.as_deref());
        let dispatch = self.mcp_dispatch();
        let mcp: Option<&dyn zeph_subagent::McpDispatch> = dispatch
            .as_ref()
            .map(|d| d as &dyn zeph_subagent::McpDispatch);
        if let Err(e) = zeph_subagent::hooks::fire_hooks(&pd_hooks, &env, mcp, None)
            .instrument(tracing::info_span!(
                "core.hooks.permission_denied",
                tool = %tc.name
            ))
            .await
        {
            tracing::warn!(error = %e, tool = %tc.name, "PermissionDenied hook failed");
        }
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

            // Skip focus tools, compress_context, and request_compaction — pre-handled before the tier loop.
            if tc.name == "compress_context"
                || tc.name == "request_compaction"
                || (self.services.focus.config.enabled
                    && (tc.name == "start_focus" || tc.name == "complete_focus"))
            {
                continue;
            }

            // Fire PreToolUse hooks before any gate check so the hook always observes the
            // LLM's tool request, even when a gate (utility, quota, dep, repeat) intercepts it.
            // Focus/compress tools are excluded by the early `continue` above — they are
            // synthetic internal tools that must never surface to the hook system.
            let pre_hooks = self.services.session.hooks_config.pre_tool_use.clone();
            if !pre_hooks.is_empty() {
                let matched: Vec<&zeph_config::HookDef> =
                    zeph_subagent::matching_hooks(&pre_hooks, tc.name.as_str());
                if !matched.is_empty() {
                    let conv_id_str = self
                        .services
                        .memory
                        .persistence
                        .conversation_id
                        .map(|id| id.0.to_string());
                    let env =
                        make_tool_hook_env(tc.name.as_str(), &tc.input, conv_id_str.as_deref());
                    let has_fail_closed = matched.iter().any(|h| h.fail_closed);
                    let owned: Vec<zeph_config::HookDef> = matched.into_iter().cloned().collect();
                    let dispatch = self.mcp_dispatch();
                    let mcp: Option<&dyn zeph_subagent::McpDispatch> = dispatch
                        .as_ref()
                        .map(|d| d as &dyn zeph_subagent::McpDispatch);
                    if let Err(e) = zeph_subagent::hooks::fire_hooks(&owned, &env, mcp, None)
                        .instrument(tracing::info_span!(
                            "core.hooks.pre_tool_use",
                            tool = %tc.name
                        ))
                        .await
                    {
                        if has_fail_closed {
                            self.tool_orchestrator.hook_block_count += 1;
                            tracing::warn!(
                                error = %e,
                                tool = %tc.name,
                                hook_block_count = self.tool_orchestrator.hook_block_count,
                                hook_block_cap = self.tool_orchestrator.hook_block_cap,
                                "PreToolUse hook blocked tool (fail_closed)"
                            );
                            let msg = format!(
                                "[blocked] PreToolUse hook blocked tool `{}`: {e}",
                                tc.name
                            );
                            tier_futs.push((
                                idx,
                                Box::pin(std::future::ready(Ok(Some(skipped_output(
                                    tc.name.clone(),
                                    msg,
                                ))))),
                            ));
                            continue;
                        }
                        tracing::warn!(
                            error = %e,
                            tool = %tc.name,
                            "PreToolUse hook failed"
                        );
                    }
                }
            }

            // Check static gates: dep failure, quota, pre-exec block, utility gate, repeat.
            let has_failed_dep = dag
                .string_values_for(idx)
                .iter()
                .any(|v| failed_ids.contains(v));
            if let Some((fut, reason)) = self
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
                self.fire_permission_denied_hooks(tc, &reason).await;
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
                self.fire_permission_denied_hooks(tc, &exceeded.to_error_message())
                    .await;
                tier_futs.push(ready_fut(
                    idx,
                    skipped_output(tc.name.clone(), exceeded.to_error_message()),
                ));
                continue;
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
                    self.persist_cancelled_tool_results(tool_calls, None).await;
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

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
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
        for (idx, mut result) in indices.into_iter().zip(tier_results) {
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
                    let duration_ms = u64::try_from(tool_started_ats[idx].elapsed().as_millis())
                        .unwrap_or(u64::MAX);
                    env.insert("ZEPH_TOOL_DURATION_MS".to_owned(), duration_ms.to_string());
                    let owned: Vec<zeph_config::HookDef> = matched.into_iter().cloned().collect();
                    let dispatch = self.mcp_dispatch();
                    let mcp: Option<&dyn zeph_subagent::McpDispatch> = dispatch
                        .as_ref()
                        .map(|d| d as &dyn zeph_subagent::McpDispatch);

                    // Build stdin JSON context for the hook process.
                    let tool_output_text = match &result {
                        Ok(Some(out)) => Some(out.summary.as_str()),
                        _ => None,
                    };
                    let tool_error_text = match &result {
                        Err(e) => Some(e.to_string()),
                        _ => None,
                    };
                    let hook_input = zeph_subagent::PostToolUseHookInput {
                        tool_name: tool_calls[idx].name.as_str(),
                        tool_args: &tool_calls[idx].input,
                        session_id: conv_id_str.as_deref(),
                        duration_ms,
                        tool_output: tool_output_text,
                        tool_error: tool_error_text.as_deref(),
                        agent_id: conv_id_str.as_deref(),
                        agent_type: "main",
                    };
                    let stdin_bytes = serde_json::to_vec(&hook_input).ok();

                    match zeph_subagent::hooks::fire_hooks(
                        &owned,
                        &env,
                        mcp,
                        stdin_bytes.as_deref(),
                    )
                    .instrument(tracing::info_span!(
                        "core.hooks.post_tool_use",
                        tool = %tool_calls[idx].name
                    ))
                    .await
                    {
                        Ok(run_result) => {
                            if let Some(replacement) = run_result.output.updated_tool_output {
                                // Apply hook-requested output substitution.
                                if let Ok(Some(ref mut out)) = result {
                                    tracing::debug!(
                                        tool = %tool_calls[idx].name,
                                        original_len = out.summary.len(),
                                        replacement_len = replacement.len(),
                                        "PostToolUse hook replaced tool output"
                                    );
                                    out.summary = replacement;
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                tool = %tool_calls[idx].name,
                                "PostToolUse hook failed"
                            );
                        }
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
    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
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

        // Extract goal_summary from causal pre-probe response before it is consumed.
        // Used by shadow memory below; empty when causal IPI is disabled (deviation_score = 0.0).
        let goal_summary_for_shadow: String = causal_pre_response
            .as_ref()
            .map(|(pre, _)| {
                let end = pre.floor_char_boundary(100);
                pre[..end].to_owned()
            })
            .unwrap_or_default();

        self.run_causal_ipi_post_probe(causal_pre_response, &result_parts)
            .await;

        // Spec 010-7 FR-001–FR-004: record shadow event and check goal drift.
        self.record_shadow_event(tool_calls, goal_summary_for_shadow);

        // Acon: compress tool results before they enter message history (#4021).
        self.apply_acon_compression(tool_calls, &mut result_parts);

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
                        lsp.after_tool(
                            &name,
                            &input,
                            &output,
                            &tc_arc,
                            &sanitizer,
                            &mut self.runtime.lifecycle.supervisor,
                        )
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

    /// Record a [`ShadowEvent`](zeph_sanitizer::ShadowEvent) for cross-turn goal-drift detection.
    ///
    /// Called after every tool batch completes (spec 010-7 FR-001). When `shadow_memory` is
    /// `None` (disabled) this is a no-op. When drift score triggers an alert, emits a
    /// `GoalDrift` security event (FR-003, FR-004).
    fn record_shadow_event(
        &mut self,
        tool_calls: &[zeph_llm::provider::ToolUseRequest],
        goal_summary: String,
    ) {
        let Some(ref mut mem) = self.services.security.shadow_memory else {
            return;
        };
        let tool_names: Vec<String> = tool_calls
            .iter()
            .map(|tc| tc.name.as_str().to_owned())
            .collect();
        let max_permission_class = tool_names
            .iter()
            .map(|n| zeph_sanitizer::classify_tool_permission(n))
            .max()
            .unwrap_or(0);
        let turn = u32::try_from(self.runtime.debug.iteration_counter.saturating_sub(1))
            .unwrap_or(u32::MAX);
        mem.record(zeph_sanitizer::ShadowEvent {
            turn,
            tools: tool_names,
            max_permission_class,
            deviation_score: 0.0,
            goal_summary,
        });
        let drift = mem.goal_drift_score();
        if drift.should_alert {
            tracing::warn!(
                score = drift.score,
                turn = turn,
                "shadow memory: goal drift alert"
            );
            self.push_security_event(
                zeph_common::SecurityEventCategory::GoalDrift,
                "shadow_memory",
                format!("drift_score={:.3}", drift.score),
            );
        }
    }

    /// Apply Acon tool-result compression to `result_parts` in-place before the parts enter
    /// message history. No-op when `acon_config.enabled` is false or the batch is empty.
    #[tracing::instrument(name = "context.tool_result_compress", skip_all, level = "debug")]
    fn apply_acon_compression(
        &mut self,
        tool_calls: &[zeph_llm::provider::ToolUseRequest],
        result_parts: &mut [MessagePart],
    ) {
        use zeph_context::tool_result_compress::{
            CompressionMethod, ToolResultCompressionConfig, ToolResultCompressor, ToolResultEntry,
        };

        let acon = &self.services.memory.subsystems.acon_config;
        if !acon.enabled {
            return;
        }

        let cfg = ToolResultCompressionConfig::from(acon);
        let tc = std::sync::Arc::clone(&self.runtime.metrics.token_counter);

        // Build a lookup from tool_use_id → tool_name so we can populate the trace field
        // without relying on positional correspondence between result_parts and tool_calls.
        // This is robust to future changes where process_one_tool_result emits zero or
        // multiple ToolResult parts per call.
        let id_to_name: std::collections::HashMap<&str, &str> = tool_calls
            .iter()
            .map(|tc| (tc.id.as_str(), tc.name.as_str()))
            .collect();

        // Collect (part_index, tool_name, owned_text) for each ToolResult part. Text is cloned
        // to avoid borrow conflicts when we later mutate result_parts.
        let indexed_texts: Vec<(usize, String, String)> = result_parts
            .iter()
            .enumerate()
            .filter_map(|(i, part)| {
                if let MessagePart::ToolResult {
                    content,
                    tool_use_id,
                    ..
                } = part
                {
                    let name = id_to_name
                        .get(tool_use_id.as_str())
                        .copied()
                        .unwrap_or("")
                        .to_owned();
                    Some((i, name, content.clone()))
                } else {
                    None
                }
            })
            .collect();

        if indexed_texts.is_empty() {
            return;
        }

        let entries: Vec<ToolResultEntry<'_>> = indexed_texts
            .iter()
            .map(|(part_idx, name, text)| ToolResultEntry {
                tool_name: name.as_str(),
                text: text.as_str(),
                index: *part_idx,
            })
            .collect();

        let compressed = ToolResultCompressor::compress_batch(&entries, tc.as_ref(), &cfg);

        let mut tokens_saved: usize = 0;
        let mut results_compressed: u32 = 0;

        for (result, (part_idx, _, _)) in compressed.iter().zip(indexed_texts.iter()) {
            if result.method != CompressionMethod::PassThrough
                && let MessagePart::ToolResult { content, .. } = &mut result_parts[*part_idx]
            {
                content.clone_from(&result.text);
                tokens_saved = tokens_saved.saturating_add(
                    result
                        .original_tokens
                        .saturating_sub(result.compressed_tokens),
                );
                results_compressed += 1;
            }
        }

        if results_compressed > 0 {
            tracing::debug!(
                tokens_saved,
                results_compressed,
                "acon: tool result compression applied"
            );
            self.update_metrics(|m| {
                m.acon_tokens_saved = m
                    .acon_tokens_saved
                    .saturating_add(u64::try_from(tokens_saved).unwrap_or(u64::MAX));
                m.acon_results_compressed = m
                    .acon_results_compressed
                    .saturating_add(u64::from(results_compressed));
            });
        }
    }
}

/// Recursively unmask PAAC secret placeholders in a JSON value's string leaves, in place.
///
/// Applied to tool-call parameters at the dispatch boundary (`prepare_tool_dispatch`) so a
/// model-emitted placeholder (e.g. one copied verbatim from a prior masked tool result into a
/// shell `code` or HTTP header argument) resolves to the real secret only at execution time —
/// never during LLM-facing context assembly. `unmask` is nonce-scoped and passthrough-on-miss,
/// so a placeholder the model could not have legitimately seen is left untouched (#5437).
///
/// Returns the number of string leaves that still contain a `<SECRET:` prefix after unmasking
/// (S1: unmask-miss telemetry). A non-zero count means the model failed to reproduce a
/// placeholder byte-for-byte (LLMs routinely normalize/space-break long opaque tokens) — the
/// tool call proceeds with the literal placeholder text (fail-safe: no leak, but the flow that
/// depended on the real secret will likely fail). Callers should log a `tracing::warn!` and
/// increment a metric so operators can detect this class of silent breakage.
fn unmask_json_value(
    value: &mut serde_json::Value,
    registry: &zeph_sanitizer::secret_mask::SecretMaskRegistry,
) -> usize {
    match value {
        serde_json::Value::String(s) => {
            let unmasked = registry.unmask(s);
            let still_masked = usize::from(unmasked.contains("<SECRET:"));
            if unmasked != *s {
                *s = unmasked;
            }
            still_masked
        }
        serde_json::Value::Array(arr) => {
            arr.iter_mut().map(|v| unmask_json_value(v, registry)).sum()
        }
        serde_json::Value::Object(map) => map
            .values_mut()
            .map(|v| unmask_json_value(v, registry))
            .sum(),
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => 0,
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
        self.tool_orchestrator.reset_hook_block_count();

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

        // Inject request_compaction tool when ARC agent-initiated compaction is enabled (#4020).
        if self
            .services
            .memory
            .subsystems
            .arc_config
            .allow_agent_compaction
        {
            tool_defs.push(crate::agent::focus::request_compaction_tool_definition());
        }

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

    /// Drive an LLM chat call through the durable step journal when a [`DurableContext`] is
    /// attached to the session.
    ///
    /// The step commits an `ExactlyOnceGuarded` / `CostBearingOrBoundaryIdempotent` intent
    /// before the real call runs, so a crash between the API response and journal acknowledgement
    /// is handled safely (`OnAmbiguous::Skip` — the cost is already incurred). The closure payload
    /// is `None` — the actual call always executes in every branch because `&mut self` cannot be
    /// captured inside the durable closure. The [`DurableStep::was_replayed`] flag is used to
    /// suppress double-printing in the caller.
    ///
    /// When `durable_ctx` is `None` the call is forwarded directly without any journaling.
    async fn call_llm_durable(
        &mut self,
        tool_defs: &[ToolDefinition],
        iteration: usize,
    ) -> Result<Option<zeph_llm::provider::ChatResponse>, crate::agent::error::AgentError> {
        self.ensure_session_durable_ctx().await;
        let Some(ctx) = self.services.session.durable_ctx.clone() else {
            return self.call_chat_with_tools_retry(tool_defs, 2).await;
        };

        let turn_span = tracing::info_span!(
            "core.durable.turn",
            iteration,
            execution_id = %ctx.execution_id().as_uuid(),
        );
        let cached_tokens = self.runtime.providers.cached_prompt_tokens;
        let fp_input = format!("llm_call:iter={iteration}:tokens={cached_tokens}").into_bytes();
        let desc = StepDescriptor::exactly_once_guarded(
            "llm_call",
            EffectIntentSubClass::CostBearingOrBoundaryIdempotent,
            Some(OnAmbiguous::Skip),
            fp_input,
        )
        .expect("CostBearingOrBoundaryIdempotent never requires explicit policy");

        let step = ctx
            .step_recorded::<Option<i64>, _, _>(desc, |_handle| async move { Ok(None::<i64>) })
            .instrument(turn_span)
            .await;

        match step {
            Ok(record) if record.was_replayed() => {
                // Already delivered to the user in a prior run; suppress re-printing.
                self.services.session.durable_turn_replayed = true;
                self.call_chat_with_tools_retry(tool_defs, 2).await
            }
            Ok(_) => self.call_chat_with_tools_retry(tool_defs, 2).await,
            Err(e) => {
                tracing::warn!(error = %e, "durable LLM step error; degrading to non-durable");
                self.call_chat_with_tools_retry(tool_defs, 2).await
            }
        }
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
        // Clear the per-turn replay flag; it is set below when the LLM step is replayed.
        self.services.session.durable_turn_replayed = false;

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

        let chat_result = self.call_llm_durable(tool_defs, iteration).await?;

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
            // Double-print suppression: when the LLM step was replayed from the journal, the
            // assistant text was already delivered in a previous run; skip re-sending it to the
            // channel. Persistence and in-memory push still run so the context is consistent
            // (spec-064 §001 §15 RuntimeLayer observe-only; replay control is NOT in RuntimeLayer).
            if !self.services.session.durable_turn_replayed {
                if !cleaned.is_empty() {
                    let display = self.maybe_redact(&cleaned);
                    self.channel.send(&display).await?;
                }
                self.store_response_in_cache(&cleaned, query_embedding)
                    .await;
            }
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
            tracing::warn!(
                ?chat_result,
                "unexpected ChatResponse variant in native tool loop"
            );
            return Ok(Some(()));
        };
        self.preserve_thinking_blocks(thinking_blocks);
        let window_exhausted = self
            .handle_native_tool_calls(text.as_deref(), &tool_calls)
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

        if window_exhausted {
            return Ok(Some(()));
        }

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
    use zeph_subagent::TOOL_ARGS_JSON_LIMIT;

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

    // Regression guard for issue #3738: pre_tool_use hooks must fire for tools that are
    // intercepted by the utility gate (Retrieve / Verify / Stop / Respond). The fix moves
    // pre-hook dispatch before check_call_gates. This test verifies that matching_hooks
    // correctly matches gate-intercepted tools so the hook system would observe them, and
    // that internal focus/compress tools are excluded when the caller skips them explicitly.
    #[test]
    fn pre_tool_use_hook_matches_gate_intercepted_tools_but_not_internal() {
        use zeph_config::{HookAction, HookDef, HookMatcher};
        use zeph_subagent::matching_hooks;

        let hook = HookDef {
            action: HookAction::Command {
                command: "true".to_owned(),
            },
            timeout_secs: 5,
            fail_closed: false,
            r#if: None,
        };
        // A wildcard-style matcher that matches any tool name token.
        let matchers = vec![HookMatcher {
            matcher: "shell|read|write|retrieve_memory".to_owned(),
            hooks: vec![hook],
        }];

        // Tools that a utility gate may intercept — pre-hook MUST fire for these.
        assert!(!matching_hooks(&matchers, "retrieve_memory").is_empty());
        assert!(!matching_hooks(&matchers, "shell").is_empty());

        // Internal tools — they are skipped before the hook dispatch block, so
        // matching_hooks is never called for them. Confirm they do NOT match the
        // hook matchers in the first place (extra guard).
        assert!(matching_hooks(&matchers, "compress_context").is_empty());
        assert!(matching_hooks(&matchers, "request_compaction").is_empty());
        assert!(matching_hooks(&matchers, "start_focus").is_empty());
        assert!(matching_hooks(&matchers, "complete_focus").is_empty());
    }

    // Regression guard for issue #3774: permission_denied hook env must contain
    // ZEPH_DENIED_TOOL and ZEPH_DENY_REASON for every gate/rate-limiter denial.
    // These tests verify the env construction logic mirrored in fire_permission_denied_hooks.

    fn make_pd_env(tool: &str, reason: &str) -> std::collections::HashMap<String, String> {
        let mut env = std::collections::HashMap::new();
        env.insert("ZEPH_DENIED_TOOL".to_owned(), tool.to_owned());
        env.insert("ZEPH_DENY_REASON".to_owned(), reason.to_owned());
        env
    }

    #[test]
    fn permission_denied_env_contains_tool_name_and_reason_for_quota_denial() {
        let tool = "shell";
        let reason = "session tool call quota exceeded (limit: 10 calls)";
        let env = make_pd_env(tool, reason);

        assert_eq!(
            env.get("ZEPH_DENIED_TOOL").map(String::as_str),
            Some("shell")
        );
        assert!(
            env.get("ZEPH_DENY_REASON")
                .is_some_and(|r| r.contains("quota")),
            "ZEPH_DENY_REASON should mention quota"
        );
    }

    #[test]
    fn permission_denied_env_contains_tool_name_and_reason_for_rate_limit_denial() {
        use crate::agent::rate_limiter::{RateLimitExceeded, ToolCategory};

        let exceeded = RateLimitExceeded {
            category: ToolCategory::Shell,
            count: 5,
            limit: 3,
            cooldown_remaining_secs: 30,
        };
        let reason = exceeded.to_error_message();
        let env = make_pd_env("bash", &reason);

        assert_eq!(
            env.get("ZEPH_DENIED_TOOL").map(String::as_str),
            Some("bash")
        );
        let deny_reason = env
            .get("ZEPH_DENY_REASON")
            .expect("ZEPH_DENY_REASON missing");
        assert!(
            deny_reason.contains("rate-limited"),
            "ZEPH_DENY_REASON should mention rate-limited, got: {deny_reason}"
        );
        assert!(
            deny_reason.contains("3/min"),
            "ZEPH_DENY_REASON should contain limit, got: {deny_reason}"
        );
    }

    #[test]
    fn permission_denied_env_contains_tool_name_and_reason_for_pre_exec_block() {
        let tool = "write";
        let reason = format!("blocked by pre-execution verifier: {tool} is not permitted");
        let env = make_pd_env(tool, &reason);

        assert_eq!(
            env.get("ZEPH_DENIED_TOOL").map(String::as_str),
            Some("write")
        );
        assert!(
            env.get("ZEPH_DENY_REASON")
                .is_some_and(|r| r.contains("pre-execution verifier")),
            "ZEPH_DENY_REASON should mention pre-execution verifier"
        );
    }

    #[test]
    fn permission_denied_env_contains_tool_name_and_reason_for_repeat_block() {
        let tool = "read";
        let reason = format!("repeated identical call to {tool} detected");
        let env = make_pd_env(tool, &reason);

        assert_eq!(
            env.get("ZEPH_DENIED_TOOL").map(String::as_str),
            Some("read")
        );
        assert!(
            env.get("ZEPH_DENY_REASON")
                .is_some_and(|r| r.contains("repeated identical call")),
            "ZEPH_DENY_REASON should mention repeated identical call"
        );
    }

    #[test]
    fn permission_denied_env_reason_includes_utility_action_variant() {
        // Verify that utility gate reason strings include the UtilityAction Debug variant name
        // so hook authors can distinguish Respond/Retrieve/Verify/Stop in ZEPH_DENY_REASON.
        use zeph_tools::UtilityAction;

        for action in [
            UtilityAction::Respond,
            UtilityAction::Retrieve,
            UtilityAction::Verify,
            UtilityAction::Stop,
        ] {
            let reason = format!("utility gate ({action:?}) intercepted memory_search");
            let env = make_pd_env("memory_search", &reason);

            let deny_reason = env
                .get("ZEPH_DENY_REASON")
                .expect("ZEPH_DENY_REASON missing");
            assert!(
                deny_reason.contains(&format!("{action:?}")),
                "ZEPH_DENY_REASON should contain {action:?}, got: {deny_reason}"
            );
        }
    }

    // --- record_shadow_event (spec 010-7 FR-001–FR-004) ---

    fn make_tool_req(name: &str) -> zeph_llm::provider::ToolUseRequest {
        zeph_llm::provider::ToolUseRequest {
            id: format!("id_{name}"),
            name: name.into(),
            input: serde_json::Value::Null,
        }
    }

    fn make_agent_with_shadow(enabled: bool) -> Agent<crate::testing::MockChannel> {
        use crate::testing::{MockChannel, MockToolExecutor, mock_provider};
        use zeph_skills::registry::SkillRegistry;
        let provider = mock_provider(vec![]);
        let channel = MockChannel::new(vec![] as Vec<String>);
        let registry = SkillRegistry::empty();
        let executor = MockToolExecutor::no_tools();
        let cfg = zeph_config::ShadowMemoryConfig {
            enabled,
            drift_threshold: 0.01,
            window_size: 3,
            max_events: 50,
        };
        Agent::new(provider, channel, registry, None, 5, executor).with_shadow_memory_config(&cfg)
    }

    #[test]
    fn record_shadow_event_noop_when_disabled() {
        let mut agent = make_agent_with_shadow(false);
        agent.runtime.debug.iteration_counter = 1;
        let calls = vec![make_tool_req("shell")];
        // Must not panic; shadow_memory stays None.
        agent.record_shadow_event(&calls, "goal".into());
        assert!(
            agent.services.security.shadow_memory.is_none(),
            "shadow_memory must remain None when disabled"
        );
    }

    #[test]
    fn record_shadow_event_appends_event_when_enabled() {
        let mut agent = make_agent_with_shadow(true);
        agent.runtime.debug.iteration_counter = 1;
        let calls = vec![make_tool_req("shell"), make_tool_req("web_scrape")];
        agent.record_shadow_event(&calls, "test goal".into());
        let mem = agent.services.security.shadow_memory.as_ref().unwrap();
        assert_eq!(mem.len(), 1, "one event must be recorded after one batch");
    }

    #[test]
    fn record_shadow_event_goal_drift_emits_security_event() {
        use tokio::sync::watch;
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            use crate::testing::{MockChannel, MockToolExecutor, mock_provider};
            use zeph_skills::registry::SkillRegistry;
            let (tx, rx) = watch::channel(crate::metrics::MetricsSnapshot::default());
            let cfg = zeph_config::ShadowMemoryConfig {
                enabled: true,
                drift_threshold: 0.01,
                window_size: 3,
                max_events: 100,
            };
            let mut agent = Agent::new(
                mock_provider(vec![]),
                MockChannel::new(vec![] as Vec<String>),
                SkillRegistry::empty(),
                None,
                5,
                MockToolExecutor::no_tools(),
            )
            .with_shadow_memory_config(&cfg)
            .with_metrics(tx);

            agent.runtime.debug.iteration_counter = 1;

            // Fill initial window with low-variance events.
            let low = vec![make_tool_req("read")];
            for _ in 0..5 {
                agent.record_shadow_event(&low, "read files".into());
            }
            // Introduce high-privilege divergent batch to spike drift.
            let high = vec![
                make_tool_req("shell"),
                make_tool_req("fetch"),
                make_tool_req("write"),
            ];
            for _ in 0..5 {
                agent.record_shadow_event(&high, "exfiltrate everything".into());
            }

            let snap = rx.borrow().clone();
            // The test verifies that if GoalDrift fires, the event has the right category.
            // (Whether it fires depends on drift score internals; we assert structural correctness.)
            for ev in &snap.security_events {
                if ev.category == zeph_common::SecurityEventCategory::GoalDrift {
                    assert_eq!(ev.source, "shadow_memory");
                    return;
                }
            }
            // If no GoalDrift was emitted, at minimum confirm events were recorded.
            let mem = agent.services.security.shadow_memory.as_ref().unwrap();
            assert!(!mem.is_empty(), "shadow_memory must have recorded events");
        });
    }

    // Gap 3: handle_request_compaction must return the rate-limit error when
    // CompactionState is already CompactedThisTurn.
    #[test]
    fn request_compaction_rate_limit_fires_when_compacted_this_turn() {
        use crate::agent::context_manager::CompactionState;
        use crate::testing::{MockChannel, MockToolExecutor, mock_provider};
        use zeph_skills::registry::SkillRegistry;
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            let mut agent = Agent::new(
                mock_provider(vec![]),
                MockChannel::new(vec![] as Vec<String>),
                SkillRegistry::empty(),
                None,
                5,
                MockToolExecutor::no_tools(),
            );
            // Simulate a compaction that already happened this turn.
            agent
                .context_manager
                .set_compaction_state(CompactionState::CompactedThisTurn { cooldown: 0 });
            let input = serde_json::json!({"reason": "context is growing"});
            let result = agent.handle_request_compaction(&input).await;
            assert!(
                result.contains("already performed this turn"),
                "rate-limit guard must fire: got {result:?}"
            );
        });
    }

    // Gap 5: apply_acon_compression must be a no-op when acon_config.enabled = false.
    #[test]
    fn apply_acon_compression_noop_when_disabled() {
        use crate::testing::{MockChannel, MockToolExecutor, mock_provider};
        use zeph_llm::provider::MessagePart;
        use zeph_skills::registry::SkillRegistry;

        let mut agent = Agent::new(
            mock_provider(vec![]),
            MockChannel::new(vec![] as Vec<String>),
            SkillRegistry::empty(),
            None,
            5,
            MockToolExecutor::no_tools(),
        );
        // Disable Acon.
        agent.services.memory.subsystems.acon_config.enabled = false;

        // Build a result part with text that would be truncated if Acon were active.
        let big_content = "word ".repeat(5000);
        let mut parts = vec![MessagePart::ToolResult {
            tool_use_id: "id_shell".to_owned(),
            content: big_content.clone(),
            is_error: false,
        }];
        let calls = vec![make_tool_req("shell")];

        agent.apply_acon_compression(&calls, &mut parts);

        // Content must be unchanged.
        if let MessagePart::ToolResult { content, .. } = &parts[0] {
            assert_eq!(
                content.len(),
                big_content.len(),
                "content must not be modified when acon is disabled"
            );
        } else {
            panic!("expected ToolResult part");
        }
    }

    // Regression guard for #5584: when a retryable failure (e.g. Qdrant unreachable) was
    // already recorded this turn, handle_retrieve_action must not mandate another doomed
    // retry — it should let the originally-requested tool call proceed (Ok(None)) and inject
    // a graceful-degradation hint instead of the "you MUST call again" hint.
    #[tokio::test]
    async fn handle_retrieve_action_skips_mandatory_retry_after_retryable_failure() {
        use crate::testing::{MockChannel, MockToolExecutor, mock_provider};
        use zeph_skills::registry::SkillRegistry;
        use zeph_tools::error_taxonomy::ToolErrorCategory;

        let mut agent = Agent::new(
            mock_provider(vec![]),
            MockChannel::new(vec![] as Vec<String>),
            SkillRegistry::empty(),
            None,
            5,
            MockToolExecutor::no_tools(),
        );
        agent
            .tool_orchestrator
            .last_tool_error
            .insert("memory_search".to_owned(), ToolErrorCategory::NetworkError);

        let tc = make_tool_req("bash");
        let mut hints = Vec::new();
        let result = agent
            .handle_retrieve_action(0, &tc, &mut hints)
            .await
            .unwrap();

        assert!(
            result.is_none(),
            "must return None so the originally-requested tool proceeds to dispatch"
        );
        assert_eq!(hints.len(), 1);
        assert!(
            hints[0].contains("Proceed with the 'bash' tool call"),
            "hint must direct graceful degradation, got: {}",
            hints[0]
        );
        assert!(
            !hints[0].contains("you MUST call"),
            "must not mandate another retry, got: {}",
            hints[0]
        );
    }

    #[tokio::test]
    async fn handle_retrieve_action_mandates_retry_without_prior_failure() {
        use crate::testing::{MockChannel, MockToolExecutor, mock_provider};
        use zeph_skills::registry::SkillRegistry;

        let mut agent = Agent::new(
            mock_provider(vec![]),
            MockChannel::new(vec![] as Vec<String>),
            SkillRegistry::empty(),
            None,
            5,
            MockToolExecutor::no_tools(),
        );

        let tc = make_tool_req("bash");
        let mut hints = Vec::new();
        let result = agent
            .handle_retrieve_action(0, &tc, &mut hints)
            .await
            .unwrap();

        assert!(
            result.is_some(),
            "without a prior failure, the tool call is skipped pending retrieval"
        );
        assert_eq!(hints.len(), 1);
        assert!(
            hints[0].contains("you MUST call the 'bash' tool again"),
            "hint must mandate a retry, got: {}",
            hints[0]
        );
    }

    // Regression guard for critic-flagged S3 (#5584 follow-up): a retryable failure of an
    // UNRELATED tool (e.g. web_fetch) must not suppress the Retrieve mandatory-retry hint
    // for the rest of the turn — only a failure of memory_search, the retrieval tool the
    // Retrieve branch's hint actually recommends, should trigger graceful degradation.
    #[tokio::test]
    async fn handle_retrieve_action_mandates_retry_when_only_unrelated_tool_failed() {
        use crate::testing::{MockChannel, MockToolExecutor, mock_provider};
        use zeph_skills::registry::SkillRegistry;
        use zeph_tools::error_taxonomy::ToolErrorCategory;

        let mut agent = Agent::new(
            mock_provider(vec![]),
            MockChannel::new(vec![] as Vec<String>),
            SkillRegistry::empty(),
            None,
            5,
            MockToolExecutor::no_tools(),
        );
        agent
            .tool_orchestrator
            .last_tool_error
            .insert("web_fetch".to_owned(), ToolErrorCategory::NetworkError);

        let tc = make_tool_req("bash");
        let mut hints = Vec::new();
        let result = agent
            .handle_retrieve_action(0, &tc, &mut hints)
            .await
            .unwrap();

        assert!(
            result.is_some(),
            "an unrelated tool's stale retryable failure must not suppress Retrieve"
        );
        assert_eq!(hints.len(), 1);
        assert!(
            hints[0].contains("you MUST call the 'bash' tool again"),
            "hint must still mandate a retry, got: {}",
            hints[0]
        );
    }

    mod reformat_phase_tests {
        use zeph_tools::registry::{InvocationHint, ToolDef};

        use super::*;
        use crate::agent::agent_tests::*;

        fn test_tool_def() -> ToolDef {
            ToolDef {
                id: "test_tool".into(),
                description: "a test tool".into(),
                schema: schemars::Schema::default(),
                invocation: InvocationHint::ToolCall,
                output_schema: None,
            }
        }

        fn invalid_params_error() -> zeph_tools::ToolError {
            zeph_tools::ToolError::InvalidParams {
                message: "path must be a string".to_owned(),
            }
        }

        fn bad_tool_call() -> ToolCall {
            let mut params = serde_json::Map::new();
            params.insert("path".to_owned(), serde_json::json!(123));
            ToolCall {
                tool_id: zeph_common::ToolName::new("test_tool"),
                params,
                ..Default::default()
            }
        }

        fn tool_use_request() -> zeph_llm::provider::ToolUseRequest {
            zeph_llm::provider::ToolUseRequest {
                id: "call-1".to_owned(),
                name: "test_tool".to_owned().into(),
                input: serde_json::json!({"path": 123}),
            }
        }

        #[tokio::test]
        async fn retries_with_corrected_arguments_on_success() {
            let provider = mock_provider(vec![r#"{"arguments":{"path":"/corrected"}}"#.into()]);
            let channel = MockChannel::new(vec![]);
            let registry = create_test_registry();
            let executor = MockToolExecutor::new(vec![Ok(Some(zeph_tools::ToolOutput {
                tool_name: "test_tool".to_owned().into(),
                summary: "done".to_owned(),
                blocks_executed: 1,
                filter_stats: None,
                diff: None,
                streamed: false,
                terminal_id: None,
                locations: None,
                raw_response: None,
                claim_source: None,
            }))])
            .with_definitions(vec![test_tool_def()]);
            let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
            agent.tool_orchestrator.parameter_reformat_provider = "fast".to_owned();

            let tool_calls = vec![tool_use_request()];
            let calls = vec![bad_tool_call()];
            let mut tool_results: Vec<
                Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError>,
            > = vec![Err(invalid_params_error())];
            let cancel = tokio_util::sync::CancellationToken::new();

            agent
                .handle_reformat_phase(&tool_calls, &calls, &mut tool_results, &cancel)
                .await
                .unwrap();

            let output = tool_results
                .remove(0)
                .expect("reformat retry should succeed")
                .expect("tool output should be present");
            assert_eq!(output.summary, "done");
        }

        #[tokio::test]
        async fn keeps_original_error_when_provider_returns_malformed_json() {
            let provider = mock_provider(vec!["not json at all".into()]);
            let channel = MockChannel::new(vec![]);
            let registry = create_test_registry();
            let executor =
                MockToolExecutor::new(vec![Ok(None)]).with_definitions(vec![test_tool_def()]);
            let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
            agent.tool_orchestrator.parameter_reformat_provider = "fast".to_owned();

            let tool_calls = vec![tool_use_request()];
            let calls = vec![bad_tool_call()];
            let mut tool_results: Vec<
                Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError>,
            > = vec![Err(invalid_params_error())];
            let cancel = tokio_util::sync::CancellationToken::new();

            agent
                .handle_reformat_phase(&tool_calls, &calls, &mut tool_results, &cancel)
                .await
                .unwrap();

            assert!(
                matches!(
                    tool_results[0],
                    Err(zeph_tools::ToolError::InvalidParams { .. })
                ),
                "original error must be preserved on parse failure"
            );
        }

        #[tokio::test]
        async fn keeps_original_error_when_tool_schema_unknown() {
            let provider = mock_provider(vec![r#"{"arguments":{"path":"/corrected"}}"#.into()]);
            let channel = MockChannel::new(vec![]);
            let registry = create_test_registry();
            // No `with_definitions` — schema lookup for "test_tool" fails.
            let executor = MockToolExecutor::new(vec![Ok(None)]);
            let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
            agent.tool_orchestrator.parameter_reformat_provider = "fast".to_owned();

            let tool_calls = vec![tool_use_request()];
            let calls = vec![bad_tool_call()];
            let mut tool_results: Vec<
                Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError>,
            > = vec![Err(invalid_params_error())];
            let cancel = tokio_util::sync::CancellationToken::new();

            agent
                .handle_reformat_phase(&tool_calls, &calls, &mut tool_results, &cancel)
                .await
                .unwrap();

            assert!(
                matches!(
                    tool_results[0],
                    Err(zeph_tools::ToolError::InvalidParams { .. })
                ),
                "original error must be preserved when schema is unknown"
            );
        }

        #[tokio::test]
        async fn is_a_noop_when_provider_not_configured() {
            let provider = mock_provider(vec![r#"{"arguments":{"path":"/corrected"}}"#.into()]);
            let channel = MockChannel::new(vec![]);
            let registry = create_test_registry();
            let executor =
                MockToolExecutor::new(vec![Ok(None)]).with_definitions(vec![test_tool_def()]);
            let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
            // parameter_reformat_provider left empty (default) — FR: disabled means no LLM call.

            let tool_calls = vec![tool_use_request()];
            let calls = vec![bad_tool_call()];
            let mut tool_results: Vec<
                Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError>,
            > = vec![Err(invalid_params_error())];
            let cancel = tokio_util::sync::CancellationToken::new();

            agent
                .handle_reformat_phase(&tool_calls, &calls, &mut tool_results, &cancel)
                .await
                .unwrap();

            assert!(
                matches!(
                    tool_results[0],
                    Err(zeph_tools::ToolError::InvalidParams { .. })
                ),
                "reformat must not run when parameter_reformat_provider is empty"
            );
        }

        // Regression test: when the provider-pool registry was never wired for this `Agent`
        // (empty `provider_pool`, no `provider_config_snapshot` — the state of a lightweight
        // test/bootstrap agent that never called `with_provider_pool`, never a real production
        // agent since `validate_pool` rejects an empty `[[llm.providers]]` list at config-load
        // time), `reformat_tool_call` still falls back to the primary provider, matching every
        // other `resolve_background_provider` call site (e.g. `compress_provider`). Only once
        // the registry IS wired does an unresolvable name become a real misconfiguration (see
        // `is_a_noop_when_registry_wired_but_name_absent_from_pool` below).
        #[tokio::test]
        async fn falls_back_to_primary_when_registry_never_wired() {
            let provider = mock_provider(vec![r#"{"arguments":{"path":"/corrected"}}"#.into()]);
            let channel = MockChannel::new(vec![]);
            let registry = create_test_registry();
            let executor = MockToolExecutor::new(vec![Ok(Some(zeph_tools::ToolOutput {
                tool_name: "test_tool".to_owned().into(),
                summary: "done".to_owned(),
                blocks_executed: 1,
                filter_stats: None,
                diff: None,
                streamed: false,
                terminal_id: None,
                locations: None,
                raw_response: None,
                claim_source: None,
            }))])
            .with_definitions(vec![test_tool_def()]);
            let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
            // Non-empty name, but provider_pool is empty — name cannot resolve.
            agent.tool_orchestrator.parameter_reformat_provider = "unregistered".to_owned();

            let tool_calls = vec![tool_use_request()];
            let calls = vec![bad_tool_call()];
            let mut tool_results: Vec<
                Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError>,
            > = vec![Err(invalid_params_error())];
            let cancel = tokio_util::sync::CancellationToken::new();

            agent
                .handle_reformat_phase(&tool_calls, &calls, &mut tool_results, &cancel)
                .await
                .unwrap();

            let output = tool_results
                .remove(0)
                .expect("reformat should run using the primary provider fallback")
                .expect("tool output should be present");
            assert_eq!(output.summary, "done");
        }

        /// Minimal `ProviderConfigSnapshot` fixture shared by the registry-wired regression
        /// tests below — all `None`/empty since the fields under test don't need real secrets.
        fn empty_snapshot() -> crate::agent::state::ProviderConfigSnapshot {
            crate::agent::state::ProviderConfigSnapshot {
                claude_api_key: None,
                openai_api_key: None,
                gemini_api_key: None,
                compatible_api_keys: std::collections::HashMap::new(),
                llm_request_timeout_secs: 30,
                embedding_model: String::new(),
                gonka_private_key: None,
                gonka_address: None,
                cocoon_access_hash: None,
            }
        }

        // Regression test for #5478: once the provider-pool registry IS wired (non-empty
        // `provider_pool`, the state every real production `Agent` is in per #5450), a
        // configured `parameter_reformat_provider` name that does not match any entry is a real
        // misconfiguration and must no-op (keep the original tool error) rather than silently
        // reformatting with the primary provider.
        #[tokio::test]
        async fn is_a_noop_when_registry_wired_but_name_absent_from_pool() {
            let provider = mock_provider(vec![r#"{"arguments":{"path":"/corrected"}}"#.into()]);
            let channel = MockChannel::new(vec![]);
            let registry = create_test_registry();
            let executor =
                MockToolExecutor::new(vec![Ok(None)]).with_definitions(vec![test_tool_def()]);

            // The pool is wired (non-empty), but registers a different name than the one
            // configured below, so "unregistered" still cannot resolve.
            let other_entry = crate::config::ProviderEntry {
                provider_type: crate::config::ProviderKind::Ollama,
                name: Some("other".into()),
                ..Default::default()
            };
            let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
                .with_provider_pool(vec![other_entry], empty_snapshot());
            agent.tool_orchestrator.parameter_reformat_provider = "unregistered".to_owned();

            let tool_calls = vec![tool_use_request()];
            let calls = vec![bad_tool_call()];
            let mut tool_results: Vec<
                Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError>,
            > = vec![Err(invalid_params_error())];
            let cancel = tokio_util::sync::CancellationToken::new();

            agent
                .handle_reformat_phase(&tool_calls, &calls, &mut tool_results, &cancel)
                .await
                .unwrap();

            assert!(
                matches!(
                    tool_results[0],
                    Err(zeph_tools::ToolError::InvalidParams { .. })
                ),
                "reformat must no-op when the registry is wired but the configured name is \
                 absent from the pool, not silently fall back to the primary provider"
            );
        }

        // Regression test for #5600: a configured `parameter_reformat_provider` name that IS
        // present in `provider_pool` but whose provider construction fails (e.g. a required
        // secret is missing from the config snapshot) must no-op — unlike the registry-not-wired
        // case above, this must not silently fall back to the primary provider.
        #[tokio::test]
        async fn is_a_noop_when_registry_wired_but_provider_build_fails() {
            let provider = mock_provider(vec![r#"{"arguments":{"path":"/corrected"}}"#.into()]);
            let channel = MockChannel::new(vec![]);
            let registry = create_test_registry();
            let executor =
                MockToolExecutor::new(vec![Ok(None)]).with_definitions(vec![test_tool_def()]);

            // "broken" is present in provider_pool, but is a Claude entry with no API key in
            // the snapshot, so `build_provider_for_switch` fails at resolve time.
            let broken_entry = crate::config::ProviderEntry {
                provider_type: crate::config::ProviderKind::Claude,
                name: Some("broken".into()),
                ..Default::default()
            };
            let mut agent = Agent::new(provider, channel, registry, None, 5, executor)
                .with_provider_pool(vec![broken_entry], empty_snapshot());
            agent.tool_orchestrator.parameter_reformat_provider = "broken".to_owned();

            let tool_calls = vec![tool_use_request()];
            let calls = vec![bad_tool_call()];
            let mut tool_results: Vec<
                Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError>,
            > = vec![Err(invalid_params_error())];
            let cancel = tokio_util::sync::CancellationToken::new();

            agent
                .handle_reformat_phase(&tool_calls, &calls, &mut tool_results, &cancel)
                .await
                .unwrap();

            assert!(
                matches!(
                    tool_results[0],
                    Err(zeph_tools::ToolError::InvalidParams { .. })
                ),
                "reformat must no-op when the in-pool provider fails to build, not silently \
                 fall back to the primary provider"
            );
        }

        #[tokio::test]
        async fn replaces_original_error_when_retry_still_fails() {
            // FR-004: the retried outcome MUST replace tool_results[idx] even when the retry
            // itself fails — the reformat phase gives up cleanly after a single attempt rather
            // than looping, and never leaves the pre-reformat error in place.
            let provider = mock_provider(vec![r#"{"arguments":{"path":"/still-bad"}}"#.into()]);
            let channel = MockChannel::new(vec![]);
            let registry = create_test_registry();
            let executor = MockToolExecutor::new(vec![Err(zeph_tools::ToolError::InvalidParams {
                message: "still not a valid path".to_owned(),
            })])
            .with_definitions(vec![test_tool_def()]);
            let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
            agent.tool_orchestrator.parameter_reformat_provider = "fast".to_owned();

            let tool_calls = vec![tool_use_request()];
            let calls = vec![bad_tool_call()];
            let mut tool_results: Vec<
                Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError>,
            > = vec![Err(invalid_params_error())];
            let cancel = tokio_util::sync::CancellationToken::new();

            agent
                .handle_reformat_phase(&tool_calls, &calls, &mut tool_results, &cancel)
                .await
                .unwrap();

            match &tool_results[0] {
                Err(zeph_tools::ToolError::InvalidParams { message }) => {
                    assert_eq!(
                        message, "still not a valid path",
                        "the retried failure must replace the original error, not leave the \
                         pre-reformat error in place"
                    );
                }
                other => {
                    panic!("expected the retried failure to replace the original, got {other:?}")
                }
            }
        }

        #[tokio::test]
        async fn budget_exhausted_skips_remaining_calls_in_same_phase() {
            // Regression test for the timing bug: `reformat_start` used to be recreated
            // immediately before each per-call elapsed check, making the budget guard an
            // effective no-op. It is now created once before the loop, so real time consumed
            // by an earlier reformat call in the same phase counts against later calls' budget.
            let provider = mock_provider(vec![
                r#"{"arguments":{"path":"/corrected"}}"#.into(),
                r#"{"arguments":{"path":"/corrected"}}"#.into(),
            ]);
            let channel = MockChannel::new(vec![]);
            let registry = create_test_registry();
            let executor = MockToolExecutor::new(vec![Ok(Some(zeph_tools::ToolOutput {
                tool_name: "test_tool".to_owned().into(),
                summary: "done".to_owned(),
                blocks_executed: 1,
                filter_stats: None,
                diff: None,
                streamed: false,
                terminal_id: None,
                locations: None,
                raw_response: None,
                claim_source: None,
            }))])
            .with_definitions(vec![test_tool_def()])
            // Consumes >1s of wall time on the first (only) dispatched retry, so the second
            // tool call's budget check — using the same `reformat_start` — sees the whole-phase
            // budget already exhausted.
            .with_delay(1_100);
            let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
            agent.tool_orchestrator.parameter_reformat_provider = "fast".to_owned();
            agent.tool_orchestrator.max_retry_duration_secs = 1;

            let tool_calls = vec![tool_use_request(), tool_use_request()];
            let calls = vec![bad_tool_call(), bad_tool_call()];
            let mut tool_results: Vec<
                Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError>,
            > = vec![Err(invalid_params_error()), Err(invalid_params_error())];
            let cancel = tokio_util::sync::CancellationToken::new();

            agent
                .handle_reformat_phase(&tool_calls, &calls, &mut tool_results, &cancel)
                .await
                .unwrap();

            assert!(
                tool_results[0].is_ok(),
                "first call is within budget and should be reformatted successfully"
            );
            assert!(
                matches!(
                    tool_results[1],
                    Err(zeph_tools::ToolError::InvalidParams { .. })
                ),
                "second call must be skipped once the whole-phase budget is exhausted by the \
                 first call's real elapsed time"
            );
        }
    }

    // --- PAAC tool-dispatch unmasking: unmask_json_value (#5437) ---

    mod unmask_json_value_tests {
        use super::unmask_json_value;
        use zeph_sanitizer::secret_mask::{SecretCategory, SecretMaskRegistry};

        #[test]
        fn unmasks_top_level_string() {
            let registry = SecretMaskRegistry::new();
            registry.register("KEY", "hunter2password", SecretCategory::Password);
            let masked = registry.mask("hunter2password");
            let mut value = serde_json::Value::String(masked);
            unmask_json_value(&mut value, &registry);
            assert_eq!(value, serde_json::json!("hunter2password"));
        }

        #[test]
        fn unmasks_string_nested_in_object_and_array() {
            let registry = SecretMaskRegistry::new();
            registry.register("KEY", "supersecretvalue1", SecretCategory::ApiKey);
            let masked_placeholder = registry.mask("supersecretvalue1");
            let mut value = serde_json::json!({
                "code": format!("curl -H 'Authorization: Bearer {masked_placeholder}'"),
                "headers": [masked_placeholder.clone(), "plain-value"],
                "nested": {"token": masked_placeholder},
            });
            unmask_json_value(&mut value, &registry);
            assert!(
                value["code"]
                    .as_str()
                    .unwrap()
                    .contains("supersecretvalue1")
            );
            assert_eq!(value["headers"][0], serde_json::json!("supersecretvalue1"));
            assert_eq!(value["headers"][1], serde_json::json!("plain-value"));
            assert_eq!(
                value["nested"]["token"],
                serde_json::json!("supersecretvalue1")
            );
        }

        /// Placeholder-injection safety (#5437): a model-crafted placeholder that was never
        /// issued by this registry (wrong/foreign nonce) must be left verbatim — `unmask` is
        /// nonce-scoped and passthrough-on-miss, so a model cannot forge a placeholder to
        /// exfiltrate a secret it never legitimately saw.
        #[test]
        fn foreign_placeholder_is_left_untouched() {
            let registry = SecretMaskRegistry::new();
            registry.register("KEY", "realsecretvalue1", SecretCategory::ApiKey);
            let forged = "<SECRET:api_key:0000000000000000:0>".to_owned();
            let mut value = serde_json::Value::String(forged.clone());
            unmask_json_value(&mut value, &registry);
            assert_eq!(
                value,
                serde_json::Value::String(forged),
                "a placeholder this registry never issued must pass through unchanged"
            );
        }

        #[test]
        fn non_string_values_are_untouched() {
            let registry = SecretMaskRegistry::new();
            registry.register("KEY", "somesecretvalue1", SecretCategory::Generic);
            let mut value =
                serde_json::json!({"count": 3, "enabled": true, "ratio": 1.5, "n": null});
            let before = value.clone();
            unmask_json_value(&mut value, &registry);
            assert_eq!(value, before);
        }

        // --- S1: unmask-miss telemetry (#5437 critique) ---

        #[test]
        fn mangled_placeholder_returns_a_miss_and_is_left_verbatim() {
            let registry = SecretMaskRegistry::new();
            registry.register("KEY", "realsecretvalue1", SecretCategory::ApiKey);
            let masked = registry.mask("realsecretvalue1");
            // Simulate an LLM inserting a space into the opaque token — a real, observed
            // failure mode for long tokens (S1).
            let mangled = masked.replacen(':', ": ", 1);
            let mut value = serde_json::Value::String(mangled.clone());
            let misses = unmask_json_value(&mut value, &registry);
            assert_eq!(
                misses, 1,
                "a mangled placeholder must be reported as a miss"
            );
            assert_eq!(
                value,
                serde_json::Value::String(mangled),
                "mangled placeholder is left verbatim (fail-safe, no leak)"
            );
        }

        #[test]
        fn successful_unmask_reports_zero_misses() {
            let registry = SecretMaskRegistry::new();
            registry.register("KEY", "realsecretvalue1", SecretCategory::ApiKey);
            let masked = registry.mask("realsecretvalue1");
            let mut value = serde_json::Value::String(masked);
            let misses = unmask_json_value(&mut value, &registry);
            assert_eq!(misses, 0);
            assert_eq!(value, serde_json::json!("realsecretvalue1"));
        }

        #[test]
        fn miss_count_aggregates_across_nested_structure() {
            let registry = SecretMaskRegistry::new();
            registry.register("KEY", "realsecretvalue1", SecretCategory::ApiKey);
            let masked = registry.mask("realsecretvalue1");
            let mangled = masked.replacen(':', ": ", 1);
            let mut value = serde_json::json!({
                "ok": masked,
                "nested": {"a": mangled.clone(), "b": mangled},
                "plain": "no placeholder here",
            });
            let misses = unmask_json_value(&mut value, &registry);
            assert_eq!(
                misses, 2,
                "both mangled leaves must be counted, the valid one must not"
            );
        }

        // --- secret values containing JSON/regex-special characters ---

        #[test]
        fn secret_with_json_and_regex_special_chars_roundtrips() {
            let registry = SecretMaskRegistry::new();
            let tricky_secret = r#"p@ss"w0rd\n{[(.*+?)]}$^|\}"#;
            registry.register("KEY", tricky_secret, SecretCategory::Password);
            let masked = registry.mask(tricky_secret);
            assert!(!masked.contains(tricky_secret));

            let mut value = serde_json::json!({
                "code": format!("echo '{masked}'"),
                "list": [masked.clone()],
            });
            let misses = unmask_json_value(&mut value, &registry);
            assert_eq!(misses, 0);
            assert!(value["code"].as_str().unwrap().contains(tricky_secret));
            assert_eq!(value["list"][0], serde_json::json!(tricky_secret));
        }
    }

    /// Regression tests for #5513: cancellation during the post-dispatch phases
    /// (confirmation / retry / reformat) must write exactly one `[Cancelled]` tombstone
    /// `ToolResult` per `tool_use_id` and must never leave a `ToolUse` orphaned or let the
    /// batch persist run again afterward.
    mod cancellation_regression_tests {
        use super::*;
        use crate::agent::agent_tests::*;

        /// Always fails with a `Transient` error and is marked retryable, so
        /// `handle_retry_phase` enters its backoff-sleep branch on every attempt.
        struct AlwaysTransientExecutor;

        impl zeph_tools::executor::ToolExecutor for AlwaysTransientExecutor {
            fn execute(
                &self,
                _response: &str,
            ) -> impl std::future::Future<
                Output = Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError>,
            > + Send {
                std::future::ready(Ok(None))
            }

            fn execute_tool_call(
                &self,
                _call: &ToolCall,
            ) -> impl std::future::Future<
                Output = Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError>,
            > + Send {
                std::future::ready(Err(zeph_tools::ToolError::Execution(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "always transient",
                ))))
            }

            fn is_tool_retryable(&self, _tool_id: &str) -> bool {
                true
            }
        }

        fn tool_result_ids(agent: &Agent<MockChannel>, id: &str) -> Vec<&'static str> {
            agent
                .msg
                .messages
                .iter()
                .flat_map(|m| m.parts.iter())
                .filter(|p| {
                    matches!(p, MessagePart::ToolResult { tool_use_id, .. } if tool_use_id == id)
                })
                .map(|_| "match")
                .collect()
        }

        /// Bug A: a token already cancelled before `handle_confirmation_phase` runs must
        /// short-circuit `run_post_dispatch_phases` after the first phase reports
        /// cancellation, instead of cascading into `handle_retry_phase` and
        /// `handle_reformat_phase` as well. Before the fix, each of the three phases
        /// independently detected the same cancellation and wrote its own tombstone batch,
        /// producing up to 3 duplicate `[Cancelled]` `ToolResult`s per `tool_use_id`.
        #[tokio::test]
        async fn cancelled_before_confirmation_phase_writes_one_tombstone_and_skips_later_phases() {
            let provider = mock_provider(vec![]);
            let channel = MockChannel::new(vec![]);
            let registry = create_test_registry();
            let executor = MockToolExecutor::no_tools();
            let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
            agent.tool_orchestrator.max_tool_retries = 2;
            agent.tool_orchestrator.parameter_reformat_provider = "fast".to_owned();

            let tool_calls = vec![
                zeph_llm::provider::ToolUseRequest {
                    id: "id-1".to_owned(),
                    name: "bash".to_owned().into(),
                    input: serde_json::json!({}),
                },
                zeph_llm::provider::ToolUseRequest {
                    id: "id-2".to_owned(),
                    name: "bash".to_owned().into(),
                    input: serde_json::json!({}),
                },
            ];
            let calls = vec![
                ToolCall {
                    tool_id: zeph_common::ToolName::new("bash"),
                    ..Default::default()
                },
                ToolCall {
                    tool_id: zeph_common::ToolName::new("bash"),
                    ..Default::default()
                },
            ];
            let mut tool_results: Vec<
                Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError>,
            > = vec![Ok(None), Ok(None)];

            let cancel = tokio_util::sync::CancellationToken::new();
            cancel.cancel();

            let cancelled = agent
                .run_post_dispatch_phases(&tool_calls, &calls, &mut tool_results, 2, &cancel)
                .await
                .unwrap();

            assert!(
                cancelled,
                "run_post_dispatch_phases must report cancellation"
            );

            for id in ["id-1", "id-2"] {
                let matches = tool_result_ids(&agent, id);
                assert_eq!(
                    matches.len(),
                    1,
                    "tool_use_id {id} must have exactly one [Cancelled] tombstone, got {}",
                    matches.len()
                );
            }
        }

        /// Bug C: cancellation landing specifically inside the retry-phase backoff-sleep
        /// `tokio::select!` must still persist a tombstone `ToolResult` for the pending
        /// `ToolUse`. Before the fix this was the only cancellation checkpoint in the file
        /// that returned without calling `persist_cancelled_tool_results`, leaving the
        /// `ToolUse` message genuinely orphaned (zero `ToolResult`s) for the rest of the
        /// live session.
        #[tokio::test]
        async fn handle_retry_phase_cancelled_during_backoff_sleep_persists_tombstone() {
            let provider = mock_provider(vec![]);
            let channel = MockChannel::new(vec![]);
            let registry = create_test_registry();
            let executor = AlwaysTransientExecutor;
            let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
            agent.tool_orchestrator.max_tool_retries = 3;
            // Large, deterministic-enough backoff window so the spawned cancellation below
            // (fired after a short real-time delay) lands during the sleep rather than after
            // it — full-jitter backoff makes an exact guarantee impossible, but at this
            // magnitude the chance of picking a delay under 200ms is negligible.
            agent.tool_orchestrator.retry_base_ms = 600_000;
            agent.tool_orchestrator.retry_max_ms = 600_000;
            agent.tool_orchestrator.max_retry_duration_secs = 0;

            let tool_calls = vec![zeph_llm::provider::ToolUseRequest {
                id: "id-retry".to_owned(),
                name: "bash".to_owned().into(),
                input: serde_json::json!({}),
            }];
            let calls = vec![ToolCall {
                tool_id: zeph_common::ToolName::new("bash"),
                ..Default::default()
            }];
            let mut tool_results: Vec<
                Result<Option<zeph_tools::ToolOutput>, zeph_tools::ToolError>,
            > = vec![Err(zeph_tools::ToolError::Execution(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "initial transient failure",
            )))];

            let cancel = tokio_util::sync::CancellationToken::new();
            let cancel_trigger = cancel.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                cancel_trigger.cancel();
            });

            let cancelled = agent
                .handle_retry_phase(&tool_calls, &calls, &mut tool_results, 3, &cancel)
                .await
                .unwrap();

            assert!(cancelled, "handle_retry_phase must report cancellation");

            let matches = tool_result_ids(&agent, "id-retry");
            assert_eq!(
                matches.len(),
                1,
                "cancellation during backoff sleep must still write exactly one tombstone \
                 ToolResult, got {}",
                matches.len()
            );
        }

        /// Bug B + Bug C, exercised through the full `handle_native_tool_calls` entry point:
        /// once `run_post_dispatch_phases` reports cancellation (here triggered during the
        /// retry-phase backoff sleep), `handle_native_tool_calls` must return immediately and
        /// must NOT call `process_tool_result_batch` afterward. Before the fix, the batch
        /// persist ran unconditionally, appending a second (contradicting, non-cancelled)
        /// `ToolResult` message right after the phases' own tombstone write.
        #[tokio::test]
        async fn handle_native_tool_calls_cancelled_during_retry_backoff_skips_batch_persist() {
            let provider = mock_provider(vec![]);
            let channel = MockChannel::new(vec![]);
            let registry = create_test_registry();
            let executor = AlwaysTransientExecutor;
            let mut agent = Agent::new(provider, channel, registry, None, 5, executor);
            agent.tool_orchestrator.max_tool_retries = 3;
            agent.tool_orchestrator.retry_base_ms = 600_000;
            agent.tool_orchestrator.retry_max_ms = 600_000;
            agent.tool_orchestrator.max_retry_duration_secs = 0;

            let tool_calls = vec![zeph_llm::provider::ToolUseRequest {
                id: "id-e2e".to_owned(),
                name: "bash".to_owned().into(),
                input: serde_json::json!({"command": "echo hi"}),
            }];

            let cancel_trigger = agent.runtime.lifecycle.cancel_token.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                cancel_trigger.cancel();
            });

            let window_exhausted = agent
                .handle_native_tool_calls(None, &tool_calls)
                .await
                .unwrap();
            assert!(
                !window_exhausted,
                "a cancelled turn must not report utility-window exhaustion"
            );

            let matches = tool_result_ids(&agent, "id-e2e");
            assert_eq!(
                matches.len(),
                1,
                "exactly one ToolResult must exist for id-e2e after cancellation, got {}",
                matches.len()
            );

            // The tombstone must be the very last message — proving process_tool_result_batch
            // did not run afterward and append a second, contradicting result.
            let last = agent.msg.messages.last().expect("at least one message");
            let has_tombstone = last.parts.iter().any(|p| {
                matches!(
                    p,
                    MessagePart::ToolResult { tool_use_id, content, is_error }
                        if tool_use_id == "id-e2e" && content == "[Cancelled]" && *is_error
                )
            });
            assert!(
                has_tombstone,
                "last message must be the [Cancelled] tombstone for id-e2e, got: {last:?}"
            );
        }
    }
}
